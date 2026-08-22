// Shared plumbing: error alias, environment/path resolution, logging.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

pub fn err(msg: impl Into<String>) -> Error {
    msg.into().into()
}

pub fn home_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(user_profile) = std::env::var("USERPROFILE") {
            if !user_profile.is_empty() {
                return PathBuf::from(user_profile);
            }
        }
        if let (Ok(drive), Ok(path)) = (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
            return PathBuf::from(format!("{drive}{path}"));
        }
    }
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
    #[cfg(windows)]
    {
        home_dir().join(".local").join("bin").join("herdr-mirror.exe")
    }
    #[cfg(not(windows))]
    {
        home_dir().join(".local").join("bin").join("herdr-mirror")
    }
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

    #[cfg(unix)]
    {
        Some(match std::os::unix::fs::symlink(&exe, &link) {
            Ok(()) => format!("relinked {} -> {}", link.display(), exe.display()),
            Err(e) => format!("could not relink {}: {e}", link.display()),
        })
    }
    #[cfg(windows)]
    {
        Some(match std::os::windows::fs::symlink_file(&exe, &link) {
            Ok(()) => format!("linked {} -> {}", link.display(), exe.display()),
            Err(_) => match fs::copy(&exe, &link) {
                Ok(_) => format!("copied {} -> {}", exe.display(), link.display()),
                Err(e) => format!("could not link or copy {}: {e}", link.display()),
            },
        })
    }
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
    if pid <= 0 {
        return false;
    }
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid, 0) == 0 }
    }
    #[cfg(windows)]
    {
        let status = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output();
        match status {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                stdout.contains(&pid.to_string())
            }
            Err(_) => false,
        }
    }
}

/// Pidfile a pane streamer writes at startup. The daemon starts streamers by
/// TYPING `exec ...` into a fresh shell pane, and interactive shell startup
/// can eat keystrokes (oh-my-zsh's update prompt swallows the leading `e` —
/// and re-prompts in every new shell until answered, so it fails every spawn,
/// not one in a fortnight). The pidfile is how the daemon can tell the exec
/// took, and retype it when it didn't.
pub fn streamer_pid_path(state_dir: &Path, ssh_target: &str, pane_target: &str) -> PathBuf {
    let sane = |s: &str| {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>()
    };
    state_dir.join("streamer-pids").join(format!("{}--{}.pid", sane(ssh_target), sane(pane_target)))
}

pub fn streamer_alive(state_dir: &Path, ssh_target: &str, pane_target: &str) -> bool {
    fs::read_to_string(streamer_pid_path(state_dir, ssh_target, pane_target))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .is_some_and(pid_alive)
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
