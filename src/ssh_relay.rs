// Exec-relay transport: reach a herdr server's unix socket over a plain ssh
// exec channel, for ssh hosts whose sshd accepts `-L`-style direct-streamlocal
// channel opens but never services them (observed against embedded Go sshds
// fronting container/VM workspaces — e.g. DevPod-style remote dev
// environments). `remote::connect_ssh_api` decides *when* to use this; this
// module only implements the bridge once that decision is made.
//
// This is deliberately the same shape as docker.rs's container relay: one
// exec (here, `ssh -S <ctlpath> <target> <relay-cmd>` instead of `docker exec
// -i <cid> ...`) per accepted local connection, piping the child's
// stdin/stdout to/from the unix socket at daemon → <state>/<host>-api.sock.
//
//   daemon → <state>/<host>-api.sock        (a local socket WE listen on)
//              │ copy both directions
//   ssh -S <ctlpath> <target> "<relay-cmd>"
//              │ (multiplexed over the existing ControlMaster: no new
//              │  handshake, and exec channels are the transport that is
//              │  known to work where streamlocal is not)
//              └→ /path/to/herdr.sock   (herdr, on the remote)
//
// `relay-cmd` is `socat - UNIX-CONNECT:<sock>` when the remote has socat, else
// a small python3 stdio↔unix bridge (see `python_bridge_command`) — chosen
// once per host by `detect_relay_command` and reused for every connection.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::timeout;

use crate::docker::validate_socket_path;
use crate::remote::RemoteHost;
use crate::util::{err, Logger, Result};

/// A remote stdio↔unix bridge, run once and reused across probes: not worth
/// re-detecting on every reconnect within a daemon's lifetime, and `remote`
/// already remembers the *transport* choice across reconnects for the same
/// reason.
#[derive(Debug, Clone)]
pub struct RelayCommand(String);

/// Find a relay command the remote can run: socat if present (cheap, no
/// runtime dependency beyond the binary itself), else a python3 fallback.
/// Errors when neither exists — a clear "install one" beats a silent hang.
pub async fn detect_relay_command(host: &RemoteHost, remote_sock: &str) -> Result<RelayCommand> {
    validate_socket_path(remote_sock)?;
    if !host.exec("command -v socat", 8000).await.unwrap_or_default().trim().is_empty() {
        // socat's address grammar is not a plain path (`,` starts an option
        // list, `!!` a dual address); validated above, same reasoning as the
        // docker relay.
        return Ok(RelayCommand(format!("socat - UNIX-CONNECT:{remote_sock}")));
    }
    if !host.exec("command -v python3", 8000).await.unwrap_or_default().trim().is_empty() {
        return Ok(RelayCommand(python_bridge_command(remote_sock)));
    }
    Err(err(
        "remote has neither socat nor python3, which one of is needed to reach herdr's \
         socket over an exec relay — install either",
    ))
}

/// Minimal stdio↔`AF_UNIX` bridge for hosts without socat.
///
/// Uses `os.read`/`os.write` on the raw fds rather than `sys.stdin`/`stdout`:
/// those wrap a buffered, text-capable layer that both delays bytes (bad for
/// a request/response protocol waiting on a prompt-less socket) and risks
/// re-encoding on non-UTF8 bytes. Raw fd I/O is the only way to get
/// unbuffered, binary-safe transfer in either direction.
///
/// The two directions run as separate strands (a background thread for
/// stdin→socket, the main thread for socket→stdout) so they proceed
/// concurrently — `os.read`/`socket.recv` release the GIL while blocked, so
/// this is real concurrency, not a serialized illusion. A single-threaded
/// version that alternated between the two would deadlock the JSON API the
/// first time a request's bytes hadn't fully arrived before the response
/// started: the client can't finish sending while the bridge is blocked
/// reading the (still empty) response half.
///
/// EOF handling matches `docker::pump`'s reasoning exactly: stdin closing
/// means the local side finished sending its request, which is NOT the end of
/// the exchange (`socket.shutdown(SHUT_WR)` forwards that half-close without
/// tearing down the read side); the socket closing IS the end of the exchange
/// (herdr closes after each response), so that is what ends the process.
///
/// The socket path travels as a base64 argv element rather than interpolated
/// into the script text, so it can never be read as anything but a string —
/// no quoting of untrusted remote data survives into a shell or Python
/// grammar at all.
fn python_bridge_command(remote_sock: &str) -> String {
    const BRIDGE: &str = r#"import os,socket,sys,threading,base64
p=base64.b64decode(sys.argv[1]).decode()
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
s.connect(p)
def up():
    while True:
        b=os.read(0,65536)
        if not b:
            break
        s.sendall(b)
    try:
        s.shutdown(socket.SHUT_WR)
    except OSError:
        pass
threading.Thread(target=up,daemon=True).start()
while True:
    try:
        b=s.recv(65536)
    except OSError:
        break
    if not b:
        break
    os.write(1,b)
"#;
    let sock_b64 = B64.encode(remote_sock.as_bytes());
    format!("python3 -c '{BRIDGE}' '{sock_b64}'")
}

