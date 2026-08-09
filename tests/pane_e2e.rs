#![cfg(unix)]

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "herdr-mirror-pane-e2e-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("bin")).unwrap();
    dir
}

fn write_fake_ssh(dir: &Path) {
    let script = r#"#!/bin/sh
if [ -n "${MIRROR_TEST_CALLS:-}" ]; then
  printf '%s\n' "$*" >> "$MIRROR_TEST_CALLS"
fi
if [ -n "${MIRROR_TEST_SSH_PID:-}" ]; then
  printf '%s\n' "$$" > "$MIRROR_TEST_SSH_PID"
fi

case "$*" in
  *"terminal session control"*)
    mirror_test_state=${MIRROR_TEST_STATE:-local}
    if [ -n "${MIRROR_TEST_STATE_FILE:-}" ] && [ -f "$MIRROR_TEST_STATE_FILE" ]; then
      IFS= read -r mirror_test_state < "$MIRROR_TEST_STATE_FILE"
    fi
    case "$mirror_test_state" in
      local)
        printf '%s\n' '{"type":"terminal.state","mouse_reporting":false,"mouse_pixel_reporting":false,"mouse_any_motion":false,"alternate_screen":false,"application_cursor":false}'
        ;;
      remote)
        printf '%s\n' '{"type":"terminal.state","mouse_reporting":true,"mouse_pixel_reporting":false,"mouse_any_motion":true,"alternate_screen":true,"application_cursor":true}'
        ;;
      remote-button)
        printf '%s\n' '{"type":"terminal.state","mouse_reporting":true,"mouse_pixel_reporting":false,"mouse_any_motion":false,"alternate_screen":true,"application_cursor":true}'
        ;;
      pixel)
        printf '%s\n' '{"type":"terminal.state","mouse_reporting":true,"mouse_pixel_reporting":true,"mouse_any_motion":true,"alternate_screen":true,"application_cursor":true}'
        ;;
      malformed)
        printf '%s\n' '{"type":"terminal.state","mouse_reporting":"yes","application_cursor":1}'
        ;;
      missing-pixel)
        printf '%s\n' '{"type":"terminal.state","mouse_reporting":true,"mouse_any_motion":true}'
        ;;
      null-pixel)
        printf '%s\n' '{"type":"terminal.state","mouse_reporting":true,"mouse_pixel_reporting":null,"mouse_any_motion":true}'
        ;;
      wrong-pixel)
        printf '%s\n' '{"type":"terminal.state","mouse_reporting":true,"mouse_pixel_reporting":"no","mouse_any_motion":true}'
        ;;
      missing-any)
        printf '%s\n' '{"type":"terminal.state","mouse_reporting":true,"mouse_pixel_reporting":false}'
        ;;
      null-any)
        printf '%s\n' '{"type":"terminal.state","mouse_reporting":true,"mouse_pixel_reporting":false,"mouse_any_motion":null}'
        ;;
      wrong-any)
        printf '%s\n' '{"type":"terminal.state","mouse_reporting":true,"mouse_pixel_reporting":false,"mouse_any_motion":"yes"}'
        ;;
      missing)
        ;;
    esac
    printf '%s\n' '{"type":"terminal.frame","seq":1,"full":true,"width":100,"height":40,"bytes":"G1sxOzFIG1swbWhlbGxvIHdvcmxkG1sxOzEySBtbPzI1aA=="}'
    frame_reader_pid=
    cleanup_frame_reader() {
      if [ -n "$frame_reader_pid" ]; then
        kill "$frame_reader_pid" 2>/dev/null || :
        wait "$frame_reader_pid" 2>/dev/null || :
      fi
    }
    trap cleanup_frame_reader EXIT
    trap 'exit 0' HUP INT TERM
    if [ -n "${MIRROR_TEST_FRAME_FIFO:-}" ]; then
      while :; do cat "$MIRROR_TEST_FRAME_FIFO"; done &
      frame_reader_pid=$!
    fi
    while IFS= read -r line; do
      printf '%s\n' "$line" >> "$MIRROR_TEST_CAPTURE"
    done
    ;;
  *"terminal session observe"*)
    printf '%s\n' '{"type":"terminal.state","mouse_reporting":false,"mouse_pixel_reporting":false,"mouse_any_motion":false,"alternate_screen":false,"application_cursor":false}'
    printf '%s\n' '{"type":"terminal.frame","seq":1,"full":true,"width":100,"height":40,"bytes":"G1sxOzFIG1swbWhlbGxvIHdvcmxkG1sxOzEySBtbPzI1aA=="}'
    while IFS= read -r line; do :; done
    ;;
  *)
    exit 2
    ;;
esac
"#;
    let path = dir.join("bin/ssh");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn open_pty(cols: u16, rows: u16) -> (File, File) {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    assert_eq!(
        result,
        0,
        "openpty failed: {}",
        std::io::Error::last_os_error()
    );
    let master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    (master, slave)
}

