// Shared plumbing: error alias, environment/path resolution, logging.

use std::fs;
use std::io::Write;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

pub fn err(msg: impl Into<String>) -> Error {
    msg.into().into()
}

pub fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

/// herdr's own config.toml — same precedence herdr's config_path() uses.
pub fn herdr_config_path() -> PathBuf {
    if let Ok(p) = std::env::var("HERDR_CONFIG_PATH") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("herdr/config.toml"),
        _ => home_dir().join(".config/herdr/config.toml"),
    }
}

/// Resolved runtime environment. Config is searched across candidate dirs so
/// shell and plugin-action invocations agree (see `config_candidates`); state
/// is ALWAYS the fixed path so both share one id map and pidfile.
pub struct Env {
    /// config dirs to search, in order (see `config_candidates`)
    pub config_search: Vec<PathBuf>,
    pub state_dir: PathBuf,
    pub local_socket: PathBuf,
}

impl Env {
    pub fn resolve() -> Result<Env> {
        let config_search = config_candidates();
        let state_dir = home_dir().join(".local").join("state").join("herdr-mirror");
        // create only the canonical dir; the others are probed, not owned
        fs::create_dir_all(default_config_dir())?;
        fs::create_dir_all(&state_dir)?;
        let local_socket = match std::env::var("HERDR_SOCKET_PATH") {
            Ok(s) if !s.is_empty() => PathBuf::from(s),
            _ => {
                let out = std::process::Command::new("herdr")
                    .args(["status", "--json"])
                    .output()
                    .map_err(|e| err(format!("cannot run herdr status: {e}")))?;
                let parsed: serde_json::Value = serde_json::from_slice(&out.stdout)?;
                let sock = parsed
                    .pointer("/server/socket")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if sock.is_empty() {
                    return Err(err(
                        "cannot resolve local herdr socket (HERDR_SOCKET_PATH unset, herdr status gave none)",
                    ));
                }
                PathBuf::from(sock)
            }
        };
        Ok(Env {
            config_search,
            state_dir,
            local_socket,
        })
    }
}

/// Canonical config dir: the one path both plugin actions and shell
/// invocations can always reach, so it's what we create and what docs name.
pub fn default_config_dir() -> PathBuf {
    home_dir().join(".config").join("herdr-mirror")
}

const DELETED_MARKER: &[u8] = b" (deleted)";

