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
    dirs.dedup();
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
