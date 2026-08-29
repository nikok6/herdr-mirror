// Shared plumbing: error alias, environment/path resolution, logging.

use std::fs;
use std::io::Write;
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
        Ok(Env { config_search, state_dir, local_socket })
    }
}

/// Canonical config dir: the one path both plugin actions and shell
/// invocations can always reach, so it's what we create and what docs name.
pub fn default_config_dir() -> PathBuf {
    home_dir().join(".config").join("herdr-mirror")
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
            let exe = std::env::current_exe().ok().and_then(|e| fs::canonicalize(e).ok());
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
        CliLink::Dangling(t) => {
            Some(format!("{} dangles (-> {})", cli_link_path().display(), t.display()))
        }
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
    let exe = std::env::current_exe().ok()?;
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
        Logger { file: state_dir.join("daemon.log"), also_stdout }
    }

    pub fn log(&self, msg: &str) {
        let line = format!("{} {}\n", now_iso(), msg);
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&self.file) {
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
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

pub fn streamer_pid_path(state_dir: &Path, ssh_target: &str, pane_target: &str) -> PathBuf {
    state_dir
        .join("streamer-pids")
        .join(format!("{}--{}.pid", sane_component(ssh_target), sane_component(pane_target)))
}

/// A launch claim exists from the first local `pane.send_text` until the
/// streamer writes its pidfile. Recovery must not type another command into
/// that pane during this interval: the first streamer can already own stdin
/// even when herdr's process snapshot still reports the shell.
pub fn streamer_spawn_pending_path(state_dir: &Path, local_pane_id: &str) -> PathBuf {
    state_dir.join("streamer-spawns").join(format!("{}.pending", sane_component(local_pane_id)))
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
    state_dir.join("pane-pids").join(format!("{}.pid", sane_component(local_pane_id)))
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
        let mut claim = match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
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
        let times = libc::utimbuf { actime: past, modtime: past };
        assert_eq!(unsafe { libc::utime(cpath.as_ptr(), &times) }, 0);

        assert_eq!(
            claim_streamer_spawn(&state_dir, "host", "w1:p1", "w2:p2").unwrap(),
            StreamerSpawnClaim::Claimed
        );
        let _ = fs::remove_dir_all(state_dir);
    }
}
