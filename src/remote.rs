// ssh transport for the DAEMON's own traffic (remote CLI execs + API-socket
// forward) over one ControlMaster per host, plus (opt-in) pre-creation of
// per-slot masters that pooled pane streams may attach to (see pane.rs).
// Pane streams never create a master themselves; unpooled or unattached
// streams simply run direct.

use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::api::ApiClient;
use crate::config::{ApiTransport, HostConfig};
use crate::util::{err, Logger, Result};

/// Marker in the error text for "the container isn't running". A stopped
/// devcontainer is its resting state, unlike an unreachable ssh host, so the
/// daemon backs off gently instead of treating it as a fault.
pub const DORMANT: &str = "dormant";

/// first build with terminal session observe/control
const MIN_PREVIEW_BUILD: &str = "2026-06-30";

/// Common ssh options, shared by the daemon's master and every pane stream.
pub const SSH_COMMON_OPTS: [&str; 6] = [
    "-o",
    "BatchMode=yes",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=3",
];

#[derive(Debug)]
pub struct RemoteStatus {
    pub socket: String,
    pub supported: bool,
    pub reason: Option<String>,
}

struct SshOutput {
    code: i32,
    out: String,
    err: String,
}

/// Spawn `cmd` and wait up to `timeout_ms`, draining stdout/stderr
/// concurrently with the wait (the same reason `Command::output` does —
/// otherwise a child that fills its pipe buffer before exiting deadlocks
/// against a parent that isn't reading yet).
///
/// `cmd` is spawned as the leader of its own process group (`process_group(0)`)
/// so a `ProxyCommand` or other ssh descendant can be reaped too: on timeout
/// this kills the whole group, not just the direct child, then explicitly
/// waits for the child before returning rather than relying on `kill_on_drop`
/// alone. `kill_on_drop` only *requests* termination when the `Child` drops,
/// asynchronously, with no guarantee it has actually happened by the time
/// this function's caller proceeds — e.g. to remove and recreate the very
/// control socket a still-alive timed-out `ssh -f -N` might still be racing
/// to bind. `kill_on_drop` stays set too, as a backstop for any path that
/// drops `child` without reaching the explicit kill below (a panic, an early
/// return added later, ...).
///
/// Must not simply wrap the wait in `tokio::time::timeout`: that combinator
/// wins by dropping the losing future, which would drop `child` itself
/// (moved into the wait) and lose the handle needed to kill it explicitly.
/// `tokio::select!` on a plain (non-`move`) async block instead only
/// *borrows* `child` in the racing branch, so it survives in this function's
/// own scope for the timeout arm to act on.
async fn run_with_timeout(mut cmd: Command, timeout_ms: u64, timeout_err: &str) -> SshOutput {
    let mut child = match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return SshOutput {
                code: 1,
                out: String::new(),
                err: e.to_string(),
            }
        }
    };
    // `process_group(0)` makes the child its own group leader, so its pgid
    // equals its pid and this also covers the child itself, not just its
    // descendants.
    let pgid = child.id().map(|id| id as i32);
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();

    let wait_and_drain = async {
        let (_, _, status) = tokio::join!(
            stdout.read_to_end(&mut out_buf),
            stderr.read_to_end(&mut err_buf),
            child.wait()
        );
        status
    };

    tokio::select! {
        status = wait_and_drain => match status {
            Ok(status) => SshOutput {
                code: status.code().unwrap_or(1),
                out: String::from_utf8_lossy(&out_buf).into_owned(),
                err: String::from_utf8_lossy(&err_buf).into_owned(),
            },
            Err(e) => SshOutput { code: 1, out: String::new(), err: e.to_string() },
        },
        _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
            match pgid {
                // ProxyCommand and any other ssh descendants share this group;
                // killing only the direct child would leave those behind.
                Some(pgid) => { unsafe { libc::kill(-pgid, libc::SIGKILL) }; }
                None => { let _ = child.start_kill(); }
            }
            let _ = child.wait().await;
            SshOutput { code: 1, out: String::new(), err: timeout_err.into() }
        }
    }
}

async fn ssh(args: &[String], timeout_ms: u64) -> SshOutput {
    let mut cmd = Command::new("ssh");
    cmd.args(args);
    run_with_timeout(cmd, timeout_ms, "ssh timeout").await
}

fn remove_stale_control_socket(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(err(format!(
                "cannot inspect ssh control socket {}: {e}",
                path.display()
            )))
        }
    };
    if !metadata.file_type().is_socket() {
        return Err(err(format!(
            "refusing to replace ssh control path {} because it is not a socket",
            path.display()
        )));
    }
    fs::remove_file(path).map_err(|e| {
        err(format!(
            "cannot remove stale ssh control socket {}: {e}",
            path.display()
        ))
    })
}

/// Timeouts for the daemon's own `<host>.ctl` control-plane master: this
/// bootstraps the whole host connection, so it can afford to wait.
const CONTROL_MASTER_CHECK_TIMEOUT_MS: u64 = 15000;
const CONTROL_MASTER_START_TIMEOUT_MS: u64 = 20000;

/// Timeouts for a stream-pool slot master. Deliberately much shorter than the
/// control-plane master's: the daemon is the sole creator of these (see
/// pane.rs::ssh_stream_args — streamers attach with `ControlMaster=no` and
/// fall back to a direct connection if a live master isn't already there),
/// so a hung/unreachable attempt must not be allowed to stall a host's whole
/// reconciliation loop anywhere near as long as bootstrapping the connection
/// itself is allowed to.
const STREAM_MASTER_CHECK_TIMEOUT_MS: u64 = 4000;
const STREAM_MASTER_START_TIMEOUT_MS: u64 = 8000;

/// Finite, not `yes`: an idle pool slot (its assigned panes all closed, or a
/// config/target change stopped using it) must eventually let its master
/// exit on its own, rather than every slot ever assigned accumulating into a
/// permanent background ssh process nothing still uses.
const STREAM_MASTER_CONTROL_PERSIST: &str = "10m";