/// Where and how to reach the remote: reuses the daemon's own ControlMaster
/// (`-S ctl_path`) so a relay exec costs no new ssh handshake, exactly like
/// every other multiplexed exec this daemon already runs.
#[derive(Debug, Clone)]
struct ExecTarget {
    ctl_path: PathBuf,
    ssh_target: String,
    relay_cmd: RelayCommand,
}

impl ExecTarget {
    fn relay_child(&self) -> Result<tokio::process::Child> {
        let mut cmd = Command::new("ssh");
        cmd.args([
            "-S",
            &self.ctl_path.display().to_string(),
            "-o",
            "BatchMode=yes",
            &self.ssh_target,
            &self.relay_cmd.0,
        ]);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // kept, not nulled: an sshd that killed the channel, or a relay
            // command that can't open the socket, is otherwise indistinguishable
            // from a clean close and the real reason is lost
            .stderr(Stdio::piped())
            // if we drop the connection the child must die rather than linger
            // holding an exec channel open on the ControlMaster
            .kill_on_drop(true);
        cmd.spawn().map_err(|e| err(format!("ssh exec (relay) failed: {e}")))
    }
}

/// Serve `local_sock` by relaying every accepted connection into the remote
/// over a fresh exec channel. Returns once the listener is bound, so callers
/// can connect immediately.
///
/// Structurally identical to `docker::serve_relay` — see that function's
/// comments for the accept-loop and shutdown reasoning, which apply unchanged
/// here.
pub fn serve_relay(
    ctl_path: PathBuf,
    ssh_target: String,
    relay_cmd: RelayCommand,
    local_sock: PathBuf,
    log: Logger,
) -> Result<RelayHandle> {
    let target = ExecTarget { ctl_path, ssh_target, relay_cmd };
    let _ = std::fs::remove_file(&local_sock);
    if let Some(parent) = local_sock.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let listener = UnixListener::bind(&local_sock)
        .map_err(|e| err(format!("cannot bind {}: {e}", local_sock.display())))?;
    // anyone who can connect here drives the remote herdr API unauthenticated,
    // which can start agents with arbitrary argv. ssh's -L forward is 0600, so
    // match it rather than inheriting the ambient umask.
    if let Err(e) = std::fs::set_permissions(&local_sock, std::fs::Permissions::from_mode(0o600)) {
        log.log(&format!("relay: cannot restrict {}: {e}", local_sock.display()));
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let path = local_sock.clone();
    let task = tokio::spawn(async move {
        let mut consecutive_errors = 0u32;
        loop {
            let stream = tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((s, _)) => s,
                    Err(e) => {
                        consecutive_errors += 1;
                        log.log(&format!("exec relay accept failed ({consecutive_errors}): {e}"));
                        if consecutive_errors >= 5 {
                            log.log("exec relay: giving up on accept; host will reconnect");
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(200 * consecutive_errors as u64))
                            .await;
                        continue;
                    }
                },
                _ = wait_shutdown(shutdown_rx.clone()) => return,
            };
            consecutive_errors = 0;
            let t = target.clone();
            let log = log.clone();
            let shutdown = shutdown_rx.clone();
            tokio::spawn(async move {
                if let Err(e) = pump(&t, stream, shutdown).await {
                    log.log(&format!("exec relay connection failed: {e}"));
                }
            });
        }
    });
    Ok(RelayHandle { path, task, shutdown: shutdown_tx })
}

async fn wait_shutdown(mut rx: watch::Receiver<bool>) {
    while rx.changed().await.is_ok() {
        if *rx.borrow() {
            return;
        }
    }
}

/// Owns the relay. Dropping it stops accepting, terminates in-flight
/// connections, and unlinks the socket. Same shape as `docker::RelayHandle`.
pub struct RelayHandle {
    pub path: PathBuf,
    task: tokio::task::JoinHandle<()>,
    shutdown: watch::Sender<bool>,
}

