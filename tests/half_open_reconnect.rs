#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[test]
#[ignore = "process-level failure simulation"]
fn half_open_child_is_reaped_and_replaced_automatically() {
    run_scenario(false, 1, Duration::from_millis(9_500));
}

#[test]
#[ignore = "process-level failure simulation"]
fn silent_child_hits_ready_timeout_and_is_replaced_automatically() {
    run_scenario(true, 1, Duration::from_millis(9_500));
}

#[test]
#[ignore = "long process-level resource simulation"]
fn ten_half_open_cycles_return_to_one_child_then_zero_on_shutdown() {
    run_scenario(false, 10, Duration::from_secs(50));
}

fn run_scenario(first_silent: bool, blackhole_count: u32, recovery_limit: Duration) {
    let dir = std::env::temp_dir().join(format!(
        "herdr-mirror-half-open-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let fake_ssh = dir.join("ssh");
    let counter = dir.join("count");
    std::fs::write(
        &fake_ssh,
        r#"#!/usr/bin/env python3
import base64, json, os, signal, sys

if "terminal session" not in " ".join(sys.argv[1:]):
    sys.exit(1)

count_path = os.environ["FAKE_SSH_COUNT"]
try:
    with open(count_path, "r", encoding="utf-8") as f:
        count = int(f.read()) + 1
except Exception:
    count = 1
with open(count_path, "w", encoding="utf-8") as f:
    f.write(str(count))

signal.signal(signal.SIGTERM, signal.SIG_IGN)
silent = count == 1 and os.environ.get("FAKE_SSH_FIRST_SILENT") == "1"
if not silent:
    print(json.dumps({"type":"terminal.ready","mode":"observe","lease_ms":20000}), flush=True)
healthy = count > int(os.environ["FAKE_SSH_BLACKHOLE_COUNT"])
if healthy:
    print(json.dumps({
        "type":"terminal.frame", "seq":1, "full":True, "width":20, "height":2,
        "bytes":base64.b64encode(b"RECOVERED\r\n").decode("ascii")
    }), flush=True)

for line in sys.stdin:
    try:
        message = json.loads(line)
    except Exception:
        continue
    if healthy and message.get("type") == "terminal.heartbeat":
        print(json.dumps({
            "type":"terminal.heartbeat_ack", "nonce":message["nonce"]
        }), flush=True)
"#,
    )
    .unwrap();
    std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o700)).unwrap();

    let path = format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_herdr-mirror"))
        .args([
            "pane",
            "fake-host",
            "w1:p1",
            "--dump",
            "--terminal-reconnect",
        ])
        .env("PATH", path)
        .env("HOME", &dir)
        .env("FAKE_SSH_COUNT", &counter)
        .env(
            "FAKE_SSH_FIRST_SILENT",
            if first_silent { "1" } else { "0" },
        )
        .env("FAKE_SSH_BLACKHOLE_COUNT", blackhole_count.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (line_tx, line_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = line_tx.send(line);
        }
    });

    let started = Instant::now();
    let recovered = loop {
        let remaining = recovery_limit.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break false;
        }
        match line_rx.recv_timeout(remaining) {
            Ok(line) if line.contains("RECOVERED") => break true,
            Ok(_) => {}
            Err(_) => break false,
        }
    };
    let elapsed = started.elapsed();
    let direct_children = child_process_count(child.id(), None);
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let _ = child.wait();
    let cleanup_deadline = Instant::now() + Duration::from_secs(1);
    while child_process_count(0, Some(&fake_ssh)) != 0 && Instant::now() < cleanup_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(recovered, "mirror did not recover within {recovery_limit:?}");
    assert!(elapsed <= recovery_limit, "elapsed={elapsed:?}");
    assert_eq!(direct_children, 1, "steady state must own one SSH child");
    assert_eq!(child_process_count(0, Some(&fake_ssh)), 0, "no fake SSH child may remain");
    println!(
        "first_silent={first_silent} recovered_ms={}",
        elapsed.as_millis()
    );
    assert!(
        std::fs::read_to_string(&counter)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap()
            > blackhole_count
    );
    std::fs::remove_dir_all(dir).unwrap();
}

fn child_process_count(parent: u32, command_path: Option<&std::path::Path>) -> usize {
    let output = Command::new("ps")
        .args(["-ax", "-o", "ppid=,command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| {
            let Some((ppid, command)) = line.trim().split_once(' ') else {
                return false;
            };
            match command_path {
                Some(path) => command.contains(path.to_string_lossy().as_ref()),
                None => ppid.parse::<u32>().ok() == Some(parent),
            }
        })
        .count()
}
