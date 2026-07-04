// ssh transport for the DAEMON's own traffic (remote CLI execs + API-socket
// forward) over one ControlMaster per host. Pane streams deliberately use
// their own direct connections instead (see pane.rs).

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;
use tokio::time::timeout;

use crate::api::ApiClient;
use crate::config::HostConfig;
use crate::util::{err, Result};

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

pub struct RemoteHost {
    pub cfg: HostConfig,
    ctl_path: PathBuf,
    pub fwd_sock: PathBuf,
    forwarded: bool,
}

impl RemoteHost {
    pub fn new(cfg: &HostConfig, state_dir: &std::path::Path) -> RemoteHost {
        RemoteHost {
            ctl_path: state_dir.join(format!("{}.ctl", cfg.name)),
            fwd_sock: state_dir.join(format!("{}-api.sock", cfg.name)),
            cfg: cfg.clone(),
            forwarded: false,
        }
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
        let mut start: Vec<String> =
            vec!["-M".into(), "-S".into(), self.ctl_path.display().to_string()];
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
        Ok(())
    }

    pub async fn exec(&self, command: &str, timeout_ms: u64) -> Result<String> {
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
        self.ensure_master().await?;
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
        let sock = self.forward_api(&status.socket).await?;
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