async fn master_check(ctl_path: &Path, target: &str, timeout_ms: u64) -> bool {
    let check = vec![
        "-S".into(),
        ctl_path.display().to_string(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-O".into(),
        "check".into(),
        target.to_string(),
    ];
    ssh(&check, timeout_ms).await.code == 0
}

/// Start an ssh ControlMaster listening at `ctl_path` for `target` and verify
/// it actually came up, used both for the daemon's own `<host>.ctl` master
/// (`RemoteHost::ensure_master`) and for a stream-pool slot master
/// (`ensure_stream_master`) — same check/stale-socket/start/verify sequence
/// either way, just parameterized on the control path, how long the master
/// persists with no client attached, and the timeouts to apply.
async fn start_master(
    ctl_path: &Path,
    target: &str,
    control_persist: &str,
    check_timeout_ms: u64,
    start_timeout_ms: u64,
) -> Result<()> {
    // OpenSSH falls back to a standalone connection when ControlPath exists
    // but no master is listening. With -f -N that silently leaks one process
    // per retry, while every later -O command keeps targeting the dead socket.
    remove_stale_control_socket(ctl_path)?;
    let mut start: Vec<String> = vec!["-M".into(), "-S".into(), ctl_path.display().to_string()];
    start.extend(SSH_COMMON_OPTS.iter().map(|s| s.to_string()));
    start.extend([
        "-o".into(),
        format!("ControlPersist={control_persist}"),
        "-f".into(),
        "-N".into(),
        target.to_string(),
    ]);
    let res = ssh(&start, start_timeout_ms).await;
    if res.code != 0 {
        return Err(err(format!(
            "ssh master to {target} failed: {}",
            nonempty(&res.err, res.code)
        )));
    }
    if !master_check(ctl_path, target, check_timeout_ms).await {
        return Err(err(format!(
            "ssh master to {target} did not create a usable control socket"
        )));
    }
    Ok(())
}

/// A dedicated sibling file whose flock (never its contents, which are never
/// read) is the cross-process lock for one control-master path — not the
/// control socket itself, so nothing about opening/holding this lock has to
/// reason about socket bind/connect semantics.
fn cross_process_lock_path(ctl_path: &Path) -> PathBuf {
    let mut name = ctl_path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Whether `ensure_master_locked` found a master already listening, or had
/// to (re)start one — `RemoteHost::ensure_master` needs to know which, to
/// decide whether its cached API-socket forward is still valid.
enum MasterState {
    AlreadyAlive,
    Started,
}

/// Bring up (or confirm) the ControlMaster at `ctl_path`, serialized across
/// every process on this host via `filelock::acquire_async` so a concurrent
/// caller — another tokio task in this same daemon (converge's fresh-pane
/// pre-create vs. the local-server zombie-heal sweep), or a wholly separate
/// `herdr-mirror` invocation (a manual `once` racing the daemon) — can't
/// unlink a master a concurrent attempt just brought up. The shared layer
/// behind both `RemoteHost::ensure_master` (control-plane `<host>.ctl`) and
/// `ensure_stream_master` (pool slots) — same
/// check/lock/re-check/start/verify sequence either way, just parameterized
/// on the control path, how long the master persists with no client
/// attached, and the timeouts to apply.
///
/// Lock order: this lock is always the INNERMOST one held — see
/// `filelock`'s module doc for the required ordering against the per-host
/// state lock (`state.rs`) and `daemon.lock`.
async fn ensure_master_locked(
    ctl_path: &Path,
    target: &str,
    control_persist: &str,
    check_timeout_ms: u64,
    start_timeout_ms: u64,
) -> Result<MasterState> {
    // Fast path without the lock: the common case (already alive) shouldn't
    // pay for lock acquisition at all.
    if master_check(ctl_path, target, check_timeout_ms).await {
        return Ok(MasterState::AlreadyAlive);
    }
    let lock_path = cross_process_lock_path(ctl_path);
    let _guard =
        crate::filelock::acquire_async(&lock_path, Duration::from_millis(check_timeout_ms)).await?;
    // Re-check now that this path is ours alone across every process on this
    // host: a concurrent attempt may have brought up a live master while we
    // waited for the lock, and unlinking-and-restarting on top of it would
    // tear down exactly the master that attempt just created.
    if master_check(ctl_path, target, check_timeout_ms).await {
        return Ok(MasterState::AlreadyAlive);
    }
    start_master(
        ctl_path,
        target,
        control_persist,
        check_timeout_ms,
        start_timeout_ms,
    )
    .await?;
    Ok(MasterState::Started)
}

/// Pre-create the ControlMaster for one ssh stream-pool slot, so the
/// streamers that will share it (attaching with `ControlMaster=no`, see
/// pane.rs::ssh_stream_args) find an already-listening master instead of
/// each falling back to its own direct connection. The daemon is the sole
/// creator of stream-pool masters — a streamer never starts one itself —
/// so this call is what makes pooling actually happen; a streamer whose
/// slot has no live master here simply runs direct until it does.
pub(crate) async fn ensure_stream_master(
    state_dir: &Path,
    cfg: &HostConfig,
    slot: u32,
) -> Result<()> {
    let path = stream_control_path(state_dir, &cfg.name, &cfg.target, slot);
    ensure_master_locked(
        &path,
        &cfg.target,
        STREAM_MASTER_CONTROL_PERSIST,
        STREAM_MASTER_CHECK_TIMEOUT_MS,
        STREAM_MASTER_START_TIMEOUT_MS,
    )
    .await?;
    Ok(())
}

/// Ensure every slot in `slots` has a master, concurrently rather than one at
/// a time: sequential attempts turn one hung/unreachable slot into a delay
/// that multiplies by the number of slots, which is exactly what previously
/// let a broken host stall reconciliation for minutes (see the `stream_pool`
/// module docs). Bounded by `slots`' own size — realistically a handful, one
/// per `ssh_streams_per_connection`-sized group of mirrored panes — not an
/// unbounded or ever-growing set.
///
/// Returns one result per slot so the caller can log and/or drive its own
/// retry backoff; a task that fails to complete at all (panics or is
/// cancelled) is logged here directly, since it has no slot number to
/// attribute a returned result to.
pub(crate) async fn ensure_stream_masters_concurrent(
    state_dir: &Path,
    cfg: &HostConfig,
    slots: impl IntoIterator<Item = u32>,
    log: &Logger,
) -> Vec<(u32, Result<()>)> {
    let mut set = tokio::task::JoinSet::new();
    for slot in slots {
        let state_dir = state_dir.to_path_buf();
        let cfg = cfg.clone();
        set.spawn(async move {
            let res = ensure_stream_master(&state_dir, &cfg, slot).await;
            (slot, res)
        });
    }
    let mut results = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(pair) => results.push(pair),
            Err(e) => log.log(&format!(
                "[{}] a stream-pool master pre-create task did not complete: {e}",
                cfg.name
            )),
        }
    }
    results
}

pub struct RemoteHost {
    pub cfg: HostConfig,
    ctl_path: PathBuf,
    pub fwd_sock: PathBuf,
    forwarded: bool,
    /// docker hosts only: resolved container + chosen stdio bridge
    container: Option<crate::docker::Container>,
    /// docker hosts only: owns the relay listener. Dropping it stops serving
    /// and unlinks the socket, so a reconnect never inherits a dead one.
    relay: Option<crate::docker::RelayHandle>,
    /// ssh hosts only: owns the exec-relay listener when the exec transport
    /// is in use. Same lifecycle reasoning as `relay` above, one level up
    /// the transport stack (ssh exec instead of `docker exec`).
    exec_relay: Option<crate::ssh_relay::RelayHandle>,
    /// ssh hosts only: where the exec relay listens. Deliberately NOT
    /// `fwd_sock`: sharing one path with the streamlocal forward makes the
    /// relay's "is a healthy relay already serving this?" check answerable by
    /// a live `-L` forward, which silently ignores `api_transport = "exec"`
    /// and leaves the daemon believing it is on a relay it never started.
    exec_sock: PathBuf,
    /// ssh hosts only: which transport to try first. Seeded from `cfg` at
    /// construction; `hint_transport` lets the daemon override it with what
    /// last worked, since a fresh `RemoteHost` is built on every reconnect
    /// and would otherwise re-probe streamlocal every time even after it is
    /// known to be dead for this host.
    transport_hint: ApiTransport,
    /// ssh hosts only: which transport this connection actually used, so the
    /// daemon can feed it back into the next `RemoteHost`'s `hint_transport`.
    pub last_api_transport: Option<ApiTransport>,
    log: Logger,
}

/// Base directory + filename stem for a host's control-plane sockets
/// (`.ctl`, `-api.sock`, `-api-exec.sock`), the stem bounded so the longest
/// one still fits sockaddr_un.
///
/// Two disjoint cases, corresponding to `util::is_safe_path_component`:
///
/// - A safe host name (the common case — nothing is reserved, an `h#`-
///   prefixed one included) lives at the root of `state_dir` and behaves
///   exactly as it did before this crate ever hashed anything: verbatim if
///   it fits the budget, else a truncated readable prefix plus
///   `legacy_socket_hash` — the original, unprefixed 32-bit hash, preserved
///   byte-for-byte. An existing install with an overlong-but-safe host name
///   must not get a new `.ctl` path (and so a new ControlMaster + API
///   forward) on an otherwise-unrelated upgrade; only a host name that was
///   never safely representable at all moves.
/// - An unsafe host name (never valid pre-upgrade, so there is nothing to
///   stay compatible with) lives under `util::unsafe_host_artifacts_dir`
///   instead, with its fixed-length 64-bit hash stem — no truncation is
///   needed there at all, and directory separation (not a reserved
///   filename prefix) is what keeps it from ever aliasing a root-level safe
///   stem, however that stem looks.
fn control_socket_base(state_dir: &std::path::Path, host_name: &str) -> (PathBuf, String) {
    if !crate::util::is_safe_path_component(host_name) {
        let dir = crate::util::unsafe_host_artifacts_dir(state_dir);
        let _ = crate::util::ensure_private_dir(&dir);
        return (dir, crate::util::unsafe_host_stem(host_name));
    }
    let budget = socket_path_budget(state_dir);
    let stem = if host_name.len() <= budget {
        host_name.to_string()
    } else {
        legacy_degrade_with_hash(host_name, host_name, budget)
    };
    (state_dir.to_path_buf(), stem)
}

/// sockaddr_un byte budget for a root-level (safe host name) control-plane
/// socket stem. Not needed for an unsafe name's `.h/`-nested stem (fixed
/// length regardless of host name) or for any stream-pool socket (see
/// `streams_dir`/`stream_stem_base`, similarly fixed-length) — those are
/// instead checked directly against their REAL constructed path by
/// `validate_socket_path_budget`, since neither ever truncates to fit.
fn socket_path_budget(state_dir: &std::path::Path) -> usize {
    use std::os::unix::ffi::OsStrExt;

    // macOS reserves one byte of sockaddr_un's 104-byte path for NUL; Linux
    // allows 108, and we deliberately apply the tighter bound on both so a
    // hosts.toml is portable. Overhead is the worst case across the three
    // suffixes (`-api-exec.sock`, 14) plus OpenSSH's mux temp suffix on the
    // ControlPath (a dot and 11 chars); 21 keeps a little slack.
    const CONTROL_SOCKET_OVERHEAD: usize = 21;

    let directory_bytes = state_dir.as_os_str().as_bytes().len() + 1;
    MAX_SOCKET_PATH_BYTES.saturating_sub(directory_bytes + CONTROL_SOCKET_OVERHEAD)
}

/// The portable sockaddr_un byte budget every actual Unix socket path this
/// crate creates must fit: macOS reserves one byte of its 104-byte
/// sockaddr_un for NUL, Linux allows 108, and using the tighter bound on
/// both keeps a hosts.toml portable between them.
pub(crate) const MAX_SOCKET_PATH_BYTES: usize = 103;