/// `/proc/self/exe` gains a literal " (deleted)" suffix once the file behind it
/// is replaced or unlinked. Returns the path without it, or None when there is
/// no marker to strip. Pure, so the rule is testable without unlinking a binary.
fn strip_deleted_marker(p: &Path) -> Option<PathBuf> {
    let stripped = p.as_os_str().as_bytes().strip_suffix(DELETED_MARKER)?;
    Some(PathBuf::from(OsString::from_vec(stripped.to_vec())))
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Resolve the path a future child should execute from a path reported for the
/// current process. The replacement must already exist and be executable: a
/// path that merely happens to look right is not safe to put into pane argv.
fn resolve_reported_exe(reported: &Path) -> Option<PathBuf> {
    if is_executable_file(reported) {
        return Some(reported.to_path_buf());
    }
    let stripped = strip_deleted_marker(reported)?;
    is_executable_file(&stripped).then_some(stripped)
}

/// A path to THIS binary that can still be RUN.
///
/// `current_exe()` reads /proc/self/exe, and Linux appends " (deleted)" to that
/// link the moment the file behind it is replaced -- which every rebuild does to
/// a running daemon. Rust passes the suffix straight through, so the value went
/// into the streamer argv and produced `exec '/path/herdr-mirror (deleted)'`: a
/// command that cannot run, leaving a bare shell parked in the `.mirror-pane`
/// placeholder where a mirror should be. The same value was also handed to
/// `Command::new` when respawning the daemon, and symlinked into the CLI link by
/// `repair_cli_link`, which would have made the breakage outlive the process.
///
/// Order: the reported path if it is executable, then the same path with the
/// Linux marker stripped (a rebuild in place -- the common case). The CLI link
/// is deliberately not a fallback: this project permits that link to point at
/// another installation and reports it as `CliLink::Other` below.
pub fn self_exe_path() -> Option<PathBuf> {
    let reported = std::env::current_exe().ok()?;
    resolve_reported_exe(&reported)
}

/// `self_exe_path` as a command string, falling back to a bare name so PATH
/// lookup still gives a streamer something to try.
pub fn self_exe() -> String {
    self_exe_path().map(|p| p.display().to_string()).unwrap_or_else(|| "herdr-mirror".into())
}

/// The stable CLI path install.sh links and the README's keybindings use.
pub fn cli_link_path() -> PathBuf {
    home_dir().join(".local").join("bin").join("herdr-mirror")
}

pub enum CliLink {
    /// resolves to the running binary
    Ok(PathBuf),
    /// nothing at the path
    Missing,
    /// a symlink whose target no longer exists
    Dangling(PathBuf),
    /// a live symlink to some other binary — a deliberate arrangement
    Other(PathBuf),
    /// a regular file we don't manage
    File,
}

pub fn cli_link_state() -> CliLink {
    let link = cli_link_path();
    match fs::read_link(&link) {
        // read_link fails for both "missing" and "not a symlink"
        Err(_) if !link.exists() => CliLink::Missing,
        Err(_) => CliLink::File,
        Ok(target) => {
            let resolved = fs::canonicalize(&link).ok();
            let exe = self_exe_path().and_then(|e| fs::canonicalize(e).ok());
            match resolved {
                None => CliLink::Dangling(target),
                Some(r) if exe.as_ref() == Some(&r) => CliLink::Ok(target),
                Some(_) => CliLink::Other(target),
            }
        }
    }
}

/// The states worth interrupting the user about: keybindings through the link
/// cannot fire at all. `Other`/`File` are deliberate arrangements, not
/// breakage, so they never warn — `status` still shows them.
pub fn cli_link_problem() -> Option<String> {
    match cli_link_state() {
        CliLink::Missing => Some(format!("{} is missing", cli_link_path().display())),
        CliLink::Dangling(t) => Some(format!(
            "{} dangles (-> {})",
            cli_link_path().display(),
            t.display()
        )),
        _ => None,
    }
}

/// Repair a missing/dangling link by pointing it at the running binary.
/// Reserved for the explicit `start` command — the daemon only reports (see
/// cli_link_problem), so nothing rewrites the filesystem in the background.
/// A live foreign link or real file is never replaced.
pub fn repair_cli_link() -> Option<String> {
    cli_link_problem()?;
    let link = cli_link_path();
    let exe = self_exe_path()?;
    let _ = fs::create_dir_all(link.parent()?);
    let _ = fs::remove_file(&link);
    Some(match std::os::unix::fs::symlink(&exe, &link) {
        Ok(()) => format!("relinked {} -> {}", link.display(), exe.display()),
        Err(e) => format!("could not relink {}: {e}", link.display()),
    })
}

/// Config dirs to search, most specific first.
///
/// Order matters more than it looks. herdr injects `HERDR_PLUGIN_CONFIG_DIR`
/// into plugin actions but not into a shell, so resolution must not *branch*
/// on it: a config only reachable when that variable happens to be set is
/// visible to the autostart hook and invisible to the same command typed in a
/// terminal. Probing the conventional plugin dir unconditionally means a
/// README-following user (who is told to use `herdr plugin config-dir mirror`)
/// gets the same answer in both modes.
pub fn config_candidates() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = std::env::var("HERDR_PLUGIN_CONFIG_DIR") {
        if !dir.is_empty() {
            dirs.push(PathBuf::from(dir));
        }
    }
    dirs.push(home_dir().join(".config/herdr/plugins/config/mirror"));
    dirs.push(default_config_dir());
    // NOT Vec::dedup, which only collapses *consecutive* duplicates: with
    // HERDR_PLUGIN_CONFIG_DIR set to the canonical dir the list is
    // [canonical, plugin, canonical], so the duplicates are not adjacent and
    // both survive — making the daemon warn that it is "ignoring" the very file
    // it is reading.
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    dirs
}

