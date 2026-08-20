#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

#[test]
#[ignore = "requires HERDR_MIRROR_E2E_SSH_TARGET, _PANE, and _REMOTE_BIN"]
fn real_ssh_blackhole_recovers_without_releasing_the_old_connection() {
    let target = required("HERDR_MIRROR_E2E_SSH_TARGET");
    let pane = required("HERDR_MIRROR_E2E_PANE");
    let remote_bin = required("HERDR_MIRROR_E2E_REMOTE_BIN");
    let cycles = std::env::var("HERDR_MIRROR_E2E_CYCLES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1);

    let pane_info: serde_json::Value = serde_json::from_slice(&direct_ssh(
        &target,
        &format!("{} pane get {}", shell_quote(&remote_bin), shell_quote(&pane)),
    ))
    .expect("remote pane info json");
    let terminal_id = pane_info["result"]["pane"]["terminal_id"]
        .as_str()
        .expect("remote terminal id")
        .to_owned();

    let ssh = SshTarget::resolve(&target);
    let proxy = BlackholeProxy::start(ssh.address);
    let home = TempHome::new("real-ssh");
    ssh.write_alias(&home.path, proxy.address());
    let probe = Command::new(home.path.join("bin/ssh"))
        .args(["-o", "BatchMode=yes", "herdr-reconnect-e2e", "true"])
        .status()
        .expect("ssh proxy probe");
    assert!(probe.success(), "ssh proxy probe failed");
    let mut mirror = MirrorProcess::start(&home.path, &pane, &remote_bin);

    mirror.wait_for("--- frame", Duration::from_secs(10));
    mirror.write_and_wait("INITIAL_OK", Duration::from_secs(10));
    let streams_before_stress = stream_attempts(&home.path);
    proxy.set_downstream_rate(32 * 1024);
    mirror.write_command_and_wait(
        "i=0; while [ $i -lt 4000 ]; do echo SUSTAINED_$i; i=$((i+1)); done; echo SUSTAINED_DONE",
        "SUSTAINED_DONE",
        Duration::from_secs(20),
    );
    proxy.set_downstream_rate(0);
    assert!(
        stream_attempts(&home.path) == streams_before_stress,
        "healthy slow output triggered a reconnect"
    );
    for cycle in 0..cycles {
        let blocked = proxy.blackhole_existing();
        let started = Instant::now();
        proxy.wait_for_new_connection(blocked, Duration::from_secs(9));
        mirror.write_and_wait(&format!("RECOVERED_{cycle}"), Duration::from_secs(10));
        assert!(
            started.elapsed() <= Duration::from_secs(10),
            "cycle {cycle} exceeded recovery budget: {:?}",
            started.elapsed()
        );
    }

    mirror.stop();
    proxy.stop();

    std::thread::sleep(Duration::from_secs(21));
    let sessions: serde_json::Value = serde_json::from_slice(&direct_ssh(
        &target,
        &format!(
            "{} server terminal-sessions --json",
            shell_quote(&remote_bin)
        ),
    ))
    .expect("terminal session list json");
    let retained = sessions["result"]["sessions"]
        .as_array()
        .expect("terminal sessions")
        .iter()
        .any(|session| session["terminal_id"].as_str() == Some(terminal_id.as_str()));
    assert!(!retained, "remote client/claim/lease survived cleanup");

    let processes = String::from_utf8(direct_ssh(&target, "ps -eo ppid=,args="))
        .expect("remote process list utf8");
    assert!(
        !processes
            .lines()
            .any(|line| line.contains("terminal session") && line.contains(&pane)),
        "remote helper survived cleanup:\n{processes}"
    );
}

#[test]
#[ignore = "requires HERDR_MIRROR_E2E_SSH_TARGET, _PANE, and _REMOTE_BIN"]
fn different_installation_cannot_displace_the_live_controller() {
    let target = required("HERDR_MIRROR_E2E_SSH_TARGET");
    let pane = required("HERDR_MIRROR_E2E_PANE");
    let remote_bin = required("HERDR_MIRROR_E2E_REMOTE_BIN");
    let ssh = SshTarget::resolve(&target);
    let proxy = BlackholeProxy::start(ssh.address);
    let home_a = TempHome::new("controller-a");
    let home_b = TempHome::new("controller-b");
    ssh.write_alias(&home_a.path, proxy.address());
    ssh.write_alias(&home_b.path, proxy.address());

    let mut owner = MirrorProcess::start(&home_a.path, &pane, &remote_bin);
    owner.wait_for("--- frame", Duration::from_secs(10));
    owner.write_and_wait("OWNER_READY", Duration::from_secs(10));

    let mut contender = MirrorProcess::start(&home_b.path, &pane, &remote_bin);
    contender.wait_for("--- frame", Duration::from_secs(10));
    contender.write("printf 'MUST_NOT_RUN\\n'\r");
    std::thread::sleep(Duration::from_secs(3));
    owner.write_and_wait("OWNER_STILL_WRITABLE", Duration::from_secs(10));

    let screen = String::from_utf8(direct_ssh(
        &target,
        &format!(
            "{} pane read {} --lines 200",
            shell_quote(&remote_bin),
            shell_quote(&pane)
        ),
    ))
    .expect("remote pane text");
    assert!(!screen.contains("MUST_NOT_RUN"), "contender input reached owner pane");

    contender.stop();
    owner.stop();
    proxy.stop();
}

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}

