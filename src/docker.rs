// Container transport: reach a herdr server running inside a docker container.
//
// ssh gets `-L` for free — it forwards a remote unix socket to a local one, and
// that is what `remote::forward_api` uses. Docker has no equivalent: the only
// channel `docker exec` offers is a child process with stdin/stdout pipes. So
// the API socket is bridged by hand:
//
//   daemon → <state>/<host>-api.sock        (a local socket WE listen on)
//              │ copy both directions
//   docker exec -i <cid> socat - UNIX-CONNECT:<remote sock>
//              │
//              └→ /root/.config/herdr/herdr.sock   (herdr, inside the container)
//
// One `docker exec` per accepted connection, because `ApiClient` opens a fresh
// connection per request (the server closes after each response, see api.rs).
// The held `events.subscribe` stream is a single long-lived exec, so the
// high-frequency path costs nothing extra.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::timeout;

use crate::config::HostKind;
use crate::util::{err, Logger, Result};

/// Handle to a running container. Cheap to clone; holds no processes.
#[derive(Debug, Clone)]
pub struct Container {
    pub id: String,
    pub docker_bin: String,
}

fn docker_filter(kind: &HostKind) -> Result<String> {
    match kind {
        HostKind::Ssh => Err(err("container resolution called on an ssh host")),
        // `name=` is a substring match in docker, so pin it with anchors to
        // avoid the same class of collision that broke streamer counting
        HostKind::DockerContainer(name) => Ok(format!("name=^{name}$")),
        HostKind::DockerFolder(folder) => {
            Ok(format!("label=devcontainer.local_folder={folder}"))
        }
    }
}