/// Append-to-file logger (best-effort), optionally echoing to stdout.
#[derive(Clone)]
pub struct Logger {
    file: PathBuf,
    also_stdout: bool,
}

impl Logger {
    pub fn new(state_dir: &Path, also_stdout: bool) -> Logger {
        Logger {
            file: state_dir.join("daemon.log"),
            also_stdout,
        }
    }

    pub fn log(&self, msg: &str) {
        let line = format!("{} {}\n", now_iso(), msg);
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)
        {
            let _ = f.write_all(line.as_bytes());
        }
        if self.also_stdout {
            print!("{line}");
            let _ = std::io::stdout().flush();
        }
    }
}

/// ISO-8601 UTC timestamp without pulling in chrono.
pub fn now_iso() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let millis = d.subsec_millis();
    let days = secs / 86400;
    let (y, mo, dy) = civil_from_days(days as i64);
    let rem = secs % 86400;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y,
        mo,
        dy,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        millis
    )
}

// Howard Hinnant's civil-from-days algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Is a pid alive? (signal 0)
pub fn pid_alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

/// Pidfile a pane streamer writes at startup. The daemon starts streamers by
/// TYPING `exec ...` into a fresh shell pane, and interactive shell startup
/// can eat keystrokes (oh-my-zsh's update prompt swallows the leading `e` —
/// and re-prompts in every new shell until answered, so it fails every spawn,
/// not one in a fortnight). The pidfile is how the daemon can tell the exec
/// took, and retype it when it didn't.
/// Squash anything that isn't `[A-Za-z0-9]` so an id can name a file.
pub fn sane_component(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// FNV-1a, 64-bit, as 16 hex chars.
///
/// NOT `DefaultHasher`: values derived from this land in paths the daemon
/// and every streamer must derive identically, and it has to survive a
/// toolchain upgrade. std's hasher is explicitly unstable across Rust
/// releases, so a bump would silently move a host's socket and orphan its
/// live ControlMaster. FNV-1a is a published algorithm, so the output is
/// fixed by spec rather than by implementation detail. Full 64-bit output
/// (not truncated to 32) keeps a real collision astronomically unlikely.
/// `short_hash_is_stable` (below) pins the exact value so a future swap
/// cannot move anyone's paths unnoticed. Not used for the control-plane
/// `.ctl` socket of a long SAFE host name — see `remote::legacy_socket_hash`
/// for why that one path stays on the original 32-bit value.
///
/// Hashes the raw bytes rather than going through `Hash for str`, which
/// appends a terminator byte and would give a different (still stable, but
/// arbitrary) value.
pub fn short_hash(s: &str) -> String {
    use std::hash::Hasher;

    let mut hasher = fnv::FnvHasher::default();
    hasher.write(s.as_bytes());
    format!("{:016x}", hasher.finish())
}

/// Whether `s` is safe to use as a host-artifact stem at the root of
/// `state_dir`: every artifact this crate derives from a host name (state
/// map, hidden marker, host lock, atomic temp file, control-plane socket)
/// always appends a suffix (`.ctl`, `-map.json`, `.hidden`, `.state.lock`)
/// — and the atomic temp file a leading `.` too — before joining, so the
/// bare stem is never the WHOLE path component by itself. That is what
/// makes `.`/`..`/empty harmless here even though `Path::join` would
/// resolve a BARE `.`/`..` component as "here"/"parent": `format!("{stem}.ctl")`
/// with stem `.` or `..` produces the literal filenames `..ctl`/`...ctl`
/// (or `.ctl` for an empty stem), never the actual one/two-character
/// special components. A raw `/` is the one thing no suffix can
/// neutralize — it embeds a component boundary in the MIDDLE of a
/// filename, wherever it falls — and NUL is invalid in a Unix filename at
/// all, so those two are the only bytes that require hashing away instead
/// (see `unsafe_host_artifacts_dir`). A backslash is an ordinary character
/// on this crate's only supported platforms (macOS/Linux), not a
/// separator, so it does not need excluding either.
///
/// Nothing is reserved beyond that: an unusual-but-safe name (an
/// `h#`-prefixed one, an empty one, a bare `.`/`..` one, all included) is
/// exactly as safe as any other and stays at the root of `state_dir` with
/// its untouched v0.4.1 path. Disjointness between a root-level safe name
/// and an internal hashed artifact comes from directory structure
/// (`unsafe_host_artifacts_dir`, `remote`'s `.s`) — see their doc comments
/// — never from excluding any particular name.
pub fn is_safe_path_component(s: &str) -> bool {
    !s.contains('/') && !s.contains('\0')
}

/// Directory for every artifact derived from an UNSAFE host name (state
/// map, hidden marker, host lock, atomic temp file, control-plane socket):
/// a host name is a user-chosen TOML table key, so nothing stops it from
/// containing a `/` or embedding a NUL, neither of which any suffix can
/// keep contained to one literal filename (see `is_safe_path_component`).
/// Named `.h`, not `.hosts`, to leave more of the sockaddr_un budget for
/// the hashed stem itself once this is nested under `state_dir` (see
/// `remote::control_socket_base`, which validates the result against that
/// budget). Private (mode 0700, via `ensure_private_dir`) and, critically,
/// structurally disjoint from every root-level safe-name path right next
/// to it: two different directories can never produce the same full path
/// no matter what either one's stem looks like, which is what makes this
/// safe as the sole disjointness mechanism — no reserved filename prefix
/// needed, and so no safe name (an `h#`-prefixed, empty, or dot-only one
/// included) is ever diverted from its exact v0.4.1 path.
pub fn unsafe_host_artifacts_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(".h")
}