fn direct_ssh(target: &str, command: &str) -> Vec<u8> {
    let output = Command::new("ssh")
        .args(["-o", "BatchMode=yes", target, command])
        .output()
        .expect("direct ssh command");
    assert!(
        output.status.success(),
        "direct ssh failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn stream_attempts(home: &Path) -> usize {
    std::fs::read_to_string(home.join("ssh-invocations.log"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("terminal session"))
        .count()
}

struct SshTarget {
    address: SocketAddr,
    user: String,
    identities: Vec<PathBuf>,
    ssh_program: PathBuf,
}

impl SshTarget {
    fn resolve(target: &str) -> Self {
        let which = Command::new("which").arg("ssh").output().expect("which ssh");
        assert!(which.status.success(), "ssh executable not found");
        let ssh_program = PathBuf::from(
            String::from_utf8(which.stdout)
                .expect("ssh path utf8")
                .trim(),
        );
        let output = Command::new(&ssh_program)
            .args(["-G", target])
            .output()
            .expect("ssh -G");
        assert!(output.status.success(), "ssh -G failed");
        let mut hostname = None;
        let mut port = 22_u16;
        let mut user = None;
        let mut identities = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Some((key, value)) = line.split_once(' ') else {
                continue;
            };
            match key {
                "hostname" => hostname = Some(value.to_owned()),
                "port" => port = value.parse().expect("ssh port"),
                "user" => user = Some(value.to_owned()),
                "identityfile" => {
                    let path = expand_home(value);
                    if path.exists() {
                        identities.push(path);
                    }
                }
                "proxycommand" if value != "none" => {
                    panic!("proxycommand targets are not supported by this harness")
                }
                _ => {}
            }
        }
        let hostname = hostname.expect("ssh hostname");
        let address = (hostname.as_str(), port)
            .to_socket_addrs()
            .expect("resolve ssh host")
            .next()
            .expect("ssh address");
        Self {
            address,
            user: user.expect("ssh user"),
            identities,
            ssh_program,
        }
    }

    fn write_alias(&self, home: &Path, proxy: SocketAddr) {
        let ssh_dir = home.join(".ssh");
        std::fs::create_dir_all(&ssh_dir).unwrap();
        let mut config = format!(
            "Host herdr-reconnect-e2e\n  HostName {}\n  Port {}\n  User {}\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null\n",
            proxy.ip(),
            proxy.port(),
            self.user
        );
        for identity in &self.identities {
            config.push_str(&format!("  IdentityFile {}\n", identity.display()));
        }
        std::fs::write(ssh_dir.join("config"), config).unwrap();
        let bin_dir = home.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let wrapper = bin_dir.join("ssh");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}/ssh-invocations.log'\nexec '{}' -F '{}/.ssh/config' \"$@\"\n",
                home.display(),
                self.ssh_program.display(),
                home.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        PathBuf::from(std::env::var("HOME").unwrap()).join(rest)
    } else {
        PathBuf::from(path)
    }
}