/// OpenSSH creates a temporary file alongside the real ControlPath during
/// mux setup, named with this many extra bytes appended — a live socket
/// that itself fits `MAX_SOCKET_PATH_BYTES` can still fail to bind if this
/// leaves no room for that temp file. Applies to a ControlPath (the
/// control-plane `.ctl` and every stream-pool socket) but not the plain
/// API-forward/exec sockets, which OpenSSH never touches.
pub(crate) const CONTROL_MUX_TEMP_SUFFIX_BYTES: usize = 17;

/// The socket families this host's `HostKind`/`api_transport` can actually
/// reach at runtime (see `RemoteHost::new`/`connect_ssh_api`/`connect_api`),
/// each as (label, real path, extra bytes OpenSSH's mux temp file needs
/// beyond the live socket itself — 0 for a socket ssh never puts under
/// mux). A pure, directly-testable enumeration so "does this host's
/// config actually reach this socket" is checked against the real enums,
/// not duplicated/guessed inline:
/// - docker: only its relay socket, which reuses the plain `-api.sock`
///   path (no ControlMaster, no mux suffix, no exec relay — docker never
///   opens either).
/// - ssh: always the ControlMaster (`.ctl`, plus the mux temp suffix); the
///   API-forward socket unless `api_transport = "exec"` pins the exec
///   relay exclusively, and the API-exec socket unless `api_transport =
///   "socket"` pins the forward exclusively (`"auto"`, the default, can
///   reach both).
/// - ssh with pooling on (`ssh_streams_per_connection > 1`): also one
///   representative stream-pool slot (also a ControlMaster, so also plus
///   the mux suffix). `util::stream_namespace_hash` folds only host+target
///   into a fixed-length hash, and slot is a separate fixed-width suffix
///   (see `stream_control_path`), so every slot's path is the identical
///   length regardless of which one this is.
fn reachable_control_sockets(
    state_dir: &std::path::Path,
    cfg: &HostConfig,
) -> Vec<(&'static str, PathBuf, usize)> {
    let (dir, stem) = control_socket_base(state_dir, &cfg.name);
    if cfg.kind.is_docker() {
        return vec![(
            "Docker relay socket",
            dir.join(format!("{stem}-api.sock")),
            0,
        )];
    }
    let mut sockets = vec![(
        "control-plane socket (.ctl)",
        dir.join(format!("{stem}.ctl")),
        CONTROL_MUX_TEMP_SUFFIX_BYTES,
    )];
    if cfg.api_transport != ApiTransport::Exec {
        sockets.push((
            "API forward socket",
            dir.join(format!("{stem}-api.sock")),
            0,
        ));
    }
    if cfg.api_transport != ApiTransport::Socket {
        sockets.push((
            "API exec socket",
            dir.join(format!("{stem}-api-exec.sock")),
            0,
        ));
    }
    if cfg.ssh_streams_per_connection > 1 {
        sockets.push((
            "stream-pool socket",
            stream_control_path(state_dir, &cfg.name, &cfg.target, 0),
            CONTROL_MUX_TEMP_SUFFIX_BYTES,
        ));
    }
    sockets
}

/// Validate every actual Unix socket path this host's connections will use
/// (see `reachable_control_sockets`) against the portable budget above —
/// the same limit `control_socket_base`'s root-level truncation already
/// respects for a SAFE name, but not automatically enforced for an unsafe
/// name's fixed-length `.h/`-nested stem or any `.s/`-nested stream-pool
/// socket, since neither one ever truncates to fit. A `state_dir` deep
/// enough to blow this budget would otherwise fail only at actual
/// ssh/docker bind time — silently, and far from its actionable cause
/// (`state_dir` itself, or this one host's name).
pub(crate) fn validate_socket_path_budget(
    state_dir: &std::path::Path,
    cfg: &HostConfig,
) -> Result<()> {
    for (label, path, mux_suffix) in reachable_control_sockets(state_dir, cfg) {
        check_socket_path_budget(&cfg.name, label, &path, mux_suffix)?;
    }
    Ok(())
}

fn check_socket_path_budget(
    host_name: &str,
    label: &str,
    path: &std::path::Path,
    mux_suffix: usize,
) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let len = path.as_os_str().as_bytes().len() + mux_suffix;
    if len > MAX_SOCKET_PATH_BYTES {
        return Err(err(format!(
            "hosts.toml: \"{host_name}\"'s {label} path is {len} bytes ({}{}), over the \
             {MAX_SOCKET_PATH_BYTES}-byte portable Unix socket limit — use a shorter state_dir \
             or host name",
            path.display(),
            if mux_suffix > 0 {
                format!(" plus a {mux_suffix}-byte OpenSSH mux suffix")
            } else {
                String::new()
            }
        )));
    }
    Ok(())
}

/// The pre-widening 32-bit FNV-1a hash, preserved byte-for-byte: this is
/// what `control_socket_base` uses for a safe-but-overlong host name
/// specifically so that path does not move on this upgrade. Do not change
/// this function — change `util::short_hash` (the 64-bit hash every other
/// path in this crate uses) instead.
pub(crate) fn legacy_socket_hash(s: &str) -> String {
    use std::hash::Hasher;

    let mut hasher = fnv::FnvHasher::default();
    hasher.write(s.as_bytes());
    format!("{:08x}", hasher.finish() as u32)
}

/// Degrade `readable` to fit `budget` bytes: keep a prefix of it plus
/// `legacy_socket_hash` of `hash_input`, so two names that overflow the
/// budget identically (the exact case that motivates truncating at all)
/// still diverge. Below that even the hash can't fit, which means the state
/// dir path itself is too long for any socket — return the hash alone so
/// hosts stay distinct and let the bind fail loudly, rather than handing
/// every host one shared path.
///
/// Only ever called from `control_socket_base`'s safe-host-name branch —
/// `readable` is therefore always safe to use verbatim as a path component
/// here.
fn legacy_degrade_with_hash(readable: &str, hash_input: &str, budget: usize) -> String {
    let hash = legacy_socket_hash(hash_input);
    let keep = budget.saturating_sub(hash.len() + 1);
    let mut end = keep.min(readable.len());
    while !readable.is_char_boundary(end) {
        end -= 1;
    }
    let prefix = readable[..end].trim_end_matches('-');
    if prefix.is_empty() {
        hash
    } else {
        format!("{prefix}-{hash}")
    }
}

pub(crate) fn control_path(state_dir: &std::path::Path, host_name: &str) -> PathBuf {
    let (dir, stem) = control_socket_base(state_dir, host_name);
    dir.join(format!("{stem}.ctl"))
}

/// Private subdirectory for every stream-pool socket (`stream_control_path`):
/// entirely new sockets with no pre-pooling path on disk to stay compatible
/// with, kept structurally apart from the root-level control-plane sockets
/// right next to it (and from `util::unsafe_host_artifacts_dir`) rather than
/// distinguished by a reserved filename prefix — the same reasoning as
/// `unsafe_host_artifacts_dir`, and what actually stops a stream socket from
/// aliasing a different host's control-plane socket (both would otherwise
/// end in `.ctl` at the same root).
/// Private subdirectory for every stream-pool socket (`stream_control_path`):
/// entirely new sockets with no pre-pooling path on disk to stay compatible
/// with, kept structurally apart from the root-level control-plane sockets
/// right next to it (and from `util::unsafe_host_artifacts_dir`) rather than
/// distinguished by a reserved filename prefix — the same reasoning as
/// `unsafe_host_artifacts_dir`, and what actually stops a stream socket from
/// aliasing a different host's control-plane socket (both would otherwise
/// end in `.ctl` at the same root). Named `.s`, not `.streams`, for the
/// same budget reason `unsafe_host_artifacts_dir` is `.h`.
fn streams_dir(state_dir: &std::path::Path) -> PathBuf {
    state_dir.join(".s")
}

/// Control socket for one ssh stream-pool slot (see `stream_pool`): a private
/// mux master shared by up to `ssh_streams_per_connection` pane streams,
/// distinct from both the daemon's own `<host>.ctl` control-plane master and
/// from every pane's data connection when pooling is off.
///
/// Always lives under `streams_dir`, named `<namespace>-<slot:08x>.ctl`:
/// `namespace` is `util::stream_namespace_hash(host, target)` (16 hex
/// chars, no readable host-name prefix), and `slot` is hex-encoded to a
/// fixed 8 digits so the filename's length never depends on how large a
/// slot index gets — `ssh_streams_per_connection` bounds pane count per
/// slot, not the number of slots a host can ever reach, so nothing about
/// slot width can be assumed small. `config::load_config_for_env` validates
/// this fixed length once, for one representative slot, rather than
/// needing to re-check per slot width. `config::validate_unique_stream_namespaces`
/// checks every configured pooled host's namespace (not per slot — the
/// namespace alone determines any collision) for the residual
/// (astronomically unlikely) case of two hashing alike.
pub(crate) fn stream_control_path(
    state_dir: &std::path::Path,
    host_name: &str,
    target: &str,
    slot: u32,
) -> PathBuf {
    let dir = streams_dir(state_dir);
    let _ = crate::util::ensure_private_dir(&dir);
    let namespace = crate::util::stream_namespace_hash(host_name, target);
    dir.join(format!("{namespace}-{slot:08x}.ctl"))
}