/// Stable stem for an unsafe host name's artifacts within
/// `unsafe_host_artifacts_dir`: its 64-bit hash. Two DIFFERENT unsafe host
/// names sharing this stem would be a real (astronomically unlikely) 64-bit
/// collision; `config::validate_unique_host_stems` checks for it at config
/// load.
pub fn unsafe_host_stem(host_name: &str) -> String {
    short_hash(host_name)
}

/// Create `dir` (and its ancestors) with mode 0700 if it doesn't already
/// exist: every internal-artifacts subdirectory (`unsafe_host_artifacts_dir`,
/// `remote`'s `.s`) holds hashed-but-still-identifying material for hosts
/// that couldn't safely live at the root, so it shouldn't be
/// group/world-traversable even if `state_dir` itself is looser. A no-op,
/// not an error, if the directory already exists (mirrors `create_dir_all`);
/// mode only applies at creation, so a pre-existing directory's permissions
/// are left alone.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Base directory + filename stem for a host-derived artifact (state map,
/// hidden marker, host lock, atomic temp file): the root of `state_dir`
/// with the host name used verbatim when it's safe — the common case, and
/// the one every existing install's paths already use, completely
/// unmodified — or `unsafe_host_artifacts_dir` with its hashed stem
/// otherwise. Config resolution additionally rejects two configured unsafe
/// hosts whose hash collides outright (see
/// `config::validate_unique_host_stems`), so that check is defense in
/// depth, not the only guard against a real (astronomically unlikely)
/// 64-bit collision. NOT used for the control-plane socket — see
/// `remote::control_socket_base`, which has a separate, sockaddr_un-budget-
/// aware path for a safe but overlong name.
pub fn host_artifact_base(state_dir: &Path, host_name: &str) -> (PathBuf, String) {
    if is_safe_path_component(host_name) {
        (state_dir.to_path_buf(), host_name.to_string())
    } else {
        let dir = unsafe_host_artifacts_dir(state_dir);
        let _ = ensure_private_dir(&dir);
        (dir, unsafe_host_stem(host_name))
    }
}