struct BlackholeProxy {
    listener_address: SocketAddr,
    accepted: Arc<AtomicU64>,
    blackhole_through: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    downstream_bytes_per_second: Arc<AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl BlackholeProxy {
    fn start(remote: SocketAddr) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener_address = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicU64::new(0));
        let blackhole_through = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let downstream_bytes_per_second = Arc::new(AtomicU64::new(0));
        let thread = {
            let accepted = Arc::clone(&accepted);
            let blackhole_through = Arc::clone(&blackhole_through);
            let stop = Arc::clone(&stop);
            let downstream_bytes_per_second = Arc::clone(&downstream_bytes_per_second);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((client, _)) => {
                            let id = accepted.fetch_add(1, Ordering::AcqRel) + 1;
                            let server = TcpStream::connect(remote).expect("proxy connects remote");
                            client.set_nonblocking(false).unwrap();
                            server.set_nonblocking(false).unwrap();
                            spawn_pump(
                                client.try_clone().unwrap(),
                                server.try_clone().unwrap(),
                                id,
                                Arc::clone(&blackhole_through),
                                Arc::new(AtomicU64::new(0)),
                            );
                            spawn_pump(
                                server,
                                client,
                                id,
                                Arc::clone(&blackhole_through),
                                Arc::clone(&downstream_bytes_per_second),
                            );
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("proxy accept: {error}"),
                    }
                }
            })
        };
        Self {
            listener_address,
            accepted,
            blackhole_through,
            stop,
            downstream_bytes_per_second,
            thread: Some(thread),
        }
    }

    fn address(&self) -> SocketAddr {
        self.listener_address
    }

    fn set_downstream_rate(&self, bytes_per_second: u64) {
        self.downstream_bytes_per_second
            .store(bytes_per_second, Ordering::Release);
    }

    fn blackhole_existing(&self) -> u64 {
        let mut through = self.accepted.load(Ordering::Acquire);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            std::thread::sleep(Duration::from_millis(250));
            let current = self.accepted.load(Ordering::Acquire);
            if current == through || Instant::now() >= deadline {
                break;
            }
            through = current;
        }
        assert!(through > 0, "no established SSH connection to blackhole");
        self.blackhole_through.store(through, Ordering::Release);
        through
    }

    fn wait_for_new_connection(&self, previous: u64, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.accepted.load(Ordering::Acquire) <= previous {
            assert!(Instant::now() < deadline, "mirror did not dial replacement SSH");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn spawn_pump(
    mut source: TcpStream,
    mut destination: TcpStream,
    id: u64,
    blackhole_through: Arc<AtomicU64>,
    bytes_per_second: Arc<AtomicU64>,
) {
    std::thread::spawn(move || {
        let mut bytes = [0_u8; 16 * 1024];
        loop {
            let count = match source.read(&mut bytes) {
                Ok(0) | Err(_) => return,
                Ok(count) => count,
            };
            if id <= blackhole_through.load(Ordering::Acquire) {
                continue;
            }
            let bytes_per_second = bytes_per_second.load(Ordering::Acquire);
            if bytes_per_second > 0 {
                std::thread::sleep(Duration::from_secs_f64(
                    count as f64 / bytes_per_second as f64,
                ));
            }
            if destination.write_all(&bytes[..count]).is_err() {
                return;
            }
        }
    });
}

struct MirrorProcess {
    child: Child,
    stdin: std::process::ChildStdin,
    lines: mpsc::Receiver<String>,
}

impl MirrorProcess {
    fn start(home: &Path, pane: &str, remote_bin: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_herdr-mirror"))
            .args([
                "pane",
                "herdr-reconnect-e2e",
                pane,
                "--dump",
                "--always-control",
                "--terminal-reconnect",
                "--remote-bin",
                remote_bin,
            ])
            .env("HOME", home)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    home.join("bin").display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (line_tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = line_tx.send(line);
            }
        });
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn write_and_wait(&mut self, marker: &str, timeout: Duration) {
        self.write_command_and_wait(&format!("printf '{marker}\\n'"), marker, timeout);
    }

    fn write_command_and_wait(&mut self, command: &str, marker: &str, timeout: Duration) {
        self.write(&format!("{command}\r"));
        self.wait_for(marker, timeout);
    }

    fn write(&mut self, input: &str) {
        self.stdin.write_all(input.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn wait_for(&mut self, marker: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "did not observe marker {marker}");
            let line = self.lines.recv_timeout(remaining).expect("mirror output closed");
            if line.contains(marker) {
                return;
            }
        }
    }

    fn stop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let _ = self.child.wait();
    }
}

struct TempHome {
    path: PathBuf,
}

impl TempHome {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "herdr-mirror-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