impl RemoteHost {
    pub fn new(cfg: &HostConfig, state_dir: &std::path::Path) -> RemoteHost {
        let (dir, stem) = control_socket_base(state_dir, &cfg.name);
        RemoteHost {
            ctl_path: dir.join(format!("{stem}.ctl")),
            fwd_sock: dir.join(format!("{stem}-api.sock")),
            transport_hint: cfg.api_transport,
            cfg: cfg.clone(),
            forwarded: false,
            container: None,
            relay: None,
            exec_relay: None,
            exec_sock: dir.join(format!("{stem}-api-exec.sock")),
            last_api_transport: None,
            log: Logger::new(state_dir, false),
        }
    }

    /// Seed the transport hint from a daemon-remembered choice. A no-op
    /// unless the host is configured `api_transport = "auto"` (the default):
    /// an explicit `socket` or `exec` override always pins its own choice and
    /// ignores anything remembered from a previous connection.
    pub fn hint_transport(&mut self, hint: Option<ApiTransport>) {
        if self.cfg.api_transport == ApiTransport::Auto {
            if let Some(h) = hint {
                self.transport_hint = h;
            }
        }
    }

    /// Bring the transport up: an ssh ControlMaster, or a resolved container.
    ///
    /// ssh hosts take the identical path they always did; the docker branch is
    /// additive.
    pub async fn ensure_ready(&mut self) -> Result<()> {
        if !self.cfg.kind.is_docker() {
            return self.ensure_master().await;
        }
        let bin = self.cfg.docker_bin.clone();
        let ids = crate::docker::resolve(&bin, &self.cfg.kind).await?;
        let Some(id) = ids.first().cloned() else {
            // a stopped devcontainer is the resting state, not a fault; the
            // daemon matches this marker to back off gently
            return Err(err(format!(
                "{DORMANT}: no running container for {}",
                self.cfg.target
            )));
        };
        if ids.len() > 1 {
            // reachable without an attacker: a compose devcontainer can put the
            // same local_folder label on several services
            self.log.log(&format!(
                "[{}] {} containers match; using {id} — narrow the config if that is wrong",
                self.cfg.name,
                ids.len()
            ));
        }
        // re-probe on every (re)connect: a rebuilt container may differ
        crate::docker::probe_socat(&bin, &id).await?;
        self.container = Some(crate::docker::Container {
            id,
            docker_bin: bin,
        });
        Ok(())
    }

    fn base_args(&self) -> Vec<String> {
        vec![
            "-S".into(),
            self.ctl_path.display().to_string(),
            "-o".into(),
            "BatchMode=yes".into(),
        ]
    }

    pub async fn ensure_master(&mut self) -> Result<()> {
        let state = ensure_master_locked(
            &self.ctl_path,
            &self.cfg.target,
            "yes",
            CONTROL_MASTER_CHECK_TIMEOUT_MS,
            CONTROL_MASTER_START_TIMEOUT_MS,
        )
        .await?;
        if matches!(state, MasterState::Started) {
            // API-socket forwards registered on the master that just failed
            // its check are gone with it; the caller must re-add them now
            // that a fresh master is up
            self.forwarded = false;
        }
        Ok(())
    }

    pub async fn exec(&self, command: &str, timeout_ms: u64) -> Result<String> {
        if let Some(c) = &self.container {
            return c.exec(command, timeout_ms).await;
        }
        let mut args = self.base_args();
        args.extend([self.cfg.target.clone(), command.to_string()]);
        let res = ssh(&args, timeout_ms).await;
        if res.code != 0 {
            return Err(err(format!(
                "ssh exec failed ({command}): {}",
                nonempty(&res.err, res.code)
            )));
        }
        Ok(res.out)
    }

    pub async fn status(&self) -> Result<RemoteStatus> {
        let bin = crate::config::remote_herdr_expr(
            self.cfg.remote_bin.as_deref(),
            self.cfg.session.as_deref(),
        );
        let out = self
            .exec(&format!("exec {} status --json", bin), 15000)
            .await?;
        #[derive(Deserialize)]
        struct Client {
            version: Option<String>,
        }
        #[derive(Deserialize)]
        struct Server {
            running: Option<bool>,
            socket: Option<String>,
            version: Option<String>,
        }
        #[derive(Deserialize)]
        struct StatusJson {
            client: Option<Client>,
            server: Option<Server>,
        }
        let parsed: StatusJson = serde_json::from_str(&out)?;
        let version = parsed
            .server
            .as_ref()
            .and_then(|s| s.version.clone())
            .or(parsed.client.and_then(|c| c.version))
            .unwrap_or_else(|| "unknown".into());
        let running = parsed.server.as_ref().and_then(|s| s.running) == Some(true);
        let socket = parsed.server.and_then(|s| s.socket).unwrap_or_default();
        let mut status = RemoteStatus {
            socket,
            supported: false,
            reason: None,
        };
        if !running {
            // Name the session, or this reads as "that machine's herdr is
            // down" while the default session is running perfectly and only
            // the configured one is stopped — which is the common way to get
            // here once `session` is in play (a typo, or `herdr session stop`).
            status.reason = Some(match &self.cfg.session {
                Some(name) => format!("remote herdr session {name:?} is not running"),
                None => "remote herdr server is not running".into(),
            });
            return Ok(status);
        }
        match version_supported(&version) {
            Some(true) => status.supported = true,
            Some(false) => {
                status.reason = Some(format!(
                    "remote herdr {version} lacks terminal session streams (need >= 0.7.2 or preview {MIN_PREVIEW_BUILD})"
                ))
            }
            None => status.reason = Some(format!("cannot parse remote version {version}")),
        }
        Ok(status)
    }

    pub async fn forward_api(&mut self, remote_socket: &str) -> Result<PathBuf> {
        if self.forwarded && self.fwd_sock.exists() {
            return Ok(self.fwd_sock.clone());
        }
        // NEVER cancel a healthy forward — other processes may be using it
        if self.fwd_sock.exists() && ApiClient::connect(&self.fwd_sock).await.is_ok() {
            self.forwarded = true;
            return Ok(self.fwd_sock.clone());
        }
        let spec = format!("{}:{}", self.fwd_sock.display(), remote_socket);
        // a dead process can leave the forward registered on the master with
        // its socket file unlinked — cancel before re-adding
        let mut cancel = self.base_args();
        cancel.extend([
            "-O".into(),
            "cancel".into(),
            "-L".into(),
            spec.clone(),
            self.cfg.target.clone(),
        ]);
        let _ = ssh(&cancel, 15000).await;
        let _ = std::fs::remove_file(&self.fwd_sock);
        let mut fwd = self.base_args();
        fwd.extend([
            "-O".into(),
            "forward".into(),
            "-L".into(),
            spec,
            self.cfg.target.clone(),
        ]);
        let res = ssh(&fwd, 15000).await;
        if res.code != 0 {
            return Err(err(format!(
                "ssh socket forward failed: {}",
                nonempty(&res.err, res.code)
            )));
        }
        self.forwarded = true;
        Ok(self.fwd_sock.clone())
    }

    /// Try the streamlocal `-L` forward, verified with a real ping — not just
    /// that `ssh -O forward` reported success.
    ///
    /// The forward registering successfully is not proof the transport works:
    /// some sshds (embedded Go sshds fronting container/VM workspaces are the
    /// case this was written against) accept a direct-streamlocal channel
    /// open and then never service it. Every byte written just sits there, so
    /// the first sign of trouble is the API layer's own connect/ping timing
    /// out or the channel closing with zero bytes read — which is exactly
    /// what `ApiClient::connect`'s ping round-trip surfaces.
    ///
    /// That ping is the client `connect_api` returns, not an extra probe on
    /// top of it: a working host must not pay a round trip for a fallback it
    /// never needs.
    async fn try_socket_transport(&mut self, remote_socket: &str) -> Result<ApiClient> {
        let sock = self.forward_api(remote_socket).await?;
        ApiClient::connect(&sock).await
    }

    /// Drop a forward that just proved itself dead, so it doesn't sit
    /// registered on the ControlMaster for the connection's life with its
    /// socket file unlinked. Unlike `forward_api`'s guard this cannot steal a
    /// healthy forward: it only runs after a real ping failed.
    async fn cancel_forward(&mut self, remote_socket: &str) {
        let spec = format!("{}:{}", self.fwd_sock.display(), remote_socket);
        let mut args = self.base_args();
        args.extend([
            "-O".into(),
            "cancel".into(),
            "-L".into(),
            spec,
            self.cfg.target.clone(),
        ]);
        let _ = ssh(&args, 15000).await;
        let _ = std::fs::remove_file(&self.fwd_sock);
        self.forwarded = false;
    }