/// 64-bit hash identifying one host+target stream-pool namespace — the set
/// of slot sockets a pooled ssh host's pane streams for one target can use
/// — shared by `remote::stream_control_path` (which appends a fixed-width
/// slot suffix on top, see there) and `config::validate_unique_stream_namespaces`
/// (checked once per pooled host, not per slot: `ssh_streams_per_connection`
/// is a per-slot pane-count *capacity*, not a slot count, so a config-time
/// check can never enumerate "every slot" — the number of slots actually
/// used depends on how many panes are open, not on this number, and can
/// exceed it) — one implementation so the two can never drift on what "the
/// same namespace" means.
pub fn stream_namespace_hash(host_name: &str, target: &str) -> String {
    short_hash(&format!("{host_name}\u{0}{target}"))
}

pub fn streamer_pid_path(state_dir: &Path, ssh_target: &str, pane_target: &str) -> PathBuf {
    state_dir.join("streamer-pids").join(format!(
        "{}--{}.pid",
        sane_component(ssh_target),
        sane_component(pane_target)
    ))
}

/// A launch claim exists from the first local `pane.send_text` until the
/// streamer writes its pidfile. Recovery must not type another command into
/// that pane during this interval: the first streamer can already own stdin
/// even when herdr's process snapshot still reports the shell.
pub fn streamer_spawn_pending_path(state_dir: &Path, local_pane_id: &str) -> PathBuf {
    state_dir
        .join("streamer-spawns")
        .join(format!("{}.pending", sane_component(local_pane_id)))
}

/// How long a `.pending` claim may block another launch. The retype loop is
/// 3s+4s+4s; anything older is leftover from a crashed or abandoned attempt
/// and must not freeze heal forever.
const SPAWN_PENDING_TTL: Duration = Duration::from_secs(30);

fn pending_is_fresh(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < SPAWN_PENDING_TTL)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamerSpawnClaim {
    Claimed,
    Active,
    Pending,
}

/// Where the streamer of a given LOCAL herdr pane announces its pid.
///
/// `streamer_pid_path` addresses a streamer by what it is showing (host + remote
/// pane). This addresses one by where it is showing it, which is the only handle
/// a herdr event hook has: the hook is told a local pane id and nothing else.
pub fn pane_pid_path(state_dir: &Path, local_pane_id: &str) -> PathBuf {
    state_dir
        .join("pane-pids")
        .join(format!("{}.pid", sane_component(local_pane_id)))
}

pub fn streamer_alive(state_dir: &Path, ssh_target: &str, pane_target: &str) -> bool {
    fs::read_to_string(streamer_pid_path(state_dir, ssh_target, pane_target))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .is_some_and(pid_alive)
}

pub fn pane_streamer_alive(state_dir: &Path, local_pane_id: &str) -> bool {
    fs::read_to_string(pane_pid_path(state_dir, local_pane_id))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .is_some_and(pid_alive)
}