/// Turn a finished `docker` invocation into ids, keeping the failure reason.
///
/// Distinguishing "docker failed" from "nothing matched" matters: reporting a
/// stopped Docker daemon as "no running container" sends the user off to start
/// a container that is already there.
fn parse_ps(ok: bool, stdout: &[u8], stderr: &[u8]) -> Result<Vec<String>> {
    if !ok {
        let e = String::from_utf8_lossy(stderr);
        let e = e.trim();
        return Err(err(format!(
            "docker ps failed: {}",
            if e.is_empty() { "non-zero exit" } else { e }
        )));
    }
    Ok(String::from_utf8_lossy(stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// Resolve container ids for a host, newest first.
///
/// An empty result means "nothing running" — dormant, not an error. A stopped
/// devcontainer is the resting state, unlike an unreachable ssh host. Errors
/// are reserved for docker itself failing.
pub async fn resolve(docker_bin: &str, kind: &HostKind) -> Result<Vec<String>> {
    let filter = docker_filter(kind)?;
    let fut = Command::new(docker_bin)
        .args(["ps", "-q", "--filter", &filter])
        .stdin(Stdio::null())
        .output();
    let out = timeout(Duration::from_secs(10), fut)
        .await
        .map_err(|_| err("docker ps timed out"))?
        .map_err(|e| err(format!("cannot run {docker_bin}: {e}")))?;
    parse_ps(out.status.success(), &out.stdout, &out.stderr)
}

/// Blocking twin of [`resolve`], for the pane wrapper.
///
/// The pane process spawns its stream from sync code, and re-resolving on every
/// (re)spawn is what lets a pane survive a container rebuild: the id changes,
/// the folder label does not.
///
/// The deadline is not optional. The pane runs a single-threaded runtime, so an
/// unbounded wait here blocks input, rendering and signal handling — a wedged
/// Docker daemon would otherwise make the pane unkillable.
pub fn resolve_blocking(docker_bin: &str, kind: &HostKind, wait: Duration) -> Result<String> {
    let filter = docker_filter(kind)?;
    let mut child = std::process::Command::new(docker_bin)
        .args(["ps", "-q", "--filter", &filter])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| err(format!("cannot run {docker_bin}: {e}")))?;
    let deadline = Instant::now() + wait;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = child.wait_with_output().map_err(|e| err(format!("docker ps: {e}")))?;
                let ids = parse_ps(status.success(), &out.stdout, &out.stderr)?;
                return ids
                    .into_iter()
                    .next()
                    .ok_or_else(|| err(format!("no running container matching {filter}")));
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err("docker ps timed out"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(err(format!("docker ps: {e}"))),
        }
    }
}

/// socat is the only supported in-container bridge.
///
/// We cannot ship our own: the daemon is built for the host (macOS/arm64) and
/// the container is Linux, so there is no binary to `docker cp` in. A python
/// fallback existed briefly and was removed — it was a second language embedded
/// as a string, never exercised by any test, and leaked a process plus a
/// server-side subscription on every resubscribe. A clear error beats a
/// silently leaking fallback.
pub async fn probe_socat(docker_bin: &str, cid: &str) -> Result<()> {
    let fut = Command::new(docker_bin)
        .args(["exec", cid, "sh", "-c", "command -v socat"])
        .stdin(Stdio::null())
        .output();
    let out = timeout(Duration::from_secs(10), fut)
        .await
        .map_err(|_| err("probing for socat timed out"))?
        .map_err(|e| err(format!("cannot run {docker_bin}: {e}")))?;
    if out.status.success() && !out.stdout.is_empty() {
        return Ok(());
    }
    Err(err("container has no socat, which is needed to reach herdr's socket — \
             install it in the image, or bind-mount the socket to the host"))
}

/// socat's address grammar is not a plain path: `,` starts an option list and
/// `!!` separates a dual address, so `UNIX-CONNECT:<path>` with an unvetted
/// path could be steered into an entirely different address type.
///
/// The path comes from `herdr status --json` *inside the container*, which is
/// the less-trusted side of the boundary, so it is validated rather than
/// trusted.
fn validate_socket_path(sock: &str) -> Result<()> {
    if !sock.starts_with('/') {
        return Err(err(format!("remote socket path is not absolute: {sock}")));
    }
    if sock.contains(',') || sock.contains("!!") {
        return Err(err(format!("remote socket path has socat metacharacters: {sock}")));
    }
    Ok(())
}

impl Container {
    /// Run a command inside the container and capture stdout.
    ///
    /// `sh -c`, not `sh -lc`: `ssh host cmd` runs a *non-login* shell, so it
    /// never sources `/etc/profile*`. Matching that matters because the output
    /// is parsed as JSON — an image whose profile prints a welcome banner would
    /// otherwise make every connect fail with "expected value at line 1
    /// column 1". Tilde expansion in `remote_bin` works either way.
    pub async fn exec(&self, command: &str, timeout_ms: u64) -> Result<String> {
        let fut = Command::new(&self.docker_bin)
            .args(["exec", &self.id, "sh", "-c", command])
            .stdin(Stdio::null())
            .output();
        let out = timeout(Duration::from_millis(timeout_ms), fut)
            .await
            .map_err(|_| err(format!("docker exec timed out: {command}")))?
            .map_err(|e| err(format!("cannot run {}: {e}", self.docker_bin)))?;
        if !out.status.success() {
            let e = String::from_utf8_lossy(&out.stderr);
            let e = e.trim();
            return Err(err(format!(
                "docker exec failed ({command}): {}",
                if e.is_empty() { "non-zero exit" } else { e }
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// One `docker exec` bridging a single connection to the container socket.
    fn relay_child(&self, remote_sock: &str) -> Result<tokio::process::Child> {
        let mut cmd = Command::new(&self.docker_bin);
        cmd.args(["exec", "-i", &self.id, "socat", "-", &format!("UNIX-CONNECT:{remote_sock}")]);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // kept, not nulled: a container that died, or a socat that cannot
            // open the socket, is otherwise indistinguishable from a clean
            // close and the real reason is lost
            .stderr(Stdio::piped())
            // if we drop the connection the child must die rather than linger
            // holding the container socket open
            .kill_on_drop(true);
        cmd.spawn().map_err(|e| err(format!("docker exec (relay) failed: {e}")))
    }
}

/// Serve `local_sock` by relaying every accepted connection into the container.
///
/// Returns once the listener is bound, so callers can connect immediately.
pub fn serve_relay(
    container: Container,
    remote_sock: String,
    local_sock: PathBuf,
    log: Logger,
) -> Result<RelayHandle> {
    validate_socket_path(&remote_sock)?;
    // a previous daemon (or a crash) leaves the socket file behind; bind fails
    // on an existing path even when nothing is listening. Callers must only
    // reach here after establishing that nothing live owns it.
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
        // a sticky accept error (EMFILE/ENFILE is the realistic one, since each
        // request costs a docker exec with three pipes) would otherwise spin a
        // tight loop that also blocks a worker thread on synchronous log I/O
        let mut consecutive_errors = 0u32;
        loop {
            let stream = tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((s, _)) => s,
                    Err(e) => {
                        consecutive_errors += 1;
                        log.log(&format!("relay accept failed ({consecutive_errors}): {e}"));
                        if consecutive_errors >= 5 {
                            log.log("relay: giving up on accept; host will reconnect");
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
            let c = container.clone();
            let sock = remote_sock.clone();
            let log = log.clone();
            let shutdown = shutdown_rx.clone();
            tokio::spawn(async move {
                if let Err(e) = pump(&c, &sock, stream, shutdown).await {
                    log.log(&format!("relay connection failed: {e}"));
                }
            });
        }
    });
    Ok(RelayHandle { path, task, shutdown: shutdown_tx })
}

async fn wait_shutdown(mut rx: watch::Receiver<bool>) {
    // returns as soon as the handle is dropped (or told to stop)
    while rx.changed().await.is_ok() {
        if *rx.borrow() {
            return;
        }
    }
}

/// Owns the relay. Dropping it stops accepting, terminates in-flight
/// connections, and unlinks the socket.
///
/// The shutdown channel is what makes that true: per-connection tasks are
/// detached, so aborting the accept task alone would leave a held
/// `events.subscribe` pump (and its `docker exec`) running with no owner.
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

/// Copy bytes both ways until the container side closes or we are shut down.
async fn pump(
    container: &Container,
    remote_sock: &str,
    stream: UnixStream,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut child = container.relay_child(remote_sock)?;
    let mut cin = child.stdin.take().ok_or_else(|| err("relay: no child stdin"))?;
    let mut cout = child.stdout.take().ok_or_else(|| err("relay: no child stdout"))?;
    let mut cerr = child.stderr.take().ok_or_else(|| err("relay: no child stderr"))?;
    let (mut lr, mut lw) = stream.into_split();

    // Client → container. Finishing means the client half-closed after sending
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

    // Container → client. This direction ending is the real end of the
    // exchange: herdr closes after each response, and a dropped subscription
    // closes here too.
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

    // surface why socat gave up, instead of presenting every distinct fault as
    // an indistinguishable "connection closed"
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
    fn filters_are_anchored_or_labelled() {
        assert_eq!(
            docker_filter(&HostKind::DockerContainer("dev".into())).unwrap(),
            "name=^dev$",
            "unanchored would substring-match dev-staging"
        );
        assert_eq!(
            docker_filter(&HostKind::DockerFolder("/p".into())).unwrap(),
            "label=devcontainer.local_folder=/p"
        );
        assert!(docker_filter(&HostKind::Ssh).is_err());
    }

    /// A failed `docker ps` must not read as "nothing is running": that sends
    /// the user to start a container that is already there.
    #[test]
    fn docker_failure_is_distinct_from_no_match() {
        let e = parse_ps(false, b"", b"Cannot connect to the Docker daemon").unwrap_err();
        assert!(e.to_string().contains("Cannot connect"), "{e}");
        assert_eq!(parse_ps(true, b"", b"").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn parse_ps_returns_all_matches_newest_first() {
        let ids = parse_ps(true, b"aaa\nbbb\n", b"").unwrap();
        assert_eq!(ids, ["aaa", "bbb"]);
    }

    /// The socket path comes from the remote herdr, so it is validated before
    /// being spliced into socat's address grammar.
    #[test]
    fn rejects_socat_metacharacters_in_socket_path() {
        assert!(validate_socket_path("/root/.config/herdr/herdr.sock").is_ok());
        assert!(validate_socket_path("/tmp/x!!EXEC:/bin/sh").is_err(), "dual-address");
        assert!(validate_socket_path("/tmp/x,fork").is_err(), "option list");
        assert!(validate_socket_path("relative/path.sock").is_err(), "not absolute");
    }

    #[test]
    fn resolve_blocking_times_out_rather_than_hanging() {
        // A stub that ignores its args and hangs, standing in for a wedged
        // Docker daemon. The pane runs a single-threaded runtime, so an
        // unbounded wait here would freeze input, rendering and signal
        // handling — the pane would stop responding to SIGHUP and outlive its
        // herdr pane.
        let dir = std::env::temp_dir().join(format!("hm-slowdocker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("slow-docker");
        std::fs::write(&stub, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let start = Instant::now();
        let e = resolve_blocking(
            stub.to_str().unwrap(),
            &HostKind::DockerContainer("dev".into()),
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(e.to_string().contains("timed out"), "{e}");
        assert!(start.elapsed() < Duration::from_secs(5), "must not wait for the stub to finish");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end relay check against a real container. Skipped unless
    /// `HERDR_MIRROR_IT_CONTAINER` is set, because it needs docker plus a
    /// herdr server running inside the container:
    ///
    ///   HERDR_MIRROR_IT_CONTAINER=crazy_ride \
    ///   HERDR_MIRROR_IT_DOCKER=/usr/local/bin/docker \
    ///   cargo test -- --ignored --nocapture relay_reaches_real_container
    ///
    /// This is the piece the unit tests cannot cover: the relay is only
    /// meaningful against a live socket, and the held `events.subscribe`
    /// stream is the part most likely to behave differently from a one-shot
    /// request.
    #[tokio::test]
    #[ignore]
    async fn relay_reaches_real_container() {
        let Ok(name) = std::env::var("HERDR_MIRROR_IT_CONTAINER") else {
            eprintln!("skipped: set HERDR_MIRROR_IT_CONTAINER");
            return;
        };
        let sock = std::env::var("HERDR_MIRROR_IT_SOCK")
            .unwrap_or_else(|_| "/root/.config/herdr/herdr.sock".into());
        let docker_bin = std::env::var("HERDR_MIRROR_IT_DOCKER").unwrap_or_else(|_| "docker".into());

        let kind = HostKind::DockerContainer(name.clone());
        let ids = resolve(&docker_bin, &kind).await.expect("resolve failed");
        let cid = ids.first().expect("no such container running").clone();
        eprintln!("resolved {name} -> {cid}");

        probe_socat(&docker_bin, &cid).await.expect("socat missing");

        let dir = std::env::temp_dir().join(format!("herdr-mirror-it-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let local_sock = dir.join("it-api.sock");
        let container = Container { id: cid, docker_bin };
        let handle =
            serve_relay(container, sock, local_sock.clone(), Logger::new(&dir, false)).unwrap();

        // the socket must not be readable by other users: it proxies an API
        // that can start agents with arbitrary argv
        let mode = std::fs::metadata(&handle.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "relay socket must be 0600, got {mode:o}");

        // 1. one-shot request/response (ApiClient::connect pings)
        let api = crate::api::ApiClient::connect(&handle.path).await.expect("ping through relay");
        eprintln!("ping ok");

        // 2. a real payload, exercising a larger response than a pong
        let snap = api
            .request("session.snapshot", serde_json::json!({}))
            .await
            .expect("session.snapshot through relay");
        assert!(snap.pointer("/snapshot/workspaces").is_some(), "snapshot shape: {snap}");
        eprintln!("snapshot ok ({} bytes)", snap.to_string().len());

        // 3. several sequential requests: each is a fresh connection, so this
        //    is what would expose a per-request child or fd leak
        for _ in 0..5 {
            api.request("session.snapshot", serde_json::json!({})).await.expect("repeat request");
        }
        eprintln!("5 sequential requests ok");

        // 4. the held subscription — the part a one-shot ping does not prove
        let mut stream = api
            .subscribe(vec![serde_json::json!({ "type": "workspace.created" })])
            .await
            .expect("events.subscribe through relay");
        let idle = timeout(Duration::from_millis(1500), stream.next()).await;
        assert!(idle.is_err() || idle.unwrap().is_some(), "subscription closed early");
        eprintln!("subscribe held ok");

        drop(handle);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