    /// Bridge the remote socket over a plain ssh exec channel instead of a
    /// streamlocal forward. See `ssh_relay` for the transport itself; this
    /// only resolves the relay command once and (re)starts the listener,
    /// mirroring the docker branch below one function down.
    async fn exec_relay_transport(&mut self, remote_socket: &str) -> Result<PathBuf> {
        // NEVER steal a healthy relay — same reasoning as the docker guard:
        // the socket path is per-host but shared across processes (daemon,
        // `remote-*` actions, `once`), and state_dir is a single fixed path.
        // `exec_sock` is the relay's OWN path, so a live streamlocal forward
        // can't answer for it and quietly cancel the exec transport.
        if self.exec_relay.is_none() && ApiClient::connect(&self.exec_sock).await.is_ok() {
            return Ok(self.exec_sock.clone());
        }
        self.exec_relay = None;
        let relay_cmd = crate::ssh_relay::detect_relay_command(self, remote_socket).await?;
        self.log.log(&format!(
            "[{}] exec relay via {} → {remote_socket}",
            self.cfg.name,
            relay_cmd.tool()
        ));
        let handle = crate::ssh_relay::serve_relay(
            self.ctl_path.clone(),
            self.cfg.target.clone(),
            relay_cmd,
            self.exec_sock.clone(),
            self.log.clone(),
        )?;
        let path = handle.path.clone();
        self.exec_relay = Some(handle);
        Ok(path)
    }

    /// Choose and reach the ssh API transport: streamlocal socket forward, or
    /// an exec relay. `api_transport = "socket"` / `"exec"` pin one and never
    /// try the other; the default `"auto"` tries the socket transport first
    /// (unless a prior connection in this daemon's lifetime already learned
    /// it doesn't work here — see `hint_transport`) and falls back to the
    /// exec relay on failure, logging the switch exactly once per fallback.
    ///
    /// Returns a CONNECTED client rather than a path: the connect is the
    /// probe, so the socket transport costs a working host exactly what it
    /// cost before this fallback existed.
    async fn connect_ssh_api(&mut self, remote_socket: &str) -> Result<ApiClient> {
        let configured = self.cfg.api_transport;
        let start_with_socket = (if configured == ApiTransport::Auto {
            self.transport_hint
        } else {
            configured
        }) != ApiTransport::Exec;

        if start_with_socket {
            match self.try_socket_transport(remote_socket).await {
                Ok(api) => {
                    self.last_api_transport = Some(ApiTransport::Socket);
                    return Ok(api);
                }
                // only auto may fall back; an explicit `socket` pin means the
                // caller wants the real failure, not a silent transport swap
                Err(e) if configured != ApiTransport::Auto => return Err(e),
                Err(e) => {
                    self.log.log(&format!(
                        "[{}] streamlocal forward unavailable ({e}) — using exec relay",
                        self.cfg.name
                    ));
                    self.transport_hint = ApiTransport::Exec;
                    // it answered nothing; don't leave it registered
                    self.cancel_forward(remote_socket).await;
                }
            }
        }

        let sock = self.exec_relay_transport(remote_socket).await?;
        let api = ApiClient::connect(&sock).await?;
        self.last_api_transport = Some(ApiTransport::Exec);
        Ok(api)
    }

    pub async fn connect_api(&mut self) -> Result<(ApiClient, RemoteStatus)> {
        self.ensure_ready().await?;
        let status = match self.status().await {
            Ok(s) => s,
            Err(_) => {
                // transient mux hiccup (e.g. concurrent -O forward churn) — retry once
                tokio::time::sleep(Duration::from_secs(1)).await;
                self.status().await?
            }
        };
        if !status.supported {
            return Err(err(status
                .reason
                .clone()
                .unwrap_or_else(|| "remote unsupported".into())));
        }
        // ssh hosts hand back a connected client (its ping doubles as the
        // transport probe); the docker branch resolves a path and connects below
        let container = self.container.clone();
        let sock = match &container {
            None => return Ok((self.connect_ssh_api(&status.socket).await?, status)),
            Some(c) => {
                // NEVER steal a healthy relay — the socket path is per-HOST but
                // shared across processes (daemon, `remote-*` actions, `once`),
                // and state_dir is deliberately a single fixed path. Binding on
                // top of a live one orphans the owner's listener and then
                // unlinks the path from under it, bouncing the daemon's whole
                // host connection on every remote action. Same reasoning as the
                // ssh forward guard above.
                if self.relay.is_none() && ApiClient::connect(&self.fwd_sock).await.is_ok() {
                    self.fwd_sock.clone()
                } else {
                    self.relay = None;
                    let handle = crate::docker::serve_relay(
                        c.clone(),
                        status.socket.clone(),
                        self.fwd_sock.clone(),
                        self.log.clone(),
                    )?;
                    let path = handle.path.clone();
                    self.relay = Some(handle);
                    path
                }
            }
        };
        let api = ApiClient::connect(&sock).await?;
        Ok((api, status))
    }
}

fn nonempty(e: &str, code: i32) -> String {
    let t = e.trim();
    if t.is_empty() {
        format!("exit {code}")
    } else {
        t.to_string()
    }
}

