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

/// Resolved runtime environment. Config prefers the plugin dir if hosts.toml
/// lives there; state is ALWAYS the fixed path so shell and plugin-action
/// invocations share one id map and pidfile.
pub struct Env {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub local_socket: PathBuf,
    pub plugin_root: PathBuf,
}

/// HERDR_PLUGIN_ROOT, else walk up from the binary to the manifest. Only used
/// as the mirror panes' cwd.
fn resolve_plugin_root() -> PathBuf {
    if let Ok(root) = std::env::var("HERDR_PLUGIN_ROOT") {
        if !root.is_empty() {
            return PathBuf::from(root);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        for dir in exe.ancestors().skip(1) {
            if dir.join("herdr-plugin.toml").exists() {
                return dir.to_path_buf();
            }
        }
    }
    home_dir()
}

impl Env {
    pub fn resolve() -> Result<Env> {
        let fallback_config = home_dir().join(".config").join("herdr-mirror");
        let config_dir = match std::env::var("HERDR_PLUGIN_CONFIG_DIR") {
            Ok(dir) if Path::new(&dir).join("hosts.toml").exists() => PathBuf::from(dir),
            _ => fallback_config,
        };
        let state_dir = home_dir().join(".local").join("state").join("herdr-mirror");
        fs::create_dir_all(&config_dir)?;
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
        Ok(Env { config_dir, state_dir, local_socket, plugin_root: resolve_plugin_root() })
    }
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