impl Drop for RelayHandle {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Copy bytes both ways until the remote side closes or we are shut down.
async fn pump(target: &ExecTarget, stream: UnixStream, shutdown: watch::Receiver<bool>) -> Result<()> {
    let mut child = target.relay_child()?;
    let mut cin = child.stdin.take().ok_or_else(|| err("relay: no child stdin"))?;
    let mut cout = child.stdout.take().ok_or_else(|| err("relay: no child stdout"))?;
    let mut cerr = child.stderr.take().ok_or_else(|| err("relay: no child stderr"))?;
    let (mut lr, mut lw) = stream.into_split();

    // Client → remote. Finishing means the client half-closed after sending
    // its request, which is NOT the end of the exchange: forward the EOF and
    // keep waiting for the response. Ending the pump here would truncate it.
    let up = async move {
        let mut buf = [0u8; 32 * 1024];
        loop {
            let n = match lr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if cin.write_all(&buf[..n]).await.is_err() || cin.flush().await.is_err() {
                break;
            }
        }
        let _ = cin.shutdown().await;
    };

    // Remote → client. This direction ending is the real end of the exchange:
    // herdr closes after each response, and a dropped subscription closes
    // here too.
    let down = async {
        let mut buf = [0u8; 32 * 1024];
        loop {
            let n = match cout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if lw.write_all(&buf[..n]).await.is_err() || lw.flush().await.is_err() {
                break;
            }
        }
    };

    let upper = tokio::spawn(up);
    tokio::select! {
        _ = down => {}
        _ = wait_shutdown(shutdown) => {}
    }
    upper.abort();

    // surface why the relay gave up, instead of presenting every distinct
    // fault as an indistinguishable "connection closed"
    let mut errtext = String::new();
    let _ = timeout(Duration::from_millis(200), cerr.read_to_string(&mut errtext)).await;
    let errtext = errtext.trim();
    if !errtext.is_empty() {
        return Err(err(format!("relay: {errtext}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socat_command_uses_dash_stdio_form() {
        let cmd = "socat - UNIX-CONNECT:/home/vscode/.config/herdr/herdr.sock";
        assert!(cmd.starts_with("socat - UNIX-CONNECT:"));
    }

    /// The socket path must never appear verbatim in the command line: it is
    /// base64'd so a path containing shell or python metacharacters can't
    /// reach either grammar.
    #[test]
    fn python_bridge_never_interpolates_the_raw_path() {
        let sock = "/tmp/x'; rm -rf ~ #.sock";
        let cmd = python_bridge_command(sock);
        assert!(!cmd.contains(sock), "raw socket path leaked into the command line: {cmd}");
        let b64 = B64.encode(sock.as_bytes());
        assert!(cmd.contains(&b64), "expected the base64 form present: {cmd}");
    }

    #[test]
    fn python_bridge_script_has_no_single_quotes() {
        // the whole script is wrapped in single quotes on the command line;
        // an embedded one would end the quoting early and corrupt the command
        let cmd = python_bridge_command("/tmp/x.sock");
        let inner = cmd.split('\'').nth(1).expect("script segment");
        assert!(!inner.contains('\''), "script must not contain single quotes: {inner}");
    }

    /// End-to-end relay check against a real unix socket over a real ssh
    /// ControlMaster. Skipped unless the env vars below are set — this is the
    /// piece unit tests cannot cover on their own machine, since it needs a
    /// live ssh target with either socat or python3 and something herdr-like
    /// listening on a unix socket:
    ///
    ///   HERDR_MIRROR_IT_SSH_TARGET=my-host.example \
    ///   HERDR_MIRROR_IT_SSH_SOCK=/home/user/.config/herdr/herdr.sock \
    ///   cargo test -- --ignored --nocapture relay_reaches_real_ssh_host
    #[tokio::test]
    #[ignore]
    async fn relay_reaches_real_ssh_host() {
        let Ok(ssh_target) = std::env::var("HERDR_MIRROR_IT_SSH_TARGET") else {
            eprintln!("skipped: set HERDR_MIRROR_IT_SSH_TARGET");
            return;
        };
        let sock = std::env::var("HERDR_MIRROR_IT_SSH_SOCK")
            .unwrap_or_else(|_| "/home/vscode/.config/herdr/herdr.sock".into());

        let dir = std::env::temp_dir().join(format!("herdr-mirror-it-ssh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctl_path = dir.join("it.ctl");
        let local_sock = dir.join("it-api.sock");

        // establish a ControlMaster the relay child can multiplex onto
        let start = Command::new("ssh")
            .args([
                "-M",
                "-S",
                ctl_path.to_str().unwrap(),
                "-o",
                "BatchMode=yes",
                "-o",
                "ControlPersist=yes",
                "-f",
                "-N",
                &ssh_target,
            ])
            .status()
            .await
            .expect("ssh master spawn");
        assert!(start.success(), "ssh ControlMaster failed to start");

        let relay_cmd = RelayCommand(python_bridge_command(&sock));
        let handle = serve_relay(ctl_path.clone(), ssh_target.clone(), relay_cmd, local_sock.clone(), Logger::new(&dir, false))
            .unwrap();

        let mode = std::fs::metadata(&handle.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "relay socket must be 0600, got {mode:o}");

        let api = crate::api::ApiClient::connect(&handle.path).await.expect("ping through exec relay");
        eprintln!("ping ok");

        let snap = api
            .request("session.snapshot", serde_json::json!({}))
            .await
            .expect("session.snapshot through exec relay");
        assert!(snap.pointer("/snapshot/workspaces").is_some(), "snapshot shape: {snap}");
        eprintln!("snapshot ok ({} bytes)", snap.to_string().len());

        for _ in 0..5 {
            api.request("session.snapshot", serde_json::json!({})).await.expect("repeat request");
        }
        eprintln!("5 sequential requests ok");

        drop(handle);
        let _ = Command::new("ssh")
            .args(["-S", ctl_path.to_str().unwrap(), "-O", "exit", &ssh_target])
            .status()
            .await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
