// herdr-mirror pane wrapper (data plane).
//
// Runs inside a local herdr pane and shows a remote herdr pane's terminal,
// live, over ssh. Read-only observe by default; escalates to a writable
// control session when the user types and releases back to observe.
//
//   herdr-mirror pane <ssh-target> <pane-target> [options]
//
// options:
//   --remote-bin PATH   remote herdr binary (default ~/.local/bin/herdr)
//   --cols N --rows N   observe request size (default 240x72; must be >= the
//                       remote PTY size or the server clips bottom rows away)
//   --dump              headless mode: print plain-text screen per frame
//   --session NAME      remote named session (passed as --session to herdr)
//   --control-idle N    auto-release control after N seconds idle (default 60)
//
// Every stream gets its own direct ssh connection (no shared ControlMaster):
// isolated, and nothing persists to go stale on a flaky network.
//
// One owner of all state, message-driven: frames, keystrokes, timers, and
// ssh-child exits arrive on one channel; a session generation number tags
// every message so stale ones are dropped.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::util::{err, Result};
use crate::grid::{Grid, Renderer};

// ---------------------------------------------------------------------------
// args

#[derive(Debug, Clone)]
pub struct Args {
    pub ssh_target: String,
    pub pane_target: String,
    pub remote_bin: String,
    pub cols: usize,
    pub rows: usize,
    pub dump: bool,
    pub session: Option<String>,
    /// auto-release control after this much input idle; 0 disables
    pub control_idle_secs: u64,
    /// --cols/--rows are the remote pane's real size (plus margin), use as-is
    pub size_fixed: bool,
}

pub fn parse_args(argv: &[String]) -> Result<Args> {
    let mut args = Args {
        ssh_target: String::new(),
        pane_target: String::new(),
        remote_bin: "~/.local/bin/herdr".into(),
        cols: 240,
        rows: 72,
        dump: false,
        session: None,
        control_idle_secs: 60,
        size_fixed: false,
    };
    let mut positional: Vec<String> = Vec::new();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        let mut next = |flag: &str| -> Result<String> {
            it.next().cloned().ok_or_else(|| err(format!("{flag} needs a value")))
        };
        match a.as_str() {
            "--remote-bin" => args.remote_bin = next("--remote-bin")?,
            "--cols" => {
                args.cols = next("--cols")?.parse().map_err(|_| err("--cols must be a number"))?;
                args.size_fixed = true;
            }
            "--rows" => {
                args.rows = next("--rows")?.parse().map_err(|_| err("--rows must be a number"))?;
                args.size_fixed = true;
            }
            "--session" => args.session = Some(next("--session")?),
            "--control-idle" => {
                args.control_idle_secs =
                    next("--control-idle")?.parse().map_err(|_| err("--control-idle must be a number"))?
            }
            "--dump" => args.dump = true,
            other if other.starts_with('-') => return Err(err(format!("unknown option: {other}"))),
            other => positional.push(other.to_string()),
        }
    }
    if positional.len() != 2 {
        return Err(err(
            "usage: herdr-mirror pane <ssh-target> <pane-target> [--remote-bin PATH] [--cols N --rows N] [--dump]",
        ));
    }
    args.ssh_target = positional.remove(0);
    args.pane_target = positional.remove(0);
    Ok(args)
}

// ---------------------------------------------------------------------------
// remote session: one ssh child running observe or control

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Observe,
    Control,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Observe => "observe",
            Mode::Control => "control",
        }
    }
}

#[derive(Debug, Deserialize)]
struct Frame {
    #[serde(rename = "type")]
    kind: String,
    seq: Option<u64>,
    full: Option<bool>,
    width: Option<usize>,
    height: Option<usize>,
    bytes: Option<String>,
    reason: Option<String>,
}

enum Msg {
    Frame { gen: u64, frame: Frame },
    SessionExit { gen: u64, mode: Mode, reason: String, uptime: Duration },
    Stdin(Vec<u8>),
}

struct Session {
    gen: u64,
    mode: Mode,
    pid: i32,
    stdin: ChildStdin,
}