fn wait_for_output(master: &mut File, needle: &[u8], timeout: Duration) -> Vec<u8> {
    let start = Instant::now();
    let mut output = Vec::new();
    while start.elapsed() < timeout {
        let mut buf = [0u8; 8192];
        match master.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => output.extend_from_slice(&buf[..n]),
            Err(err) if err.kind() == ErrorKind::WouldBlock => {}
            Err(err) => panic!("pty read failed: {err}"),
        }
        if output.windows(needle.len()).any(|window| window == needle) {
            return output;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "timed out waiting for {:?}; output={:?}",
        String::from_utf8_lossy(needle),
        String::from_utf8_lossy(&output)
    );
}

fn wait_for_file(path: &Path, needle: &str, timeout: Duration) -> String {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let text = fs::read_to_string(path).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {needle:?} in {}", path.display());
}

fn wait_for_file_occurrences(
    path: &Path,
    needle: &str,
    expected: usize,
    timeout: Duration,
) -> String {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let text = fs::read_to_string(path).unwrap_or_default();
        if text.matches(needle).count() >= expected {
            return text;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "timed out waiting for {expected} occurrences of {needle:?} in {}",
        path.display()
    );
}

fn wait_for_pid(path: &Path, timeout: Duration) -> i32 {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(pid) = fs::read_to_string(path)
            .unwrap_or_default()
            .trim()
            .parse::<i32>()
        {
            if pid > 0 {
                return pid;
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for pid in {}", path.display());
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            start.elapsed() < timeout,
            "wrapper did not exit within {timeout:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn create_fifo(path: &Path) {
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let result = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
    assert_eq!(
        result,
        0,
        "mkfifo failed: {}",
        std::io::Error::last_os_error()
    );
}

fn inject_frame(path: &Path, seq: u64, ansi: &str, timeout: Duration) {
    inject_sized_frame(path, seq, false, 100, 40, ansi, timeout);
}

fn inject_sized_frame(
    path: &Path,
    seq: u64,
    full: bool,
    width: usize,
    height: usize,
    ansi: &str,
    timeout: Duration,
) {
    let frame = serde_json::json!({
        "type": "terminal.frame",
        "seq": seq,
        "full": full,
        "width": width,
        "height": height,
        "bytes": B64.encode(ansi.as_bytes()),
    })
    .to_string();
    inject_line(path, &frame, timeout);
}

fn terminal_state_line(alternate_screen: bool, scroll: Option<(u64, u64, u64)>) -> String {
    let mut state = serde_json::json!({
        "type": "terminal.state",
        "mouse_reporting": false,
        "mouse_pixel_reporting": false,
        "mouse_any_motion": false,
        "alternate_screen": alternate_screen,
        "application_cursor": false,
    });
    if let Some((offset_from_bottom, max_offset_from_bottom, viewport_rows)) = scroll {
        state["scroll"] = serde_json::json!({
            "offset_from_bottom": offset_from_bottom,
            "max_offset_from_bottom": max_offset_from_bottom,
            "viewport_rows": viewport_rows,
        });
    }
    state.to_string()
}

fn inject_line(path: &Path, line: &str, timeout: Duration) {
    let start = Instant::now();
    loop {
        match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
        {
            Ok(mut fifo) => {
                writeln!(fifo, "{line}").unwrap();
                fifo.flush().unwrap();
                return;
            }
            Err(err) if err.raw_os_error() == Some(libc::ENXIO) => {}
            Err(err) => panic!("failed to open frame fifo: {err}"),
        }
        assert!(
            start.elapsed() < timeout,
            "fake control session did not open frame fifo within {timeout:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn assert_file_stays_without(path: &Path, needle: &str, duration: Duration) {
    let start = Instant::now();
    while start.elapsed() < duration {
        let text = fs::read_to_string(path).unwrap_or_default();
        assert!(!text.contains(needle), "unexpected {needle:?} in {text}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn read_available(master: &mut File) -> Vec<u8> {
    let mut output = Vec::new();
    loop {
        let mut buf = [0u8; 8192];
        match master.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => output.extend_from_slice(&buf[..n]),
            Err(err) if err.kind() == ErrorKind::WouldBlock => break,
            Err(err) => panic!("pty read failed: {err}"),
        }
    }
    output
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn captured_resize_sizes(path: &Path) -> Vec<(u64, u64)> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|value| value.get("type").and_then(|kind| kind.as_str()) == Some("terminal.resize"))
        .filter_map(|value| Some((value.get("cols")?.as_u64()?, value.get("rows")?.as_u64()?)))
        .collect()
}

fn assert_remote_state_applied_before_mouse(initial: &[u8]) {
    // The remote fixture reports application cursor mode alongside mouse mode.
    // Seeing DECCKM and any-event tracking locally proves terminal.state was
    // consumed before mouse input is injected.
    assert!(contains_bytes(initial, b"\x1b[?1h"), "{initial:?}");
    assert!(contains_bytes(initial, b"\x1b[?1003h"), "{initial:?}");
}

#[test]
fn controlled_local_mouse_state_routes_wheel_and_copies_drag_selection() {
    let dir = temp_dir();
    write_fake_ssh(&dir);
    let capture = dir.join("control-input.jsonl");
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&capture)
        .unwrap();

    let (mut master, slave) = open_pty(100, 40);
    let path = format!(
        "{}:{}",
        dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-mirror"));
    command
        .args([
            "pane",
            "fake-host",
            "w1:p1",
            "--always-control",
            "--cols",
            "100",
            "--rows",
            "40",
        ])
        .env("PATH", path)
        .env("MIRROR_TEST_CAPTURE", &capture)
        .env("MIRROR_TEST_STATE", "local")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    let mut child = ChildGuard(command.spawn().unwrap());

    // Drain the complete synchronized paint, not just the first visible text.
    // The wrapper writes the frame synchronously; if the test stops reading at
    // "hello world", macOS's small PTY output queue can fill on the remaining
    // rows and block the event loop before it ever reads the wheel below.
    let initial = wait_for_output(
        &mut master,
        b"\x1b[1;12H\x1b[?25h\x1b[?2026l",
        Duration::from_secs(5),
    );
    assert!(
        contains_bytes(&initial, b"hello world"),
        "initial frame did not render: {:?}",
        String::from_utf8_lossy(&initial)
    );

    // The wrapper must emit semantic scroll JSON, never literal SGR bytes. A
    // confirmed mouse prefix remains safe even when scheduler pressure leaves
    // more than the Escape-key ambiguity timeout between PTY reads.
    master.write_all(b"\x1b[<64;").unwrap();
    master.flush().unwrap();
    thread::sleep(Duration::from_millis(30));
    master.write_all(b"10;5M").unwrap();
    master.flush().unwrap();
    let captured = wait_for_file(&capture, "terminal.scroll", Duration::from_secs(5));
    assert!(captured.contains(r#""direction":"up""#), "{captured}");
    assert!(captured.contains(r#""column":9"#), "{captured}");
    assert!(captured.contains(r#""row":4"#), "{captured}");

    // Modifier bits survive the semantic conversion. Herdr needs these to
    // re-encode Shift/Alt/Ctrl-wheel for a mouse-aware target application.
    master.write_all(b"\x1b[<68;10;5M").unwrap();
    master.flush().unwrap();
    let captured =
        wait_for_file_occurrences(&capture, "terminal.scroll", 2, Duration::from_secs(5));
    assert!(captured.contains(r#""modifiers":1"#), "{captured}");

    // An absurd but valid SGR report clamps to the primary-screen source
    // viewport, never into the reserved scrollbar column or beyond its rows.
    master
        .write_all(b"\x1b[<64;4294967295;4294967295M")
        .unwrap();
    master.flush().unwrap();
    let captured =
        wait_for_file_occurrences(&capture, "terminal.scroll", 3, Duration::from_secs(5));
    assert!(captured.contains(r#""column":98"#), "{captured}");
    assert!(captured.contains(r#""row":39"#), "{captured}");

    // Authoritative mouse_reporting=false keeps drag selection local: it paints
    // and copies "hello" through OSC 52 instead of sending mouse input remote.
    master.write_all(b"\x1b[<0;1;1M\x1b[<32;5;1M").unwrap();
    master.flush().unwrap();
    wait_for_output(&mut master, b"\x1b[7mhello", Duration::from_secs(5));
    master.write_all(b"\x1b[<0;5;1m").unwrap();
    master.flush().unwrap();
    wait_for_output(
        &mut master,
        b"\x1b]52;c;aGVsbG8=\x07",
        Duration::from_secs(5),
    );
    let captured = fs::read_to_string(&capture).unwrap();
    assert!(!captured.contains("terminal.mouse"), "{captured}");

    // Plain and non-left clicks are local too; none may leak into the prompt.
    master.write_all(b"\x1b[<0;2;1M\x1b[<0;2;1m").unwrap();
    master.flush().unwrap();
    master.write_all(b"\x1b[<2;3;1M\x1b[<2;3;1m").unwrap();
    master.flush().unwrap();
    assert_file_stays_without(&capture, "terminal.mouse", Duration::from_millis(100));

    unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) };
    let status = wait_for_exit(&mut child.0, Duration::from_secs(5));
    assert!(status.success(), "wrapper exited with {status}");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn controlled_primary_scrollbar_tracks_state_and_keeps_input_out_of_the_gutter() {
    let dir = temp_dir();
    write_fake_ssh(&dir);
    let capture = dir.join("control-input.jsonl");
    let calls = dir.join("ssh-calls.log");
    let frames = dir.join("frames.fifo");
    File::create(&capture).unwrap();
    File::create(&calls).unwrap();
    create_fifo(&frames);

    let (mut master, slave) = open_pty(100, 40);
    let path = format!(
        "{}:{}",
        dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-mirror"));
    command
        .args([
            "pane",
            "fake-host",
            "w1:p1",
            "--always-control",
            "--cols",
            "100",
            "--rows",
            "40",
        ])
        .env("PATH", path)
        .env("MIRROR_TEST_CAPTURE", &capture)
        .env("MIRROR_TEST_CALLS", &calls)
        .env("MIRROR_TEST_FRAME_FIFO", &frames)
        .env("MIRROR_TEST_STATE", "local")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    let mut child = ChildGuard(command.spawn().unwrap());

    let initial = wait_for_output(
        &mut master,
        b"\x1b[1;12H\x1b[?25h\x1b[?2026l",
        Duration::from_secs(5),
    );
    assert!(contains_bytes(&initial, b"hello world"), "{initial:?}");
    let call = wait_for_file(&calls, "--cols 100 --rows 40", Duration::from_secs(5));
    assert!(call.contains("terminal session control"), "{call}");
    wait_for_file(&capture, "terminal.resize", Duration::from_secs(5));
    assert_eq!(captured_resize_sizes(&capture), vec![(99, 40)]);

    // A resized source frame keeps its final content cell at source column 99.
    // Scroll state arrives separately, so this is also proof that state-only
    // updates repaint the gutter without waiting for another terminal frame.
    let _ = read_available(&mut master);
    inject_sized_frame(
        &frames,
        2,
        true,
        99,
        40,
        "\x1b[1;1Hleft\x1b[1;99HX\x1b[?25l",
        Duration::from_secs(5),
    );
    let frame = wait_for_output(&mut master, b"\x1b[?2026l", Duration::from_secs(5));
    assert!(contains_bytes(&frame, b"X"), "{frame:?}");
    let _ = read_available(&mut master);

    let top_state = terminal_state_line(false, Some((360, 360, 40)));
    inject_line(&frames, &top_state, Duration::from_secs(5));
    let top_bar = wait_for_output(&mut master, b"\x1b[?2026l", Duration::from_secs(5));
    assert!(
        contains_bytes(&top_bar, b"\x1b[1;100H\x1b[0m\xe2\x96\x90"),
        "top thumb was not painted: {top_bar:?}"
    );
    let edge = top_bar.iter().position(|byte| *byte == b'X').unwrap();
    let bar = top_bar
        .windows(b"\x1b[1;100H".len())
        .position(|window| window == b"\x1b[1;100H")
        .unwrap();
    assert!(edge < bar, "source edge was overwritten: {top_bar:?}");

    // A gesture born in column 100 belongs to the display-only gutter. Moving
    // back into the source must not create a selection or clipboard write.
    let _ = read_available(&mut master);
    master
        .write_all(b"\x1b[<0;100;1M\x1b[<32;4;1M\x1b[<0;4;1m")
        .unwrap();
    master.flush().unwrap();
    let gutter_release = wait_for_output(&mut master, b"\x1b[?2026l", Duration::from_secs(5));
    assert!(!contains_bytes(&gutter_release, b"\x1b]52;"));
    assert!(!contains_bytes(&gutter_release, b"\x1b[7m"));
    assert_file_stays_without(&capture, "terminal.mouse", Duration::from_millis(100));

    // Wheel input over the gutter remains useful, but its source coordinate is
    // clamped to source column 99 (zero-based 98) and the last source row.
    master.write_all(b"\x1b[<64;100;40M").unwrap();
    master.flush().unwrap();
    let captured = wait_for_file(&capture, "terminal.scroll", Duration::from_secs(5));
    assert!(captured.contains(r#""column":98"#), "{captured}");
    assert!(captured.contains(r#""row":39"#), "{captured}");

    // Repeating identical scroll metrics must not create a resize loop.
    inject_line(&frames, &top_state, Duration::from_secs(5));
    thread::sleep(Duration::from_millis(100));
    assert_eq!(captured_resize_sizes(&capture), vec![(99, 40)]);

    // Moving the thumb is a state-only repaint: the old top becomes track and
    // the bottom four rows become the thumb, with no terminal frame in between.
    let _ = read_available(&mut master);
    inject_line(
        &frames,
        &terminal_state_line(false, Some((0, 360, 40))),
        Duration::from_secs(5),
    );
    let bottom_bar = wait_for_output(&mut master, b"\x1b[?2026l", Duration::from_secs(5));
    assert!(
        contains_bytes(&bottom_bar, b"\x1b[37;100H\x1b[0m\xe2\x96\x90"),
        "bottom thumb was not painted: {bottom_bar:?}"
    );
    assert!(
        contains_bytes(&bottom_bar, b"\x1b[1;100H\x1b[0;2m\xe2\x96\x95"),
        "old thumb was not repainted as track: {bottom_bar:?}"
    );
    assert_eq!(captured_resize_sizes(&capture), vec![(99, 40)]);

    // max_offset=0 clears every stale gutter glyph without changing width.
    let _ = read_available(&mut master);
    inject_line(
        &frames,
        &terminal_state_line(false, Some((0, 0, 40))),
        Duration::from_secs(5),
    );
    let cleared = wait_for_output(&mut master, b"\x1b[?2026l", Duration::from_secs(5));
    assert!(!contains_bytes(&cleared, "▐".as_bytes()), "{cleared:?}");
    assert!(!contains_bytes(&cleared, "▕".as_bytes()), "{cleared:?}");
    assert!(contains_bytes(&cleared, b"\x1b[K"), "{cleared:?}");
    assert_eq!(captured_resize_sizes(&capture), vec![(99, 40)]);

    // Recreate a bar, then enter alternate screen while retaining deliberately
    // stale scroll metrics. Alternate screen reclaims column 100 and clears the
    // bar; it emits exactly one resize back to the full 100-column source.
    let _ = read_available(&mut master);
    inject_line(&frames, &top_state, Duration::from_secs(5));
    let restored = wait_for_output(&mut master, b"\x1b[?2026l", Duration::from_secs(5));
    assert!(
        contains_bytes(&restored, b"\x1b[1;100H\x1b[0m\xe2\x96\x90"),
        "top thumb was not restored: {restored:?}"
    );
    let _ = read_available(&mut master);
    let alternate_state = terminal_state_line(true, Some((360, 360, 40)));
    inject_line(&frames, &alternate_state, Duration::from_secs(5));
    wait_for_file_occurrences(&capture, "terminal.resize", 2, Duration::from_secs(5));
    let alternate = wait_for_output(&mut master, b"\x1b[?2026l", Duration::from_secs(5));
    assert!(!contains_bytes(&alternate, "▐".as_bytes()), "{alternate:?}");
    assert!(!contains_bytes(&alternate, "▕".as_bytes()), "{alternate:?}");
    assert!(contains_bytes(&alternate, b"\x1b[K"), "{alternate:?}");
    assert_eq!(captured_resize_sizes(&capture), vec![(99, 40), (100, 40)]);

    inject_line(&frames, &alternate_state, Duration::from_secs(5));
    thread::sleep(Duration::from_millis(100));
    assert_eq!(captured_resize_sizes(&capture), vec![(99, 40), (100, 40)]);

    unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) };
    let status = wait_for_exit(&mut child.0, Duration::from_secs(5));
    assert!(status.success(), "wrapper exited with {status}");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn reconnect_downgrades_any_motion_before_accepting_another_wheel() {
    let dir = temp_dir();
    write_fake_ssh(&dir);
    let capture = dir.join("control-input.jsonl");
    let calls = dir.join("ssh-calls.log");
    let ssh_pid = dir.join("ssh.pid");
    let state = dir.join("terminal-state");
    File::create(&capture).unwrap();
    File::create(&calls).unwrap();
    fs::write(&state, "remote\n").unwrap();

    let (mut master, slave) = open_pty(100, 40);
    let path = format!(
        "{}:{}",
        dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-mirror"));
    command
        .args([
            "pane",
            "fake-host",
            "w1:p1",
            "--always-control",
            "--cols",
            "100",
            "--rows",
            "40",
        ])
        .env("PATH", path)
        .env("MIRROR_TEST_CAPTURE", &capture)
        .env("MIRROR_TEST_CALLS", &calls)
        .env("MIRROR_TEST_SSH_PID", &ssh_pid)
        .env("MIRROR_TEST_STATE_FILE", &state)
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    let mut child = ChildGuard(command.spawn().unwrap());

    let initial = wait_for_output(
        &mut master,
        b"\x1b[1;12H\x1b[?25h\x1b[?2026l",
        Duration::from_secs(5),
    );
    assert!(contains_bytes(&initial, b"\x1b[?1002l\x1b[?1003h"));
    let first_ssh_pid = wait_for_pid(&ssh_pid, Duration::from_secs(5));

    // A reconnect starts with unknown remote state, so Mirror must downgrade
    // its host grab from 1003 to 1002. Re-enabling 1002 after disabling 1003
    // is what keeps Ghostty's encoder state aligned with its DEC mode bits.
    fs::write(&state, "remote-button\n").unwrap();
    assert_eq!(unsafe { libc::kill(first_ssh_pid, libc::SIGTERM) }, 0);
    wait_for_file_occurrences(
        &calls,
        "terminal session control",
        2,
        Duration::from_secs(5),
    );
    let reconnect = wait_for_output(
        &mut master,
        b"\x1b[?1003l\x1b[?1002h",
        Duration::from_secs(5),
    );
    assert!(
        contains_bytes(&reconnect, b"\x1b[?1003l\x1b[?1002h"),
        "{reconnect:?}"
    );

    // The PTY now behaves like the hosting terminal after that downgrade: a
    // wheel report must still reach Mirror and become one semantic command.
    master.write_all(b"\x1b[<64;10;5M").unwrap();
    master.flush().unwrap();
    let captured = wait_for_file(&capture, "terminal.scroll", Duration::from_secs(5));
    assert_eq!(captured.matches("terminal.scroll").count(), 1, "{captured}");
    assert!(captured.contains(r#""direction":"up""#), "{captured}");

    unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) };
    let status = wait_for_exit(&mut child.0, Duration::from_secs(5));
    assert!(status.success(), "wrapper exited with {status}");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn controlled_local_state_routes_wheel_selects_drops_click_and_clamps_drag() {
    let dir = temp_dir();
    write_fake_ssh(&dir);
    let capture = dir.join("control-input.jsonl");
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&capture)
        .unwrap();

    let (mut master, slave) = open_pty(100, 40);
    let path = format!(
        "{}:{}",
        dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-mirror"));
    command
        .args([
            "pane",
            "fake-host",
            "w1:p1",
            "--always-control",
            "--cols",
            "100",
            "--rows",
            "40",
        ])
        .env("PATH", path)
        .env("MIRROR_TEST_CAPTURE", &capture)
        .env("MIRROR_TEST_STATE", "local")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    let mut child = ChildGuard(command.spawn().unwrap());

    let initial = wait_for_output(
        &mut master,
        b"\x1b[1;12H\x1b[?25h\x1b[?2026l",
        Duration::from_secs(5),
    );
    assert!(contains_bytes(&initial, b"hello world"));

    master.write_all(b"\x1b[<64;10;5M").unwrap();
    master.flush().unwrap();
    let captured = wait_for_file(&capture, "terminal.scroll", Duration::from_secs(5));
    assert!(captured.contains(r#""direction":"up""#), "{captured}");

    // Controlled local state owns selection because it must keep the mouse grab
    // for semantic scrolling. The highlight must appear before release.
    master.write_all(b"\x1b[<0;1;1M\x1b[<32;5;1M").unwrap();
    master.flush().unwrap();
    wait_for_output(&mut master, b"\x1b[7mhello", Duration::from_secs(5));
    master.write_all(b"\x1b[<0;5;1m").unwrap();
    master.flush().unwrap();
    wait_for_output(
        &mut master,
        b"\x1b]52;c;aGVsbG8=\x07",
        Duration::from_secs(5),
    );

    // A same-cell click is local and must never inject mouse input remote.
    let _ = read_available(&mut master);
    master.write_all(b"\x1b[<0;2;1M").unwrap();
    master.flush().unwrap();
    wait_for_output(&mut master, b"\x1b[?2026l", Duration::from_secs(5));
    let _ = read_available(&mut master);
    master.write_all(b"\x1b[<0;2;1m").unwrap();
    master.flush().unwrap();
    wait_for_output(&mut master, b"\x1b[?2026l", Duration::from_secs(5));
    assert_file_stays_without(&capture, "terminal.mouse", Duration::from_millis(100));

    // Pointer coordinates beyond the pane clamp to the visible edge. With the
    // row's padding trimmed, selecting from column 1 beyond column 100 copies
    // the complete visible text rather than only the anchor character.
    let _ = read_available(&mut master);
    master.write_all(b"\x1b[<0;1;1M\x1b[<32;150;1M").unwrap();
    master.flush().unwrap();
    wait_for_output(&mut master, b"\x1b[7mhello world", Duration::from_secs(5));
    master.write_all(b"\x1b[<0;150;1m").unwrap();
    master.flush().unwrap();
    wait_for_output(
        &mut master,
        b"\x1b]52;c;aGVsbG8gd29ybGQ=\x07",
        Duration::from_secs(5),
    );
    assert_file_stays_without(&capture, "terminal.mouse", Duration::from_millis(100));

    unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) };
    let status = wait_for_exit(&mut child.0, Duration::from_secs(5));
    assert!(status.success(), "wrapper exited with {status}");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn repaint_during_local_drag_cancels_copy_for_rest_of_gesture() {
    let dir = temp_dir();
    write_fake_ssh(&dir);
    let capture = dir.join("control-input.jsonl");
    let frames = dir.join("frames.fifo");
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&capture)
        .unwrap();
    create_fifo(&frames);

    let (mut master, slave) = open_pty(100, 40);
    let path = format!(
        "{}:{}",
        dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-mirror"));
    command
        .args([
            "pane",
            "fake-host",
            "w1:p1",
            "--always-control",
            "--cols",
            "100",
            "--rows",
            "40",
        ])
        .env("PATH", path)
        .env("MIRROR_TEST_CAPTURE", &capture)
        .env("MIRROR_TEST_FRAME_FIFO", &frames)
        .env("MIRROR_TEST_STATE", "local")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    let mut child = ChildGuard(command.spawn().unwrap());

    let initial = wait_for_output(
        &mut master,
        b"\x1b[1;12H\x1b[?25h\x1b[?2026l",
        Duration::from_secs(5),
    );
    assert!(contains_bytes(&initial, b"hello world"));

    master.write_all(b"\x1b[<0;1;1M\x1b[<32;5;1M").unwrap();
    master.flush().unwrap();
    wait_for_output(&mut master, b"\x1b[7mhello", Duration::from_secs(5));

    // Repainting a selected character invalidates the snapshot. The active
    // gesture must remain cancelled through release: no stale clipboard write
    // and no remote mouse event.
    inject_frame(&frames, 2, "\x1b[1;1HHELLO world", Duration::from_secs(5));
    let repaint = wait_for_output(&mut master, b"HELLO world", Duration::from_secs(5));
    assert!(!contains_bytes(&repaint, b"\x1b]52;"), "{repaint:?}");
    let _ = read_available(&mut master);

    master.write_all(b"\x1b[<0;5;1m").unwrap();
    master.flush().unwrap();
    let release = wait_for_output(&mut master, b"\x1b[?2026l", Duration::from_secs(5));
    assert!(!contains_bytes(&release, b"\x1b]52;"), "{release:?}");
    assert_file_stays_without(&capture, "terminal.mouse", Duration::from_millis(100));

    unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) };
    let status = wait_for_exit(&mut child.0, Duration::from_secs(5));
    assert!(status.success(), "wrapper exited with {status}");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn controlled_mouse_reporting_state_forwards_structured_drag_instead_of_selecting() {
    let dir = temp_dir();
    write_fake_ssh(&dir);
    let capture = dir.join("control-input.jsonl");
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&capture)
        .unwrap();

    let (mut master, slave) = open_pty(100, 40);
    let path = format!(
        "{}:{}",
        dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-mirror"));
    command
        .args([
            "pane",
            "fake-host",
            "w1:p1",
            "--always-control",
            "--cols",
            "100",
            "--rows",
            "40",
        ])
        .env("PATH", path)
        .env("MIRROR_TEST_CAPTURE", &capture)
        .env("MIRROR_TEST_STATE", "remote")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    let mut child = ChildGuard(command.spawn().unwrap());

    let initial = wait_for_output(
        &mut master,
        b"\x1b[1;12H\x1b[?25h\x1b[?2026l",
        Duration::from_secs(5),
    );
    assert!(contains_bytes(&initial, b"hello world"));
    assert_remote_state_applied_before_mouse(&initial);

    master
        .write_all(b"\x1b[<0;1;1M\x1b[<32;5;1M\x1b[<0;5;1m")
        .unwrap();
    master.flush().unwrap();
    let captured = wait_for_file_occurrences(&capture, "terminal.mouse", 3, Duration::from_secs(5));
    assert_eq!(captured.matches("terminal.mouse").count(), 3, "{captured}");
    assert!(captured.contains(r#""kind":"down""#), "{captured}");
    assert!(captured.contains(r#""kind":"drag""#), "{captured}");
    assert!(captured.contains(r#""kind":"up""#), "{captured}");
    assert!(captured.contains(r#""button":"left""#), "{captured}");
    assert!(captured.contains(r#""column":4"#), "{captured}");
    assert!(!captured.contains("terminal.input"), "{captured}");
    assert!(!captured.contains("terminal.scroll"), "{captured}");

    master
        .write_all(b"\x1b[<2;3;2M\x1b[<2;3;2m\x1b[<35;4;2M")
        .unwrap();
    master.flush().unwrap();
    let captured = wait_for_file_occurrences(&capture, "terminal.mouse", 6, Duration::from_secs(5));
    assert!(captured.contains(r#""button":"right""#), "{captured}");
    assert!(captured.contains(r#""kind":"moved""#), "{captured}");
    thread::sleep(Duration::from_millis(50));
    let output = read_available(&mut master);
    assert!(!contains_bytes(&output, b"\x1b]52;"), "{output:?}");
    assert!(!contains_bytes(&output, b"\x1b[7mhello"), "{output:?}");

    unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) };
    let status = wait_for_exit(&mut child.0, Duration::from_secs(5));
    assert!(status.success(), "wrapper exited with {status}");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn controlled_partial_or_pixel_mouse_state_never_forwards_terminal_mouse() {
    for (state, warning) in [
        ("missing", "mouse state unavailable"),
        ("missing-pixel", "mouse state unavailable"),
        ("null-pixel", "mouse state unavailable"),
        ("wrong-pixel", "mouse state unavailable"),
        ("missing-any", "mouse state unavailable"),
        ("null-any", "mouse state unavailable"),
        ("wrong-any", "mouse state unavailable"),
        ("pixel", "pixel mouse mode is not supported"),
    ] {
        let dir = temp_dir();
        write_fake_ssh(&dir);
        let capture = dir.join("control-input.jsonl");
        File::create(&capture).unwrap();

        let (mut master, slave) = open_pty(100, 40);
        let path = format!(
            "{}:{}",
            dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-mirror"));
        command
            .args([
                "pane",
                "fake-host",
                "w1:p1",
                "--always-control",
                "--cols",
                "100",
                "--rows",
                "40",
            ])
            .env("PATH", path)
            .env("MIRROR_TEST_CAPTURE", &capture)
            .env("MIRROR_TEST_STATE", state)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        let mut child = ChildGuard(command.spawn().unwrap());

        wait_for_output(
            &mut master,
            b"\x1b[1;12H\x1b[?25h\x1b[?2026l",
            Duration::from_secs(5),
        );
        master
            .write_all(b"\x1b[<0;1;1M\x1b[<32;5;1M\x1b[<0;5;1m")
            .unwrap();
        master.flush().unwrap();
        wait_for_output(&mut master, warning.as_bytes(), Duration::from_secs(5));
        assert_file_stays_without(&capture, "terminal.mouse", Duration::from_millis(100));

        unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) };
        let status = wait_for_exit(&mut child.0, Duration::from_secs(5));
        assert!(
            status.success(),
            "state={state}: wrapper exited with {status}"
        );
        let _ = fs::remove_dir_all(dir);
    }
}

#[test]
fn state_change_does_not_split_an_active_mouse_gesture() {
    let dir = temp_dir();
    write_fake_ssh(&dir);
    let capture = dir.join("control-input.jsonl");
    let frames = dir.join("frames.fifo");
    File::create(&capture).unwrap();
    create_fifo(&frames);

    let (mut master, slave) = open_pty(100, 40);
    let path = format!(
        "{}:{}",
        dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-mirror"));
    command
        .args([
            "pane",
            "fake-host",
            "w1:p1",
            "--always-control",
            "--cols",
            "100",
            "--rows",
            "40",
        ])
        .env("PATH", path)
        .env("MIRROR_TEST_CAPTURE", &capture)
        .env("MIRROR_TEST_FRAME_FIFO", &frames)
        .env("MIRROR_TEST_STATE", "remote")
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    let mut child = ChildGuard(command.spawn().unwrap());

    let initial = wait_for_output(
        &mut master,
        b"\x1b[1;12H\x1b[?25h\x1b[?2026l",
        Duration::from_secs(5),
    );
    assert_remote_state_applied_before_mouse(&initial);
    master.write_all(b"\x1b[<0;1;1M").unwrap();
    master.flush().unwrap();
    wait_for_file(&capture, r#""kind":"down""#, Duration::from_secs(5));

    inject_line(
        &frames,
        r#"{"type":"terminal.state","mouse_reporting":false,"mouse_pixel_reporting":false,"mouse_any_motion":false,"alternate_screen":false,"application_cursor":false}"#,
        Duration::from_secs(5),
    );
    wait_for_output(&mut master, b"\x1b[?1l", Duration::from_secs(5));

    // The down started remote, so drag and release remain remote even after the
    // state update. A new gesture uses the new local state.
    master.write_all(b"\x1b[<32;5;1M\x1b[<0;5;1m").unwrap();
    master.flush().unwrap();
    let captured = wait_for_file_occurrences(&capture, "terminal.mouse", 3, Duration::from_secs(5));
    assert!(captured.contains(r#""kind":"drag""#), "{captured}");
    assert!(captured.contains(r#""kind":"up""#), "{captured}");

    master
        .write_all(b"\x1b[<0;1;1M\x1b[<32;5;1M\x1b[<0;5;1m")
        .unwrap();
    master.flush().unwrap();
    wait_for_output(
        &mut master,
        b"\x1b]52;c;aGVsbG8=\x07",
        Duration::from_secs(5),
    );
    thread::sleep(Duration::from_millis(100));
    let captured = fs::read_to_string(&capture).unwrap();
    assert_eq!(captured.matches("terminal.mouse").count(), 3, "{captured}");

    unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) };
    let status = wait_for_exit(&mut child.0, Duration::from_secs(5));
    assert!(status.success(), "wrapper exited with {status}");
    let _ = fs::remove_dir_all(dir);
}