/// `Some(true)` = supported, `Some(false)` = too old, `None` = unparseable.
pub(crate) fn version_supported(version: &str) -> Option<bool> {
    let core = version.split(['-', '+']).next()?;
    let mut it = core.split('.');
    let maj: u64 = it.next()?.parse().ok()?;
    let min: u64 = it.next()?.parse().ok()?;
    let pat: u64 = it.next()?.parse().ok()?;
    let newer_than_base = maj > 0 || min > 7 || (min == 7 && pat > 1);
    // preview builds look like 0.7.1-preview.2026-06-30-<hash>
    let preview_ok = version
        .split_once("-preview.")
        .map(|(_, rest)| {
            rest.get(0..10)
                .map(|d| d >= MIN_PREVIEW_BUILD)
                .unwrap_or(false)
        })
        .unwrap_or(false);
    Some(newer_than_base || preview_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same nonce-collision fix as `filelock::tests::test_path` (see its
    /// comment), compacted to a single 8-hex-char hash: several of this
    /// module's tests bind a real unix socket at this path, which leaves far
    /// less budget than an ordinary file for entropy padding.
    fn test_path(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let salt = &counter as *const u64 as usize;
        let unique = crate::util::short_hash(&format!(
            "{}-{nanos}-{counter}-{salt:x}",
            std::process::id()
        ));
        std::env::temp_dir().join(format!("herdr-mirror-{name}-{unique}"))
    }

    /// The property `run_with_timeout` exists for: a timed-out child must be
    /// definitively dead, not merely asked to die, before this function
    /// returns — proven with a harmless standalone command instead of a
    /// real ssh child. The command would touch a marker file after a delay
    /// longer than the timeout; if the kill were only requested
    /// (`kill_on_drop`'s async, best-effort drop-time behavior) rather than
    /// awaited, the process could still be alive racing to create that
    /// marker after this function returns — this waits well past that delay
    /// and asserts the marker never appears.
    #[tokio::test]
    async fn run_with_timeout_definitively_kills_a_slow_child_before_returning() {
        let marker = test_path("timeout-marker");
        let mut cmd = Command::new("sh");
        cmd.args(["-c", &format!("sleep 0.3 && touch {}", marker.display())]);

        let result = run_with_timeout(cmd, 30, "timed out").await;
        assert_eq!(result.err, "timed out");

        // well past the 0.3s the child would have needed to reach `touch`
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            !marker.exists(),
            "a timed-out child must never reach its marker"
        );
        let _ = fs::remove_file(&marker);
    }

    /// A child that exits on its own well within the timeout is unaffected:
    /// `run_with_timeout` must still return its real exit code and output,
    /// not treat every call as a kill.
    #[tokio::test]
    async fn run_with_timeout_returns_real_output_for_a_fast_child() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo hi; exit 3"]);
        let result = run_with_timeout(cmd, 2000, "timed out").await;
        assert_eq!(result.code, 3);
        assert_eq!(result.out.trim(), "hi");
    }

    // --- cross-process locking: `filelock`'s own tests cover exclusion,
    // blocking acquire, and the async retry/timeout behavior generically;
    // this only needs to cover what's specific to `cross_process_lock_path`
    // itself (its naming). `master_check`/`start_master` shell out to a real
    // `ssh` binary, so the full ensure-a-master flow isn't practical to
    // exercise deterministically here (this crate has no mocking harness for
    // that boundary, and the existing `ssh_relay::relay_reaches_real_ssh_host`
    // test marks the same kind of real-ssh dependency `#[ignore]`). Live
    // validation of the full concurrent-ensure race across real
    // `herdr-mirror` invocations still needs a real host.

    #[test]
    fn cross_process_lock_path_is_a_sibling_of_the_control_path() {
        let ctl = Path::new("/state/vps-s0-abcd1234.ctl");
        let lock = cross_process_lock_path(ctl);
        assert_eq!(lock, Path::new("/state/vps-s0-abcd1234.ctl.lock"));
    }

    fn ssh_host(name: &str) -> HostConfig {
        HostConfig {
            name: name.into(),
            target: format!("{name}.example.com"),
            kind: crate::config::HostKind::Ssh,
            docker_bin: "docker".into(),
            prefix: name.into(),
            remote_bin: None,
            session: None,
            max_cols: None,
            max_rows: None,
            api_transport: ApiTransport::Auto,
            always_control: true,
            ssh_streams_per_connection: 1,
        }
    }

    #[test]
    fn long_host_names_use_truncated_socket_paths() {
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        let name = "remote-development-environment-with-a-very-long-name";
        let remote = RemoteHost::new(&ssh_host(name), &state_dir);
        let (dir, stem) = control_socket_base(&state_dir, name);

        assert_eq!(
            dir, state_dir,
            "a safe name, however long, stays at the root"
        );
        assert!(
            stem.len() < name.len(),
            "long name should have been shortened"
        );
        assert!(
            name.starts_with(stem.split('-').next().unwrap()),
            "readable prefix kept"
        );
        assert_eq!(
            remote.ctl_path.file_name().unwrap().to_string_lossy(),
            format!("{stem}.ctl")
        );
        // the ControlPath must survive OpenSSH's mux temp suffix on top
        assert!(remote.ctl_path.as_os_str().len() + 17 <= 103);
        assert!(remote.fwd_sock.as_os_str().len() <= 103);
        assert!(remote.exec_sock.as_os_str().len() <= 103);
    }

    #[test]
    fn stream_control_path_is_stable_and_bounded() {
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        let a = stream_control_path(&state_dir, "vps", "vps.example.com", 0);
        let again = stream_control_path(&state_dir, "vps", "vps.example.com", 0);
        assert_eq!(
            a, again,
            "the same host/target/slot must always derive the same path"
        );
        // must survive OpenSSH's 17-byte mux temp suffix within the portable bound
        assert!(a.as_os_str().len() + 17 <= 103, "{}", a.display());

        let long_name = "remote-development-environment-with-a-very-long-name";
        let bounded = stream_control_path(&state_dir, long_name, "target.example.com", 3);
        assert!(
            bounded.as_os_str().len() + 17 <= 103,
            "{}",
            bounded.display()
        );
    }

    /// `slot` is hex-encoded to a fixed 8 digits (see `stream_control_path`),
    /// so the largest possible slot index must produce the exact same
    /// filename length as slot 0 — the property `validate_socket_path_budget`
    /// relies on to check only one representative slot rather than every
    /// slot a host could ever reach.
    #[test]
    fn stream_control_path_is_the_same_length_for_slot_zero_and_u32_max() {
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        let slot0 = stream_control_path(&state_dir, "vps", "vps.example.com", 0);
        let slot_max = stream_control_path(&state_dir, "vps", "vps.example.com", u32::MAX);
        assert_eq!(slot0.as_os_str().len(), slot_max.as_os_str().len());
        assert_ne!(slot0, slot_max, "still a distinct socket");
    }

    /// Every slot of the same host/target must get its own socket — sharing
    /// one would mean two independently-pooled connections fighting over a
    /// single ControlMaster.
    #[test]
    fn stream_control_path_separates_slots_hosts_and_targets() {
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        let slot0 = stream_control_path(&state_dir, "vps", "vps.example.com", 0);
        let slot1 = stream_control_path(&state_dir, "vps", "vps.example.com", 1);
        assert_ne!(slot0, slot1, "distinct slots must not share a socket");

        let other_host = stream_control_path(&state_dir, "vps2", "vps.example.com", 0);
        assert_ne!(slot0, other_host, "distinct hosts must not share a socket");

        let other_target = stream_control_path(&state_dir, "vps", "other.example.com", 0);
        assert_ne!(
            slot0, other_target,
            "distinct targets must not share a socket"
        );

        // also distinct from the host's own control-plane master and API
        // forward sockets, which are unrelated masters entirely
        assert_ne!(slot0, control_path(&state_dir, "vps"));
    }

    /// Long host names past the sockaddr_un budget must still keep slots
    /// (and other hosts) apart, the same collision-proofing
    /// `control_socket_base` already guarantees for the `.ctl` path.
    #[test]
    fn stream_control_path_never_collides_when_truncated() {
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        let a = "prod-us-east-1-application-server-cluster-node-alpha";
        let b = "prod-us-east-1-application-server-cluster-node-beta";
        assert_ne!(
            stream_control_path(&state_dir, a, "t", 0),
            stream_control_path(&state_dir, b, "t", 0),
        );
        assert_ne!(
            stream_control_path(&state_dir, a, "t", 0),
            stream_control_path(&state_dir, a, "t", 1),
        );
    }

    /// Golden comparison to origin/main behavior: a long, safe host name's
    /// control-plane stem must be byte-for-byte what it was before this
    /// feature ever touched hashing, so an existing install's ControlMaster
    /// and API forward stay at their existing socket path on upgrade.
    #[test]
    fn socket_stem_matches_v0_4_1_baseline_for_a_long_safe_host_name() {
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        let name = "prod-us-east-1-application-server-cluster-node-alpha";
        let (dir, stem) = control_socket_base(&state_dir, name);
        assert_eq!(dir, state_dir);
        assert_eq!(stem, "prod-us-east-1-application-serve-cbd62d65");
    }

    fn state_dir_of_len(bytes: usize) -> PathBuf {
        // An absolute path of exactly `bytes` bytes: 1 for the leading `/`
        // plus `bytes - 1` filler bytes, so callers can hit an exact budget
        // boundary without hand-counting a literal string.
        PathBuf::from(format!("/{}", "a".repeat(bytes - 1)))
    }

    fn pooled_ssh_host(name: &str) -> HostConfig {
        HostConfig {
            ssh_streams_per_connection: 4,
            ..ssh_host(name)
        }
    }

    /// The current real install path, with the two hosts actually
    /// documented in the README, must fit comfortably — this is the
    /// baseline every budget test below is a variation of.
    #[test]
    fn validate_socket_path_budget_accepts_the_real_state_dir_with_azure_and_rdev() {
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        for host in [ssh_host("azure"), pooled_ssh_host("rdev")] {
            validate_socket_path_budget(&state_dir, &host).unwrap();
        }
    }

    /// The reviewer's exact failure mode: a `state_dir` nested deep enough
    /// (as real user home/state directories on macOS routinely are) that
    /// even a normal short host name can no longer fit a live ControlPath
    /// once OpenSSH's mux temp suffix is accounted for. Must be rejected
    /// here, at config load, with an actionable message — not left to fail
    /// silently the first time `ssh` tries to bind it.
    #[test]
    fn validate_socket_path_budget_rejects_a_state_dir_too_deep_for_a_live_control_path() {
        let state_dir = state_dir_of_len(90);
        let err = validate_socket_path_budget(&state_dir, &ssh_host("prod"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("prod"), "{err}");
        assert!(err.contains("103"), "{err}");
    }

    /// Exact boundary for an unsafe (`.h/`-nested) host name, whose stem is
    /// a fixed 16 hex chars regardless of the raw name — the cleanest way
    /// to pin the arithmetic without also depending on truncation. One byte
    /// under the limit passes, one byte over fails, and the failure names
    /// the actual over-budget path rather than just a byte count.
    #[test]
    fn validate_socket_path_budget_boundary_for_an_unsafe_host_name() {
        let unsafe_host = ssh_host("a/b");
        validate_socket_path_budget(&state_dir_of_len(62), &unsafe_host)
            .expect("exactly at the 103-byte (minus 17-byte mux suffix) limit must pass");
        let err = validate_socket_path_budget(&state_dir_of_len(63), &unsafe_host)
            .unwrap_err()
            .to_string();
        assert!(err.contains("a/b"), "{err}");
    }

    /// A pooled host's representative stream-pool socket sits one directory
    /// deeper (`.s/` vs the control-plane's own root or `.h/`) with a longer
    /// (16 vs up to 8 hex) hash, so it goes over budget at a shallower
    /// `state_dir` than the control-plane socket alone would.
    #[test]
    fn validate_socket_path_budget_checks_the_representative_stream_socket_for_a_pooled_host() {
        let state_dir = state_dir_of_len(70);
        let unpooled = ssh_host("vps");
        validate_socket_path_budget(&state_dir, &unpooled)
            .expect("an unpooled host never opens a stream socket, so this depth still fits");

        let pooled = pooled_ssh_host("vps");
        let err = validate_socket_path_budget(&state_dir, &pooled)
            .unwrap_err()
            .to_string();
        assert!(err.contains("stream-pool"), "{err}");
    }

    fn labels(sockets: Vec<(&'static str, PathBuf, usize)>) -> Vec<&'static str> {
        sockets.into_iter().map(|(label, _, _)| label).collect()
    }

    /// Docker never opens a ControlMaster, mux suffix, or exec relay at
    /// all — only its relay, which reuses the plain (non-mux) `-api.sock`
    /// path. `ssh_streams_per_connection` is meaningless for docker (see
    /// `stream_pool`), so it must not add a stream-socket check either even
    /// when config parsing lets it be set on a docker host.
    #[test]
    fn reachable_control_sockets_docker_checks_only_its_relay_socket() {
        let state_dir = PathBuf::from("/state");
        let mut docker = ssh_host("vps");
        docker.kind = crate::config::HostKind::DockerContainer("box".into());
        docker.ssh_streams_per_connection = 4;
        assert_eq!(
            labels(reachable_control_sockets(&state_dir, &docker)),
            ["Docker relay socket"]
        );
    }

    /// `api_transport = "socket"` pins the streamlocal forward exclusively
    /// (see `connect_ssh_api`): the exec relay is never reached, so its
    /// path must never be checked.
    #[test]
    fn reachable_control_sockets_socket_transport_checks_ctl_and_forward_only() {
        let state_dir = PathBuf::from("/state");
        let mut host = ssh_host("vps");
        host.api_transport = ApiTransport::Socket;
        assert_eq!(
            labels(reachable_control_sockets(&state_dir, &host)),
            ["control-plane socket (.ctl)", "API forward socket"]
        );
    }

    /// `api_transport = "exec"` pins the exec relay exclusively: the
    /// streamlocal forward is never reached, so its path must never be
    /// checked.
    #[test]
    fn reachable_control_sockets_exec_transport_checks_ctl_and_exec_only() {
        let state_dir = PathBuf::from("/state");
        let mut host = ssh_host("vps");
        host.api_transport = ApiTransport::Exec;
        assert_eq!(
            labels(reachable_control_sockets(&state_dir, &host)),
            ["control-plane socket (.ctl)", "API exec socket"]
        );
    }

    /// The default `api_transport = "auto"` can fall back from the
    /// streamlocal forward to the exec relay at runtime, so both are
    /// reachable and both must be checked.
    #[test]
    fn reachable_control_sockets_auto_transport_checks_both_api_paths() {
        let state_dir = PathBuf::from("/state");
        let host = ssh_host("vps");
        assert_eq!(
            host.api_transport,
            ApiTransport::Auto,
            "sanity: the default"
        );
        assert_eq!(
            labels(reachable_control_sockets(&state_dir, &host)),
            [
                "control-plane socket (.ctl)",
                "API forward socket",
                "API exec socket"
            ]
        );
    }

    /// A pooled ssh host reaches one more socket than an unpooled one of
    /// the same transport: its stream-pool master.
    #[test]
    fn reachable_control_sockets_pooled_ssh_adds_the_stream_socket() {
        let state_dir = PathBuf::from("/state");
        let host = pooled_ssh_host("vps");
        assert_eq!(
            labels(reachable_control_sockets(&state_dir, &host)),
            [
                "control-plane socket (.ctl)",
                "API forward socket",
                "API exec socket",
                "stream-pool socket"
            ]
        );
    }

    /// A `state_dir` deep enough to reject every ssh host regardless of
    /// `api_transport` (the ControlMaster's mux suffix makes `.ctl` the
    /// tightest of all the families above — see `reachable_control_sockets`'
    /// doc comment) must still accept a docker host at that identical
    /// depth: it never opens a ControlMaster at all, only the shorter,
    /// mux-suffix-free relay socket.
    #[test]
    fn validate_socket_path_budget_docker_accepts_a_depth_that_rejects_every_ssh_transport() {
        let state_dir = state_dir_of_len(80);
        for transport in [ApiTransport::Auto, ApiTransport::Socket, ApiTransport::Exec] {
            let mut host = ssh_host("vps");
            host.api_transport = transport;
            let err = validate_socket_path_budget(&state_dir, &host)
                .unwrap_err()
                .to_string();
            assert!(err.contains("vps"), "{transport:?}: {err}");
        }

        let mut docker = ssh_host("vps");
        docker.kind = crate::config::HostKind::DockerContainer("box".into());
        validate_socket_path_budget(&state_dir, &docker)
            .expect("docker's relay socket alone still fits at a depth that rejects ssh");
    }

    /// A realistic depth genuinely reachable by an ordinary hosts.toml
    /// (well short of the reviewer's failure case above) must accept every
    /// transport pinning and docker alike — the happy path every one of
    /// the reachability variations above still needs to work for.
    #[test]
    fn validate_socket_path_budget_accepts_every_reachable_kind_and_transport_at_a_realistic_depth()
    {
        let state_dir = state_dir_of_len(70);
        for transport in [ApiTransport::Auto, ApiTransport::Socket, ApiTransport::Exec] {
            let mut host = ssh_host("vps");
            host.api_transport = transport;
            validate_socket_path_budget(&state_dir, &host)
                .unwrap_or_else(|e| panic!("{transport:?}: {e}"));
        }
        let mut docker = ssh_host("vps");
        docker.kind = crate::config::HostKind::DockerContainer("box".into());
        validate_socket_path_budget(&state_dir, &docker).unwrap();
    }

    #[test]
    fn truncated_stems_never_collide_across_hosts() {
        // The regression this guards: pure truncation gave these one shared
        // stem, so `-O check` found the other host's live master and every ssh
        // command — remote-invoke included — ran on the wrong machine, while
        // the docker relay unlinked the other host's live socket.
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        let a = "prod-us-east-1-application-server-cluster-node-alpha";
        let b = "prod-us-east-1-application-server-cluster-node-beta";

        // these differ only *past* the cut, so plain truncation collided
        let budget = 103 - state_dir.as_os_str().len() - 1 - 21;
        assert_eq!(
            a[..budget],
            b[..budget],
            "names must be identical up to the cut"
        );

        assert_ne!(
            control_socket_base(&state_dir, a).1,
            control_socket_base(&state_dir, b).1,
            "distinct hosts must not share a socket stem"
        );
        assert_ne!(
            RemoteHost::new(&ssh_host(a), &state_dir).ctl_path,
            RemoteHost::new(&ssh_host(b), &state_dir).ctl_path
        );
    }

    #[test]
    fn short_host_names_keep_their_exact_path() {
        // existing installs must not move to a new socket on upgrade, which
        // would orphan a live ControlMaster
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        assert_eq!(
            control_socket_base(&state_dir, "vps"),
            (state_dir.clone(), "vps".into())
        );
        assert_eq!(
            control_socket_base(&state_dir, "work"),
            (state_dir.clone(), "work".into())
        );
        assert_eq!(
            control_path(&state_dir, "vps"),
            state_dir.join("vps.ctl"),
            "path derivation must match what pre-upgrade daemons used"
        );
    }

    /// The exact compatibility break a later review reported: an `h#`-
    /// prefixed name is just an ordinary safe TOML table key (quoted), not
    /// reserved for anything — it keeps its exact v0.4.1 root-level path,
    /// same as any other safe name. Disjointness from a genuinely hashed
    /// unsafe-name artifact comes from `unsafe_host_artifacts_dir`
    /// (`.h/`), a real directory, not from excluding any particular
    /// name.
    #[test]
    fn an_h_hash_prefixed_host_name_keeps_its_v0_4_1_root_level_path() {
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        assert_eq!(
            control_socket_base(&state_dir, "h#prod"),
            (state_dir.clone(), "h#prod".into())
        );
        assert_eq!(
            control_path(&state_dir, "h#prod"),
            state_dir.join("h#prod.ctl")
        );
    }

    #[test]
    fn legacy_socket_hash_matches_v0_4_1_baseline() {
        // Golden values, unchanged since before this crate hashed anything
        // wider than 32 bits: this function must never move, or a long safe
        // host name's `.ctl` path moves on every future upgrade too.
        assert_eq!(legacy_socket_hash("vps"), "4f02d738");
        assert_eq!(
            legacy_socket_hash("prod-us-east-1-application-server-cluster-node-alpha"),
            "cbd62d65"
        );
    }

    /// A genuine (found by brute-force search, not synthetic) 32-bit
    /// FNV-1a collision between two short, plain, safe host names: both
    /// stay verbatim at the root under any realistic budget, so their
    /// control-plane paths never actually collide despite the hash match.
    #[test]
    fn legacy_socket_hash_has_a_known_real_collision() {
        assert_eq!(
            legacy_socket_hash("host10579"),
            legacy_socket_hash("host343082")
        );
        assert_eq!(legacy_socket_hash("host10579"), "350e55f1");

        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        assert_ne!(
            control_path(&state_dir, "host10579"),
            control_path(&state_dir, "host343082")
        );
    }

    /// Regression: a host name is a user-chosen TOML table key, not a
    /// validated filename. A raw `/` or an embedded NUL must never reach a
    /// root-level socket path with a mid-filename component boundary —
    /// every unsafe name's derived path must live under the private
    /// `unsafe_host_artifacts_dir` subdirectory instead, and the same
    /// unsafe name must always degrade to the identical (safe) stem there.
    /// `.`/`..`/empty are NOT unsafe here — see
    /// `dot_and_empty_host_names_stay_at_the_root_like_v0_4_1` below.
    #[test]
    fn socket_paths_stay_contained_for_unsafe_host_names() {
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        let hosts_dir = crate::util::unsafe_host_artifacts_dir(&state_dir);
        for unsafe_name in ["../../escape", "a/b", "/etc/passwd"] {
            let ctl = control_path(&state_dir, unsafe_name);
            assert_eq!(
                ctl.parent(),
                Some(hosts_dir.as_path()),
                "{unsafe_name} -> {}",
                ctl.display()
            );
            let stream = stream_control_path(&state_dir, unsafe_name, "target", 0);
            assert!(
                stream.starts_with(&state_dir),
                "{unsafe_name} stream path {} must stay within state_dir",
                stream.display()
            );
            assert_eq!(
                control_path(&state_dir, unsafe_name),
                ctl,
                "the same unsafe name must always degrade to the same stem"
            );
        }
    }

    /// The exact compatibility break a later review reported: every
    /// artifact appends a suffix before joining (`.ctl`, `-map.json`, ...),
    /// so a host named `""`/`"."`/`".."` never reaches `Path::join` as a
    /// bare special component — `format!("{stem}.ctl")` produces the
    /// literal filenames `.ctl`/`..ctl`/`...ctl`, not "here"/"parent".
    /// These names must stay at the root with their exact v0.4.1 path, not
    /// be diverted into `.h/`.
    #[test]
    fn dot_and_empty_host_names_stay_at_the_root_like_v0_4_1() {
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        assert_eq!(control_path(&state_dir, ""), state_dir.join(".ctl"));
        assert_eq!(control_path(&state_dir, "."), state_dir.join("..ctl"));
        assert_eq!(control_path(&state_dir, ".."), state_dir.join("...ctl"));
        // a backslash is an ordinary character on macOS/Linux, not a
        // separator, so it stays safe too
        assert_eq!(control_path(&state_dir, "a\\b"), state_dir.join("a\\b.ctl"));
    }

    /// A hashed (unsafe-name) socket path can never coincide with an
    /// unrelated, ordinary verbatim safe host name's path — the exact
    /// defect an earlier review reported. Directory separation, not a
    /// reserved prefix, is what guarantees it here: the two simply live in
    /// different directories regardless of what either stem looks like.
    #[test]
    fn hashed_socket_paths_never_collide_with_an_ordinary_verbatim_name() {
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        let unsafe_name = "x/EaL2b7VcKy";
        let hashed_path = control_path(&state_dir, unsafe_name);
        assert!(hashed_path.starts_with(crate::util::unsafe_host_artifacts_dir(&state_dir)));

        // a lookalike safe name that could be mistaken for a hash keeps its
        // own, verbatim, root-level, non-colliding path
        let lookalike_safe_name = "92f037a4";
        assert_eq!(
            control_path(&state_dir, lookalike_safe_name),
            state_dir.join("92f037a4.ctl")
        );
        assert_ne!(hashed_path, control_path(&state_dir, lookalike_safe_name));
    }

    /// The exact cross-family alias an independent review reported: `alpha`'s
    /// stream-pool socket for slot 0 must never be reachable at the same
    /// path as a distinct host literally named to look like that stream
    /// stem — old naming echoed the host name into the stream stem
    /// (`alpha-s0-<hash>`) at the SAME root as control-plane sockets, so a
    /// second host named exactly that string got the identical `.ctl` path
    /// as `alpha`'s slot-0 stream socket. Stream sockets now live under a
    /// disjoint `.s/` directory with no readable host-name prefix at
    /// all, so no root-level (or even `.h/`-nested) control-plane path
    /// can ever land there.
    #[test]
    fn reported_cross_family_alias_no_longer_reproduces() {
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        // the "lookalike" is the exact stream stem: namespace hash plus its
        // fixed-width hex slot suffix, the whole filename stem sans `.ctl`
        let lookalike_host_name = format!(
            "{}-{:08x}",
            crate::util::stream_namespace_hash("alpha", "alpha.example.com"),
            0u32
        );
        assert!(
            crate::util::is_safe_path_component(&lookalike_host_name),
            "sanity: otherwise a safe component"
        );

        let stream_path = stream_control_path(&state_dir, "alpha", "alpha.example.com", 0);
        let lookalike_control_path = control_path(&state_dir, &lookalike_host_name);
        assert_ne!(stream_path, lookalike_control_path);
        // the lookalike name is just an ordinary safe name (nothing is
        // reserved) — it keeps its own root-level path too
        assert_eq!(
            lookalike_control_path,
            state_dir.join(format!("{lookalike_host_name}.ctl"))
        );
    }

    /// General form of the same property: no control-plane path (`.ctl` for
    /// any host, root-level or under `.h/`) can ever equal any
    /// stream-pool path (any host, target, slot, always under `.s/`)
    /// — the two live in structurally disjoint directories, and an ordinary
    /// safe host name is never diverted from the root.
    #[test]
    fn control_and_stream_paths_never_collide_across_many_names() {
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        let host_names = [
            "alpha",
            "beta-host",
            "h#prod",
            "a/b",
            "..",
            "x/EaL2b7VcKy",
            "prod-us-east-1-application-server-cluster-node-alpha",
        ];
        for host in host_names {
            let ctl = control_path(&state_dir, host);
            for target in ["alpha.example.com", "other.example.com"] {
                for slot in 0..3 {
                    let stream = stream_control_path(&state_dir, host, target, slot);
                    assert_ne!(ctl, stream, "host={host} target={target} slot={slot}");
                }
            }
        }
    }

    #[test]
    fn socket_stem_survives_multibyte_names_and_a_tiny_budget() {
        let state_dir = PathBuf::from("/Users/example/.local/state/herdr-mirror");
        // truncation must land on a char boundary, never split a code point
        let name = "höst-nàme-with-ünicode-and-a-very-long-tail-that-overflows";
        let stem = control_socket_base(&state_dir, name).1;
        assert!(stem.is_char_boundary(stem.len()));

        // a state dir long enough to eat the whole budget still yields distinct
        // stems rather than one shared path
        let deep = PathBuf::from("/Users/example/".to_string() + &"d".repeat(80));
        assert_ne!(
            control_socket_base(&deep, "alpha-host-name").1,
            control_socket_base(&deep, "beta-host-name").1
        );
        assert!(!control_socket_base(&deep, "alpha-host-name").1.is_empty());
    }

    #[tokio::test]
    async fn timeout_terminates_proxy_command() {
        let pid_path = test_path("timed-out-proxy-pid");
        let proxy = format!(
            "ProxyCommand=sh -c 'echo $$ > {}; exec sleep 30'",
            pid_path.display()
        );
        let args = vec!["-o".into(), proxy, "timeout-test.invalid".into()];

        let output = ssh(&args, 250).await;
        let proxy_pid: i32 = fs::read_to_string(&pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut proxy_survived = false;
        for _ in 0..20 {
            proxy_survived = unsafe { libc::kill(proxy_pid, 0) } == 0;
            if !proxy_survived {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        if proxy_survived {
            unsafe { libc::kill(proxy_pid, libc::SIGKILL) };
        }
        let _ = fs::remove_file(pid_path);
        assert_eq!(output.err, "ssh timeout");
        assert!(!proxy_survived, "ProxyCommand survived the ssh timeout");
    }

    #[test]
    fn removes_stale_control_socket() {
        let path = test_path("stale-control");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(listener);

        remove_stale_control_socket(&path).unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn refuses_to_replace_non_socket_control_path() {
        let path = test_path("non-socket-control");
        fs::write(&path, "do not delete").unwrap();

        let error = remove_stale_control_socket(&path).unwrap_err().to_string();

        assert!(error.contains("is not a socket"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "do not delete");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn version_gate() {
        assert_eq!(version_supported("0.7.1"), Some(false));
        assert_eq!(version_supported("0.7.2"), Some(true));
        assert_eq!(version_supported("0.8.0"), Some(true));
        assert_eq!(version_supported("1.0.0"), Some(true));
        assert_eq!(
            version_supported("0.7.1-preview.2026-06-30-3459798b606d"),
            Some(true)
        );
        assert_eq!(
            version_supported("0.7.1-preview.2026-07-04-aaaa"),
            Some(true)
        );
        assert_eq!(
            version_supported("0.7.1-preview.2026-06-29-aaaa"),
            Some(false)
        );
        assert_eq!(version_supported("garbage"), None);
    }
}