/// POSIX single-quote: an embedded ' can't break the remote shell parse.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn spawn_session(args: &Args, mode: Mode, cols: usize, rows: usize, gen: u64, tx: mpsc::Sender<Msg>) -> Result<Session> {
    let session_flag = args
        .session
        .as_ref()
        .map(|s| format!(" --session {}", sh_quote(s)))
        .unwrap_or_default();
    // remote_bin stays unquoted on purpose: the default ~/.local/bin/herdr
    // relies on remote-shell tilde expansion
    let cmd = format!(
        "exec {}{} terminal session {} {} --cols {} --rows {}",
        args.remote_bin,
        session_flag,
        mode.as_str(),
        sh_quote(&args.pane_target),
        cols,
        rows
    );
    let mut child = tokio::process::Command::new("ssh")
        .args(crate::remote::SSH_COMMON_OPTS)
        .arg(&args.ssh_target)
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pid = child.id().map(|p| p as i32).unwrap_or(0);
    let stdin = child.stdin.take().ok_or_else(|| err("no child stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| err("no child stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| err("no child stderr"))?;
    let started = Instant::now();

    tokio::spawn(async move {
        // ssh errors arrive on stderr; the server's failure reason arrives as
        // a terminal.closed frame on STDOUT — capture both
        let err_tail: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let err_tail2 = err_tail.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let mut buf = err_tail2.lock().unwrap();
                buf.push_str(&l);
                buf.push('\n');
                if buf.len() > 400 {
                    let tail: String = buf.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
                    *buf = tail;
                }
            }
        });
        let mut close_reason = String::new();
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(frame) = serde_json::from_str::<Frame>(&line) else { continue };
            if frame.kind == "terminal.closed" {
                if let Some(r) = &frame.reason {
                    close_reason = r.clone();
                }
            }
            if tx.send(Msg::Frame { gen, frame }).await.is_err() {
                break;
            }
        }
        let _ = child.wait().await;
        stderr_task.abort();
        let tail = err_tail.lock().unwrap().trim().to_string();
        let reason = if close_reason.is_empty() { tail } else { close_reason };
        let _ = tx.send(Msg::SessionExit { gen, mode, reason, uptime: started.elapsed() }).await;
    });

    Ok(Session { gen, mode, pid, stdin })
}

// ---------------------------------------------------------------------------
// terminal plumbing

fn term_size() -> (usize, usize) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            return (ws.ws_col as usize, ws.ws_row as usize);
        }
    }
    (80, 24)
}

struct RawMode {
    orig: libc::termios,
}

impl RawMode {
    fn enable() -> Option<RawMode> {
        unsafe {
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut orig) != 0 {
                return None;
            }
            let mut raw = orig;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(RawMode { orig })
        }
    }

    fn restore(&self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.orig);
        }
    }
}

fn write_stdout(s: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

/// One SGR mouse event: ESC [ < btn ; col ; row (M|m). Returns (btn, col, row,
/// press, total len) for a sequence starting at `bytes[at]`.
fn parse_mouse(bytes: &[u8], at: usize) -> Option<(u32, u32, u32, bool, usize)> {
    let rest = &bytes[at..];
    if rest.len() < 6 || rest[0] != 0x1b || rest[1] != b'[' || rest[2] != b'<' {
        return None;
    }
    let mut nums = [0u32; 3];
    let mut n = 0usize;
    let mut i = 3usize;
    let mut have_digit = false;
    while i < rest.len() && n < 3 {
        match rest[i] {
            b'0'..=b'9' => {
                // saturate: garbage digit runs on stdin must not overflow-panic
                nums[n] = nums[n].saturating_mul(10).saturating_add((rest[i] - b'0') as u32);
                have_digit = true;
                i += 1;
            }
            b';' if n < 2 && have_digit => {
                n += 1;
                have_digit = false;
                i += 1;
            }
            b'M' | b'm' if n == 2 && have_digit => {
                return Some((nums[0], nums[1], nums[2], rest[i] == b'M', i + 1));
            }
            _ => return None,
        }
    }
    None
}

fn contains_wheel_press(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if let Some((btn, _, _, press, len)) = parse_mouse(bytes, i) {
            if press && (btn == 64 || btn == 65) {
                return true;
            }
            i += len;
        } else {
            i += 1;
        }
    }
    false
}