/// Claim the right to type one streamer launcher into a local pane.
///
/// The pidfile protects an active streamer. The exclusive pending file closes
/// the startup interval before the streamer can publish its pid. A claim older
/// than `SPAWN_PENDING_TTL` is treated as abandoned so a later heal can type
/// again (session-restore after a spawn that never published a pid).
pub fn claim_streamer_spawn(
    state_dir: &Path,
    ssh_target: &str,
    pane_target: &str,
    local_pane_id: &str,
) -> std::io::Result<StreamerSpawnClaim> {
    if streamer_alive(state_dir, ssh_target, pane_target)
        || pane_streamer_alive(state_dir, local_pane_id)
    {
        return Ok(StreamerSpawnClaim::Active);
    }

    let path = streamer_spawn_pending_path(state_dir, local_pane_id);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    // one retry: a stale file we just unlinked can lose create_new to a racer
    for _ in 0..2 {
        let mut claim = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if pending_is_fresh(&path) {
                    return Ok(StreamerSpawnClaim::Pending);
                }
                let _ = fs::remove_file(&path);
                continue;
            }
            Err(e) => return Err(e),
        };
        writeln!(claim, "{ssh_target} {pane_target}")?;

        // A streamer from an earlier launch can publish its pid between the first
        // check and our claim. Its pid wins, and its startup removes this claim.
        if streamer_alive(state_dir, ssh_target, pane_target)
            || pane_streamer_alive(state_dir, local_pane_id)
        {
            let _ = fs::remove_file(&path);
            return Ok(StreamerSpawnClaim::Active);
        }
        return Ok(StreamerSpawnClaim::Claimed);
    }
    Ok(StreamerSpawnClaim::Pending)
}

pub fn clear_streamer_spawn_pending(state_dir: &Path, local_pane_id: &str) {
    let _ = fs::remove_file(streamer_spawn_pending_path(state_dir, local_pane_id));
}

/// Poke the streamer drawing a given local pane, so it picks up a notice left
/// for it instead of waiting for its next deadline.
///
/// Checks the pid really is one of ours before signalling. A pid file outlives a
/// streamer that was SIGKILLed, pids get reused, and SIGUSR1's default
/// disposition is *terminate* — so acting on a stale file could kill a stranger.
pub fn poke_pane_streamer(state_dir: &Path, local_pane_id: &str) -> bool {
    let Some(pid) = fs::read_to_string(pane_pid_path(state_dir, local_pane_id))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .filter(|p| *p > 1 && pid_alive(*p))
    else {
        return false;
    };
    let ours = std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("herdr-mirror"));
    if !ours {
        return false;
    }
    unsafe { libc::kill(pid, libc::SIGUSR1) == 0 }
}

