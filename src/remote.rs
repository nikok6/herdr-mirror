// ssh transport for the DAEMON's own traffic (remote CLI execs + API-socket
// forward) over one ControlMaster per host. Pane streams deliberately use
// their own direct connections instead (see pane.rs).

use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;
use tokio::time::timeout;

use crate::api::ApiClient;
use crate::config::HostConfig;
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

async fn ssh(args: &[String], timeout_ms: u64) -> SshOutput {
    let fut = Command::new("ssh")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match timeout(Duration::from_millis(timeout_ms), fut).await {
        Ok(Ok(o)) => SshOutput {
            code: o.status.code().unwrap_or(1),
            out: String::from_utf8_lossy(&o.stdout).into_owned(),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Ok(Err(e)) => SshOutput { code: 1, out: String::new(), err: e.to_string() },
        Err(_) => SshOutput { code: 1, out: String::new(), err: "ssh timeout".into() },
    }
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
    log: Logger,
}

impl RemoteHost {
    pub fn new(cfg: &HostConfig, state_dir: &std::path::Path) -> RemoteHost {
        RemoteHost {
            ctl_path: state_dir.join(format!("{}.ctl", cfg.name)),
            fwd_sock: state_dir.join(format!("{}-api.sock", cfg.name)),
            cfg: cfg.clone(),
            forwarded: false,
            container: None,
            relay: None,
            log: Logger::new(state_dir, false),
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
            return Err(err(format!("{DORMANT}: no running container for {}", self.cfg.target)));
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
        self.container = Some(crate::docker::Container { id, docker_bin: bin });
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
        let mut check = self.base_args();
        check.extend(["-O".into(), "check".into(), self.cfg.target.clone()]);
        if ssh(&check, 15000).await.code == 0 {
            return Ok(());
        }
        self.forwarded = false;
        // OpenSSH falls back to a standalone connection when ControlPath exists
        // but no master is listening. With -f -N that silently leaks one process
        // per retry, while every later -O command keeps targeting the dead socket.
        remove_stale_control_socket(&self.ctl_path)?;
        let mut start: Vec<String> = vec![
            "-M".into(),
            "-S".into(),
            self.ctl_path.display().to_string(),
        ];
        start.extend(SSH_COMMON_OPTS.iter().map(|s| s.to_string()));
        start.extend([
            "-o".into(),
            "ControlPersist=yes".into(),
            "-f".into(),
            "-N".into(),
            self.cfg.target.clone(),
        ]);
        let res = ssh(&start, 20000).await;
        if res.code != 0 {
            return Err(err(format!(
                "ssh master to {} failed: {}",
                self.cfg.target,
                nonempty(&res.err, res.code)
            )));
        }
        let verified = ssh(&check, 15000).await;
        if verified.code != 0 {
            return Err(err(format!(
                "ssh master to {} did not create a usable control socket: {}",
                self.cfg.target,
                nonempty(&verified.err, verified.code)
            )));
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
        let out = self.exec(&format!("exec {} status --json", self.cfg.remote_bin), 15000).await?;
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
        let mut status = RemoteStatus { socket, supported: false, reason: None };
        if !running {
            status.reason = Some("remote herdr server is not running".into());
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
        cancel.extend(["-O".into(), "cancel".into(), "-L".into(), spec.clone(), self.cfg.target.clone()]);
        let _ = ssh(&cancel, 15000).await;
        let _ = std::fs::remove_file(&self.fwd_sock);
        let mut fwd = self.base_args();
        fwd.extend(["-O".into(), "forward".into(), "-L".into(), spec, self.cfg.target.clone()]);
        let res = ssh(&fwd, 15000).await;
        if res.code != 0 {
            return Err(err(format!("ssh socket forward failed: {}", nonempty(&res.err, res.code))));
        }
        self.forwarded = true;
        Ok(self.fwd_sock.clone())
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
            return Err(err(status.reason.clone().unwrap_or_else(|| "remote unsupported".into())));
        }
        let sock = match &self.container {
            None => self.forward_api(&status.socket).await?,
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
fn version_supported(version: &str) -> Option<bool> {
    let core = version.split(['-', '+']).next()?;
    let mut it = core.split('.');
    let maj: u64 = it.next()?.parse().ok()?;
    let min: u64 = it.next()?.parse().ok()?;
    let pat: u64 = it.next()?.parse().ok()?;
    let newer_than_base = maj > 0 || min > 7 || (min == 7 && pat > 1);
    // preview builds look like 0.7.1-preview.2026-06-30-<hash>
    let preview_ok = version
        .split_once("-preview.")
        .map(|(_, rest)| rest.get(0..10).map(|d| d >= MIN_PREVIEW_BUILD).unwrap_or(false))
        .unwrap_or(false);
    Some(newer_than_base || preview_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "herdr-mirror-{name}-{}-{nonce}",
            std::process::id()
        ))
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
        assert_eq!(version_supported("0.7.1-preview.2026-06-30-3459798b606d"), Some(true));
        assert_eq!(version_supported("0.7.1-preview.2026-07-04-aaaa"), Some(true));
        assert_eq!(version_supported("0.7.1-preview.2026-06-29-aaaa"), Some(false));
        assert_eq!(version_supported("garbage"), None);
    }
}