fn has_mouse_seq(bytes: &[u8]) -> bool {
    bytes.windows(3).any(|w| w == [0x1b, b'[', b'<'])
}

// ---------------------------------------------------------------------------
// the wrapper state machine

const BACKOFF: [u64; 4] = [1000, 2000, 5000, 10000];
const SWITCH_GAP: Duration = Duration::from_millis(200);
const QUICK_CONTROL_FAILURE: Duration = Duration::from_secs(4);

struct App {
    args: Args,
    tty: bool,
    grid: Grid,
    renderer: Renderer,
    tx: mpsc::Sender<Msg>,

    mode: Mode,
    /// in-flight mode switch (guards fast re-entry)
    switching_to: Option<Mode>,
    switch_at: Option<Instant>,
    session: Option<Session>,
    next_gen: u64,

    backoff_idx: usize,
    reconnect_at: Option<(Instant, Mode)>,
    /// consecutive quick control failures → fall back to observe
    control_failures: u32,
    control_sticky: bool,
    pending_input: Vec<Vec<u8>>,
    last_input: Instant,
    hint_clear_at: Option<Instant>,
}

impl App {
    fn paint(&mut self) {
        if !self.tty {
            return;
        }
        let (cols, rows) = term_size();
        let out = self.renderer.paint(&self.grid, cols, rows);
        write_stdout(&out);
    }

    fn hint(&mut self, text: &str) {
        self.renderer.status(text);
        self.paint();
        self.hint_clear_at = Some(Instant::now() + Duration::from_millis(1500));
    }

    fn observe_size(&self) -> (usize, usize) {
        // must request >= the remote PTY size or the server clips its bottom
        // rows; daemon-passed sizes already include a margin
        if self.args.size_fixed {
            return (self.args.cols, self.args.rows);
        }
        let (c, r) = if self.tty { term_size() } else { (0, 0) };
        (self.args.cols.max(c), self.args.rows.max(r))
    }