/// Sleep until the earliest deadline; pend forever when none.
pub async fn sleep_until_earliest<I>(deadlines: I)
where
    I: IntoIterator<Item = Option<tokio::time::Instant>>,
{
    match deadlines.into_iter().flatten().min() {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending::<()>().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "herdr-mirror-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn a_live_pid_claim_blocks_a_recovery_write() {
        let state_dir = test_state_dir("active-spawn");
        let pid_path = streamer_pid_path(&state_dir, "host", "w1:p1");
        fs::create_dir_all(pid_path.parent().unwrap()).unwrap();
        fs::write(&pid_path, std::process::id().to_string()).unwrap();

        assert_eq!(
            claim_streamer_spawn(&state_dir, "host", "w1:p1", "w2:p2").unwrap(),
            StreamerSpawnClaim::Active
        );
        assert!(!streamer_spawn_pending_path(&state_dir, "w2:p2").exists());
        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn a_live_local_pane_pid_blocks_a_recovery_write() {
        let state_dir = test_state_dir("active-local-pane");
        let pid_path = pane_pid_path(&state_dir, "w2:p2");
        fs::create_dir_all(pid_path.parent().unwrap()).unwrap();
        fs::write(&pid_path, std::process::id().to_string()).unwrap();

        assert_eq!(
            claim_streamer_spawn(&state_dir, "host", "w1:p1", "w2:p2").unwrap(),
            StreamerSpawnClaim::Active
        );
        assert!(!streamer_spawn_pending_path(&state_dir, "w2:p2").exists());
        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn one_pending_launch_owns_each_local_pane() {
        let state_dir = test_state_dir("pending-spawn");

        assert_eq!(
            claim_streamer_spawn(&state_dir, "host", "w1:p1", "w2:p2").unwrap(),
            StreamerSpawnClaim::Claimed
        );
        assert_eq!(
            claim_streamer_spawn(&state_dir, "host", "w1:p1", "w2:p2").unwrap(),
            StreamerSpawnClaim::Pending
        );
        clear_streamer_spawn_pending(&state_dir, "w2:p2");
        assert_eq!(
            claim_streamer_spawn(&state_dir, "host", "w1:p1", "w2:p2").unwrap(),
            StreamerSpawnClaim::Claimed
        );
        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn a_stale_pending_claim_can_be_replaced() {
        let state_dir = test_state_dir("stale-pending");
        let path = streamer_spawn_pending_path(&state_dir, "w2:p2");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "host w1:p1\n").unwrap();
        let past = std::time::SystemTime::now()
            .checked_sub(SPAWN_PENDING_TTL + Duration::from_secs(1))
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as libc::time_t;
        let cpath = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        let times = libc::utimbuf {
            actime: past,
            modtime: past,
        };
        assert_eq!(unsafe { libc::utime(cpath.as_ptr(), &times) }, 0);

        assert_eq!(
            claim_streamer_spawn(&state_dir, "host", "w1:p1", "w2:p2").unwrap(),
            StreamerSpawnClaim::Claimed
        );
        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn deleted_marker_is_stripped_only_when_it_is_a_real_suffix() {
        use super::{resolve_reported_exe, strip_deleted_marker};
        assert_eq!(
            strip_deleted_marker(Path::new("/usr/bin/herdr-mirror (deleted)")),
            Some(PathBuf::from("/usr/bin/herdr-mirror"))
        );
        assert_eq!(strip_deleted_marker(Path::new("/usr/bin/herdr-mirror")), None);
        // the words appearing mid-path are a directory name, not the kernel marker
        assert_eq!(strip_deleted_marker(Path::new("/tmp/x (deleted)/herdr-mirror")), None);

        // Model the real replacement case without unlinking the test runner:
        // the reported path is gone, while the original path names the new,
        // executable inode installed in its place.
        let executable = std::env::current_exe().unwrap();
        let mut reported = executable.as_os_str().as_bytes().to_vec();
        reported.extend_from_slice(DELETED_MARKER);
        assert_eq!(
            resolve_reported_exe(Path::new(&OsString::from_vec(reported))),
            Some(executable)
        );
    }

    #[test]
    fn short_hash_is_stable() {
        // Golden values. These bytes are baked into live socket/state paths,
        // so a change here silently relocates every host derived from an
        // unsafe or overlong name. If a hashing swap ever moves them, this
        // must fail first.
        assert_eq!(short_hash("vps"), "693e19194f02d738");
        assert_eq!(
            short_hash("prod-us-east-1-application-server-cluster-node-alpha"),
            "18b2f02acbd62d65"
        );
    }

    #[test]
    fn replacement_path_must_exist_and_be_executable() {
        let root = test_state_dir("replacement-exe");
        fs::create_dir_all(&root).unwrap();
        let candidate = root.join("herdr-mirror");
        fs::write(&candidate, "not executable").unwrap();
        let reported = PathBuf::from(format!("{} (deleted)", candidate.display()));
        assert_eq!(resolve_reported_exe(&reported), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn host_artifact_base_keeps_ordinary_names_at_the_root_verbatim() {
        for name in ["azure", "rdev", "prod-us-east-1", "vps.example.com", "a_b"] {
            let dir = tmpdir("artifact-base");
            let (base, stem) = host_artifact_base(&dir, name);
            assert_eq!(base, dir, "{name}");
            assert_eq!(stem, name, "{name}");
        }
    }

    /// The exact compatibility break a later review reported: an `h#`-
    /// prefixed name is just an ordinary safe TOML table key (quoted), not a
    /// reserved one — it keeps the exact v0.4.1 root-level path, same as any
    /// other safe name.
    #[test]
    fn host_artifact_base_keeps_an_h_hash_prefixed_name_at_the_root_verbatim() {
        let dir = tmpdir("artifact-base");
        let (base, stem) = host_artifact_base(&dir, "h#prod");
        assert_eq!(base, dir);
        assert_eq!(stem, "h#prod");
    }

    #[test]
    fn host_artifact_base_hashes_traversal_and_separator_names_under_the_private_subdir() {
        let dir = tmpdir("artifact-base");
        for unsafe_name in ["a/b", "/etc/passwd", "../../escape", "a/../b"] {
            let (base, stem) = host_artifact_base(&dir, unsafe_name);
            assert_eq!(base, unsafe_host_artifacts_dir(&dir), "{unsafe_name}");
            assert!(is_safe_path_component(&stem), "{unsafe_name} -> {stem}");
            assert_eq!(stem, unsafe_host_stem(unsafe_name), "{unsafe_name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The exact compatibility break a later review reported: `.`/`..`/
    /// empty are NOT unsafe here — every artifact appends a suffix before
    /// joining (see `is_safe_path_component`'s doc comment), so these stay
    /// at the root with their host name used verbatim, same as any other
    /// safe name.
    #[test]
    fn host_artifact_base_keeps_dot_and_empty_names_at_the_root_verbatim() {
        let dir = tmpdir("artifact-base");
        for name in ["", ".", ".."] {
            let (base, stem) = host_artifact_base(&dir, name);
            assert_eq!(base, dir, "{name:?}");
            assert_eq!(stem, name, "{name:?}");
        }
    }

    #[test]
    fn host_artifact_base_keeps_unicode_names_at_the_root_verbatim() {
        let dir = tmpdir("artifact-base");
        let name = "réservé-hôte-日本語";
        let (base, stem) = host_artifact_base(&dir, name);
        assert_eq!(base, dir);
        assert_eq!(stem, name);
    }

    #[test]
    fn host_artifact_base_is_deterministic_for_distinct_unsafe_names() {
        let dir = tmpdir("artifact-base");
        assert_eq!(
            host_artifact_base(&dir, "a/b"),
            host_artifact_base(&dir, "a/b")
        );
        assert_ne!(
            host_artifact_base(&dir, "a/b"),
            host_artifact_base(&dir, "c/d")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The exact bug an earlier review reported (a plain, ordinary-looking
    /// safe host name equal to another host's hashed stem) cannot reproduce
    /// at all now: the two live in different directories, so their full
    /// paths are never equal regardless of what either stem looks like.
    #[test]
    fn reported_collision_no_longer_reproduces() {
        let dir = tmpdir("artifact-base");
        let unsafe_name = "x/EaL2b7VcKy";
        let lookalike_safe_name = "92f037a4";
        let (unsafe_base, _unsafe_stem) = host_artifact_base(&dir, unsafe_name);
        let (safe_base, safe_stem) = host_artifact_base(&dir, lookalike_safe_name);
        assert_eq!(safe_base, dir);
        assert_eq!(safe_stem, lookalike_safe_name);
        assert_ne!(
            unsafe_base, safe_base,
            "different directories: can never collide"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The compatibility break a later review reported: a host literally
    /// named to look like a hashed artifact (`h#...`) is still just a safe
    /// name — it must never be diverted into `.h/`.
    #[test]
    fn a_name_that_looks_like_a_hashed_stem_still_stays_at_the_root() {
        let dir = tmpdir("artifact-base");
        let lookalike = "h#deadbeefdeadbeef";
        let (base, stem) = host_artifact_base(&dir, lookalike);
        assert_eq!(base, dir);
        assert_eq!(stem, lookalike);
    }

    #[test]
    fn ensure_private_dir_creates_mode_0700() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tmpdir("private-dir").join("nested").join("child");
        ensure_private_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "must not be group/world-traversable");
        // idempotent: calling again on an existing dir must not error
        ensure_private_dir(&dir).unwrap();
        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
    }

    fn tmpdir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let salt = &counter as *const u64 as usize;
        let unique = short_hash(&format!(
            "{}-{nanos}-{counter}-{salt:x}",
            std::process::id()
        ));
        std::env::temp_dir().join(format!("hm-util-{tag}-{unique}"))
    }
}