    /// Stop the child (clean release first for control) — never leave an
    /// orphan holding the remote attach lock.
    fn stop_session(&mut self) {
        if let Some(mut s) = self.session.take() {
            tokio::spawn(async move {
                if s.mode == Mode::Control {
                    let _ = s.stdin.write_all(b"{\"type\":\"terminal.release\"}\n").await;
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
                unsafe { libc::kill(s.pid, libc::SIGTERM) };
            });
        }
    }

    async fn connect(&mut self, m: Mode) {
        self.mode = m;
        let (cols, rows) = match m {
            Mode::Observe => self.observe_size(),
            Mode::Control => term_size(),
        };
        if let Some(s) = self.session.take() {
            unsafe { libc::kill(s.pid, libc::SIGTERM) };
        }
        self.next_gen += 1;
        match spawn_session(&self.args, m, cols, rows, self.next_gen, self.tx.clone()) {
            Ok(mut s) => {
                if m == Mode::Control {
                    self.last_input = Instant::now();
                    // keystrokes typed while the control session was spinning up
                    for buf in std::mem::take(&mut self.pending_input) {
                        let line = json!({ "type": "terminal.input", "bytes": B64.encode(&buf) }).to_string() + "\n";
                        let _ = s.stdin.write_all(line.as_bytes()).await;
                    }
                } else {
                    self.pending_input.clear();
                }
                self.session = Some(s);
                self.renderer.status(if m == Mode::Control { "CONTROL — ctrl+\\ to release" } else { "" });
            }
            Err(e) => self.schedule_reconnect(m, &e.to_string()),
        }
    }

    fn schedule_reconnect(&mut self, m: Mode, reason: &str) {
        let delay = BACKOFF[self.backoff_idx.min(BACKOFF.len() - 1)];
        self.backoff_idx += 1;
        let suffix = if reason.is_empty() { String::new() } else { format!(" — {reason}") };
        self.renderer
            .status(&format!("reconnecting in {}s ({}){suffix}", delay / 1000, m.as_str()));
        self.paint();
        self.reconnect_at = Some((Instant::now() + Duration::from_millis(delay), m));
    }

    fn switch_mode(&mut self, m: Mode) {
        // already settled or scheduled — don't restart. Without this guard,
        // fast typing during the 200ms connect gap would spawn one control
        // ssh per keystroke, all racing to attach the same terminal.
        if self.switching_to == Some(m) || (self.switching_to.is_none() && self.mode == m) {
            return;
        }
        self.reconnect_at = None;
        self.switching_to = Some(m);
        self.stop_session();
        self.renderer.invalidate();
        // immediate feedback for the mode-switch gap (stop + 200ms + reconnect)
        self.renderer.status(if m == Mode::Control { "taking control…" } else { "releasing…" });
        self.paint();
        self.switch_at = Some(Instant::now() + SWITCH_GAP);
    }

    fn handle_frame(&mut self, gen: u64, frame: Frame) {
        if self.session.as_ref().map(|s| s.gen) != Some(gen) {
            return; // stale frame from a replaced session
        }
        if frame.kind == "terminal.closed" {
            let suffix = frame.reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default();
            self.renderer.status(&format!("remote terminal closed{suffix}"));
            self.paint();
            return;
        }
        if frame.kind != "terminal.frame" {
            return;
        }
        let Some(bytes) = &frame.bytes else { return };
        self.backoff_idx = 0;
        self.renderer.status("");
        self.grid
            .resize(frame.width.unwrap_or(self.grid.width), frame.height.unwrap_or(self.grid.height));
        if frame.full == Some(true) {
            self.grid.clear();
        }
        if let Ok(decoded) = B64.decode(bytes) {
            self.grid.apply(&String::from_utf8_lossy(&decoded));
        }
        if self.args.dump {
            let lines: Vec<String> = self.grid.text_lines().into_iter().filter(|l| !l.is_empty()).collect();
            println!(
                "--- frame seq={:?} full={:?} {}x{} ---\n{}",
                frame.seq,
                frame.full,
                frame.width.unwrap_or(0),
                frame.height.unwrap_or(0),
                lines.join("\n")
            );
        } else {
            self.paint();
        }
    }

    fn handle_exit(&mut self, gen: u64, exited_mode: Mode, reason: String, uptime: Duration) {
        if self.session.as_ref().map(|s| s.gen) != Some(gen) {
            return; // an old child we already replaced/killed
        }
        self.session = None;
        let reason_line =
            reason.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("").to_string();
        // control that dies quickly twice is failing (refused/dropped): fall
        // back to observe so the pane stays viewable; a keystroke retries
        if exited_mode == Mode::Control {
            self.control_failures = if uptime < QUICK_CONTROL_FAILURE { self.control_failures + 1 } else { 0 };
            if self.control_failures >= 2 {
                self.control_failures = 0;
                self.control_sticky = true;
                self.switch_mode(Mode::Observe);
                let suffix = if reason_line.is_empty() { String::new() } else { format!(" ({reason_line})") };
                self.hint(&format!("control unavailable — viewing only{suffix}; type to retry"));
                return;
            }
        }
        self.schedule_reconnect(exited_mode, &reason_line);
    }

    async fn send(&mut self, msg: serde_json::Value) {
        if let Some(s) = self.session.as_mut() {
            let line = msg.to_string() + "\n";
            let _ = s.stdin.write_all(line.as_bytes()).await;
        }
    }

    async fn handle_stdin(&mut self, buf: Vec<u8>) {
        if self.mode == Mode::Observe || self.switching_to == Some(Mode::Observe) {
            // no quit key: the wrapper's lifecycle belongs to the hosting pane
            if has_mouse_seq(&buf) {
                // wheel escalates only after a soft release; a stray wheel
                // while glancing shouldn't grab the remote's lock
                if contains_wheel_press(&buf) {
                    if self.control_sticky {
                        self.control_sticky = false;
                        self.switch_mode(Mode::Control);
                    } else {
                        self.hint("read-only — type to take control");
                    }
                }
                return;
            }
            // any keystroke takes control and is delivered once the session is up
            self.control_sticky = false;
            self.pending_input.push(buf);
            self.switch_mode(Mode::Control);
            return;
        }

        // control mode
        self.last_input = Instant::now();
        if buf.len() == 1 && buf[0] == 0x1c {
            // ctrl+\ — manual release is deliberate: no wheel auto-retake
            self.control_sticky = false;
            self.switch_mode(Mode::Observe);
            return;
        }
        if self.switching_to == Some(Mode::Control) || self.session.is_none() {
            // spinning up or awaiting reconnect: queue the keystroke (flushed
            // on connect) and, if in backoff, reconnect now
            self.pending_input.push(buf);
            if let Some((_, m)) = self.reconnect_at {
                self.reconnect_at = Some((Instant::now(), m));
            }
            return;
        }
        // wheel becomes a semantic scroll (server decides app vs scrollback);
        // clicks/drags pass through to the remote pty as raw input
        let mut rest: Vec<u8> = Vec::with_capacity(buf.len());
        let mut i = 0usize;
        let mut scrolls: Vec<serde_json::Value> = Vec::new();
        while i < buf.len() {
            if let Some((btn, x, y, press, len)) = parse_mouse(&buf, i) {
                if press && (btn == 64 || btn == 65) {
                    scrolls.push(json!({
                        "type": "terminal.scroll",
                        "direction": if btn == 64 { "up" } else { "down" },
                        "lines": 3,
                        "source": "wheel",
                        "column": x.saturating_sub(1),
                        "row": y.saturating_sub(1),
                        "modifiers": 0,
                    }));
                } else {
                    rest.extend_from_slice(&buf[i..i + len]);
                }
                i += len;
            } else {
                rest.push(buf[i]);
                i += 1;
            }
        }
        for s in scrolls {
            self.send(s).await;
        }
        if !rest.is_empty() {
            let msg = json!({ "type": "terminal.input", "bytes": B64.encode(&rest) });
            self.send(msg).await;
        }
    }
}

// ---------------------------------------------------------------------------
// main

pub async fn run(args: Args) -> Result<()> {
    let tty = !args.dump && unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1;
    let raw = if tty {
        // 1002/1006: button-event mouse tracking with SGR encoding, so wheel and
        // clicks reach us instead of scrolling the hosting pane's scrollback
        write_stdout("\x1b[?1049h\x1b[2J\x1b[H\x1b[?1002h\x1b[?1006h");
        RawMode::enable()
    } else {
        None
    };

    let (tx, mut rx) = mpsc::channel::<Msg>(256);

    // stdin reader
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut stdin = tokio::io::stdin();
            let mut buf = [0u8; 1024];
            loop {
                match stdin.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(Msg::Stdin(buf[..n].to_vec())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }

    let mut app = App {
        args,
        tty,
        grid: Grid::new(),
        renderer: Renderer::new(),
        tx,
        mode: Mode::Observe,
        switching_to: None,
        switch_at: None,
        session: None,
        next_gen: 0,
        backoff_idx: 0,
        reconnect_at: None,
        control_failures: 0,
        control_sticky: false,
        pending_input: Vec::new(),
        last_input: Instant::now(),
        hint_clear_at: None,
    };
    app.connect(Mode::Observe).await;

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sighup = signal(SignalKind::hangup())?; // pane closed — don't orphan the ssh child
    let mut sigwinch = signal(SignalKind::window_change())?;

    loop {
        // earliest pending deadline: mode-switch gap, reconnect, hint clear, idle release
        let idle_at = (app.mode == Mode::Control
            && app.switching_to.is_none()
            && app.session.is_some()
            && app.args.control_idle_secs > 0)
            .then(|| app.last_input + Duration::from_secs(app.args.control_idle_secs));
        let sleep = crate::util::sleep_until_earliest([
            app.switch_at,
            app.reconnect_at.map(|(t, _)| t),
            app.hint_clear_at,
            idle_at,
        ]);

        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    None => break,
                    Some(Msg::Frame { gen, frame }) => app.handle_frame(gen, frame),
                    Some(Msg::SessionExit { gen, mode, reason, uptime }) => app.handle_exit(gen, mode, reason, uptime),
                    Some(Msg::Stdin(buf)) => app.handle_stdin(buf).await,
                }
            }
            _ = sigwinch.recv() => {
                app.renderer.invalidate();
                if app.mode == Mode::Control {
                    let (cols, rows) = term_size();
                    app.send(json!({ "type": "terminal.resize", "cols": cols, "rows": rows })).await;
                }
                app.paint();
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
            _ = sighup.recv() => break,
            _ = sleep => {
                let now = Instant::now();
                if app.switch_at.is_some_and(|t| t <= now) {
                    app.switch_at = None;
                    if let Some(m) = app.switching_to.take() {
                        app.connect(m).await; // pending input from the gap flushes here
                    }
                }
                if let Some((t, m)) = app.reconnect_at {
                    if t <= now {
                        app.reconnect_at = None;
                        app.connect(m).await;
                    }
                }
                if app.hint_clear_at.is_some_and(|t| t <= now) {
                    app.hint_clear_at = None;
                    app.renderer.status("");
                    app.paint();
                }
                if idle_at.is_some_and(|t| t <= now) && app.mode == Mode::Control && app.switching_to.is_none() {
                    app.control_sticky = true;
                    app.switch_mode(Mode::Observe);
                    app.hint("control released (idle) — type to retake");
                }
            }
        }
    }

    // clean shutdown: release control if held, kill the ssh child, restore tty
    if let Some(mut s) = app.session.take() {
        if s.mode == Mode::Control {
            let _ = s.stdin.write_all(b"{\"type\":\"terminal.release\"}\n").await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        unsafe { libc::kill(s.pid, libc::SIGTERM) };
    }
    if tty {
        write_stdout("\x1b[?1002l\x1b[?1006l\x1b[?25h\x1b[?1049l");
    }
    if let Some(raw) = raw {
        raw.restore();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_parsing() {
        let seq = b"\x1b[<64;10;5M";
        let (btn, x, y, press, len) = parse_mouse(seq, 0).unwrap();
        assert_eq!((btn, x, y, press, len), (64, 10, 5, true, seq.len()));
        assert!(contains_wheel_press(seq));
        assert!(!contains_wheel_press(b"\x1b[<0;3;4M")); // click, not wheel
        assert!(!contains_wheel_press(b"\x1b[<64;10;5m")); // release, not press
        assert!(has_mouse_seq(b"xx\x1b[<0;1;1Myy"));
        assert!(!has_mouse_seq(b"plain text"));
    }

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("w9:p1"), "'w9:p1'");
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
        // overflow-proof mouse params: 11 digits saturate instead of panicking
        let (_, x, _, _, _) = parse_mouse(b"\x1b[<64;99999999999;1M", 0).unwrap();
        assert_eq!(x, u32::MAX);
    }

    #[test]
    fn arg_parsing() {
        let argv: Vec<String> =
            ["work", "w9:p1", "--remote-bin", "/opt/herdr", "--cols", "176", "--rows", "66"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let a = parse_args(&argv).unwrap();
        assert_eq!(a.ssh_target, "work");
        assert_eq!(a.pane_target, "w9:p1");
        assert_eq!(a.remote_bin, "/opt/herdr");
        assert_eq!((a.cols, a.rows), (176, 66));
        assert!(a.size_fixed);
        assert!(parse_args(&["onlyone".to_string()]).is_err());
        assert!(parse_args(&["a".into(), "b".into(), "--visibility-file".into(), "x".into()]).is_err());
    }
}
