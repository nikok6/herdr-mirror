// herdr-mirror pane wrapper (data plane).
//
// Runs inside a local herdr pane and shows a remote herdr pane's terminal,
// live, over ssh. Read-only observe by default; escalates to a writable
// control session when the user types and releases back to observe.
//
//   herdr-mirror pane <ssh-target> <pane-target> [options]
//
// options:
//   --remote-bin PATH   remote herdr binary (default: PATH, then ~/.local/bin/herdr)
//   --cols N --rows N   observe request size (default 240x72; must be >= the
//                       remote PTY size or the server clips bottom rows away)
//   --dump              headless mode: print plain-text screen per frame
//   --session NAME      remote named session (passed as --session to herdr)
//   --control-idle N    auto-release control after N seconds idle (default 3600)
//   --always-control    start and stay in control: writable, no idle release,
//                       and sized to the local pane so it fills
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

use crate::grid::{Grid, Renderer, Selection};
use crate::mouse_input::{MouseInputItem, SgrMouseEvent, SgrMouseInputParser};
use crate::predict::Predictor;
use crate::util::{err, Result};

// ---------------------------------------------------------------------------
// args

#[derive(Debug, Clone)]
pub struct Args {
    pub ssh_target: String,
    pub pane_target: String,
    /// Configured remote herdr path. `None` = auto-resolve on the remote
    /// (PATH, then `~/.local/bin/herdr`). See `config::remote_bin_expr`.
    pub remote_bin: Option<String>,
    pub cols: usize,
    pub rows: usize,
    pub dump: bool,
    pub session: Option<String>,
    /// auto-release control after this much input idle; 0 disables
    pub control_idle_secs: u64,
    /// --cols/--rows are the remote pane's real size (plus margin), use as-is
    pub size_fixed: bool,
    /// start and stay in control: writable, no idle release, and sized to the
    /// local pane so it fills. Set by the daemon from per-host config.
    pub always_control: bool,
    /// container to exec into instead of ssh. `None` = ssh host.
    pub container: Option<ContainerArg>,
}

/// How the pane process should reach its container. The daemon passes a *ref*,
/// not a resolved id: the pane may outlive a rebuild, and ids change while the
/// folder label does not.
#[derive(Debug, Clone)]
pub struct ContainerArg {
    pub kind: crate::config::HostKind,
    pub docker_bin: String,
}

pub fn parse_args(argv: &[String]) -> Result<Args> {
    let mut args = Args {
        ssh_target: String::new(),
        pane_target: String::new(),
        remote_bin: None,
        cols: 240,
        rows: 72,
        dump: false,
        session: None,
        control_idle_secs: 3600,
        size_fixed: false,
        always_control: false,
        container: None,
    };
    let mut container_name: Option<String> = None;
    let mut container_folder: Option<String> = None;
    let mut docker_bin = "docker".to_string();
    let mut positional: Vec<String> = Vec::new();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        let mut next = |flag: &str| -> Result<String> {
            it.next()
                .cloned()
                .ok_or_else(|| err(format!("{flag} needs a value")))
        };
        match a.as_str() {
            "--remote-bin" => args.remote_bin = Some(next("--remote-bin")?),
            "--cols" => {
                args.cols = next("--cols")?
                    .parse()
                    .map_err(|_| err("--cols must be a number"))?;
                args.size_fixed = true;
            }
            "--rows" => {
                args.rows = next("--rows")?
                    .parse()
                    .map_err(|_| err("--rows must be a number"))?;
                args.size_fixed = true;
            }
            "--session" => args.session = Some(next("--session")?),
            "--control-idle" => {
                args.control_idle_secs = next("--control-idle")?
                    .parse()
                    .map_err(|_| err("--control-idle must be a number"))?
            }
            "--always-control" => args.always_control = true,
            // Accepted for rolling upgrades and restored launch argv from
            // releases that polled foreground processes through the daemon's
            // SSH ControlMaster. Authoritative terminal.state made it unused.
            "--ctl-path" => {
                let _ = next("--ctl-path")?;
            }
            "--container" => container_name = Some(next("--container")?),
            "--container-folder" => container_folder = Some(next("--container-folder")?),
            "--docker-bin" => docker_bin = next("--docker-bin")?,
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
    args.container = match (container_name, container_folder) {
        (Some(_), Some(_)) => return Err(err("--container and --container-folder are exclusive")),
        (Some(n), None) => Some(ContainerArg {
            kind: crate::config::HostKind::DockerContainer(n),
            docker_bin,
        }),
        (None, Some(f)) => Some(ContainerArg {
            kind: crate::config::HostKind::DockerFolder(f),
            docker_bin,
        }),
        (None, None) => None,
    };
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
    seq: Option<u64>,
    full: Option<bool>,
    width: Option<usize>,
    height: Option<usize>,
    bytes: Option<String>,
}

/// Input modes reported by the remote terminal session.
///
/// Every field is independently optional because older Herdr servers do not
/// emit `terminal.state`, and newer peers may add fields without coordinating
/// an upgrade with this plugin. Missing, null, and malformed input-mode fields
/// all mean "unknown"; unknown mouse state must never be guessed from the app
/// name or process tree.
#[derive(Debug, Clone, Default, PartialEq)]
struct TerminalSessionState {
    mouse_reporting: Option<bool>,
    mouse_pixel_reporting: Option<bool>,
    mouse_any_motion: Option<bool>,
    alternate_screen: Option<bool>,
    application_cursor: Option<bool>,
    /// Retained for forward compatibility. Scrollbar rendering is deliberately
    /// a separate change from mouse routing.
    scroll: Option<serde_json::Value>,
}

impl TerminalSessionState {
    fn from_value(value: &serde_json::Value) -> Self {
        Self {
            mouse_reporting: value.get("mouse_reporting").and_then(|v| v.as_bool()),
            mouse_pixel_reporting: value.get("mouse_pixel_reporting").and_then(|v| v.as_bool()),
            mouse_any_motion: value.get("mouse_any_motion").and_then(|v| v.as_bool()),
            alternate_screen: value.get("alternate_screen").and_then(|v| v.as_bool()),
            application_cursor: value.get("application_cursor").and_then(|v| v.as_bool()),
            scroll: value.get("scroll").filter(|v| !v.is_null()).cloned(),
        }
    }
}

enum Msg {
    Frame {
        gen: u64,
        frame: Frame,
    },
    State {
        gen: u64,
        state: TerminalSessionState,
    },
    Closed {
        gen: u64,
        reason: String,
    },
    SessionExit {
        gen: u64,
        mode: Mode,
        reason: String,
        uptime: Duration,
    },
    Stdin(Vec<u8>),
}

struct Session {
    gen: u64,
    mode: Mode,
    pid: i32,
    stdin: ChildStdin,
}

/// POSIX single-quote: an embedded ' can't break the remote shell parse.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn spawn_session(
    args: &Args,
    mode: Mode,
    cols: usize,
    rows: usize,
    gen: u64,
    tx: mpsc::Sender<Msg>,
) -> Result<Session> {
    let session_flag = args
        .session
        .as_ref()
        .map(|s| format!(" --session {}", sh_quote(s)))
        .unwrap_or_default();
    // Configured paths stay unquoted so remote-shell ~ expands; auto mode is an
    // `sh -c` resolver that takes the trailing words as "$@" (see
    // config::remote_bin_expr).
    let bin = crate::config::remote_bin_expr(args.remote_bin.as_deref());
    let cmd = format!(
        "exec {}{} terminal session {} {} --cols {} --rows {}",
        bin,
        session_flag,
        mode.as_str(),
        sh_quote(&args.pane_target),
        cols,
        rows
    );
    // ssh and docker differ only in how the command is carried; the streaming
    // contract (piped stdio, herdr's frames on stdout) is identical
    let mut builder = match &args.container {
        None => {
            let mut c = tokio::process::Command::new("ssh");
            c.args(crate::remote::SSH_COMMON_OPTS)
                .arg(&args.ssh_target)
                .arg(cmd);
            c
        }
        Some(ct) => {
            // resolve per spawn so a rebuilt container is picked up on
            // reconnect. Bounded: this runs on the pane's single-threaded
            // runtime, so a wedged Docker daemon must not be able to freeze
            // input, rendering or signal handling.
            let id =
                crate::docker::resolve_blocking(&ct.docker_bin, &ct.kind, Duration::from_secs(5))?;
            let mut c = tokio::process::Command::new(&ct.docker_bin);
            // `sh -c` not `-lc`: match ssh's non-login remote shell
            c.args(["exec", "-i", &id, "sh", "-c", &cmd]);
            c
        }
    };
    let mut child = builder
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
                    let tail: String = buf
                        .chars()
                        .rev()
                        .take(400)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    *buf = tail;
                }
            }
        });
        let mut close_reason = String::new();
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            match value.get("type").and_then(|kind| kind.as_str()) {
                Some("terminal.frame") => {
                    let Ok(frame) = serde_json::from_value::<Frame>(value) else {
                        continue;
                    };
                    if tx.send(Msg::Frame { gen, frame }).await.is_err() {
                        break;
                    }
                }
                Some("terminal.state") => {
                    // Parse every field independently. A malformed state update
                    // replaces the previous one with unknown values so stale
                    // `mouse_reporting: true` can never leak a local gesture to
                    // a newly non-mouse-aware application.
                    let state = TerminalSessionState::from_value(&value);
                    if tx.send(Msg::State { gen, state }).await.is_err() {
                        break;
                    }
                }
                Some("terminal.closed") => {
                    close_reason = value
                        .get("reason")
                        .and_then(|reason| reason.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let _ = tx
                        .send(Msg::Closed {
                            gen,
                            reason: close_reason.clone(),
                        })
                        .await;
                }
                _ => {}
            }
        }
        let _ = child.wait().await;
        stderr_task.abort();
        let tail = err_tail.lock().unwrap().trim().to_string();
        let reason = if close_reason.is_empty() {
            tail
        } else {
            close_reason
        };
        let _ = tx
            .send(Msg::SessionExit {
                gen,
                mode,
                reason,
                uptime: started.elapsed(),
            })
            .await;
    });

    Ok(Session {
        gen,
        mode,
        pid,
        stdin,
    })
}

// ---------------------------------------------------------------------------
// terminal plumbing

/// The layout herdr renders when a server has NO client attached: MIN_COLS x
/// MIN_ROWS from its headless server. Not a minimum — an attached client
/// smaller than this gets its real size — it is specifically the placeholder
/// used when nobody is watching.
///
/// Every derivation from it subtracts: sidebar, tab bar, splits, gaps, the
/// scrollbar gutter. So a pane born under that layout cannot EXCEED it on
/// either axis, which is what makes this a sound upper bound rather than a
/// shape to match. Shape matching is the trap: a phone lays out to the same
/// rectangle as the placeholder.
const HERDR_NO_CLIENT_LAYOUT: (usize, usize) = (80, 24);

/// Could this pane's size have come from a layout nobody is watching?
///
/// Strictly larger on either axis is provably a real viewport. `>` not `>=`:
/// herdr spawns restored panes at exactly 24x80, so `>=` would trust a
/// placeholder.
///
/// Consulted at birth to pick the initial mode. Deliberately NOT consulted on
/// the promotion path: a resize is taken as evidence of a client, which holds
/// in practice but is empirical rather than structural — herdr has one
/// clientless resize path (its first virtual render), so a pane created in the
/// instant before that render could in principle promote on a placeholder
/// resize. It self-heals the moment a client attaches.
fn size_is_trusted((cols, rows): (usize, usize)) -> bool {
    cols > HERDR_NO_CLIENT_LAYOUT.0 || rows > HERDR_NO_CLIENT_LAYOUT.1
}

/// Mode to open with. Split out from `run` so the composition is testable.
fn initial_mode(always_control: bool, size: (usize, usize)) -> Mode {
    if always_control && size_is_trusted(size) {
        Mode::Control
    } else {
        Mode::Observe
    }
}

fn term_size() -> (usize, usize) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0
            && ws.ws_col > 0
            && ws.ws_row > 0
        {
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

const MOUSE_INPUT_FLUSH_DELAY: Duration = Duration::from_millis(12);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseKind {
    Wheel { up: bool },
    Down(MouseButton),
    Drag(MouseButton),
    Up(MouseButton),
    Moved,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseButton {
    Left,
    Middle,
    Right,
}

impl MouseButton {
    fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Middle => "middle",
            Self::Right => "right",
        }
    }
}

/// Decode the SGR button bitfield while preserving modifier bits. Trackpads and
/// modified wheels do not necessarily arrive as exactly 64/65.
fn mouse_kind(event: &SgrMouseEvent) -> MouseKind {
    let button = event.button;
    let base = button & 0b11;
    if button & 64 != 0 {
        if !event.press {
            return MouseKind::Other;
        }
        return match base {
            0 => MouseKind::Wheel { up: true },
            1 => MouseKind::Wheel { up: false },
            _ => MouseKind::Other,
        };
    }
    let decoded_button = match base {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Middle),
        2 => Some(MouseButton::Right),
        _ => None,
    };
    if !event.press {
        decoded_button
            .map(MouseKind::Up)
            .unwrap_or(MouseKind::Other)
    } else if button & 32 != 0 {
        decoded_button
            .map(MouseKind::Drag)
            .unwrap_or(MouseKind::Moved)
    } else {
        decoded_button
            .map(MouseKind::Down)
            .unwrap_or(MouseKind::Other)
    }
}

/// Convert xterm SGR mouse modifier bits to the crossterm KeyModifiers bitset
/// expected by Herdr's terminal-session control protocol.
fn mouse_modifiers(button: u32) -> u8 {
    let shift = u8::from(button & 4 != 0);
    let alt = u8::from(button & 8 != 0) << 2;
    let control = u8::from(button & 16 != 0) << 1;
    shift | control | alt
}

fn cell_coordinate(one_based: u32) -> u16 {
    one_based.saturating_sub(1).min(u16::MAX as u32) as u16
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GestureOwner {
    Local,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseRouting {
    Local,
    Remote,
    Unknown,
    PixelUnsupported,
}

/// Authoritative terminal state is the only safe way to decide who owns a
/// gesture. Unknown state fails local; explicit pixel reporting also stays
/// local because this wrapper receives cell coordinates, not pixels.
fn mouse_routing(state: &TerminalSessionState) -> MouseRouting {
    match state.mouse_reporting {
        None => MouseRouting::Unknown,
        Some(false) => MouseRouting::Local,
        Some(true) if state.mouse_pixel_reporting == Some(true) => MouseRouting::PixelUnsupported,
        Some(true)
            if state.mouse_pixel_reporting == Some(false) && state.mouse_any_motion.is_some() =>
        {
            MouseRouting::Remote
        }
        Some(true) => MouseRouting::Unknown,
    }
}

fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", B64.encode(text.as_bytes()))
}

/// An anchored mouse-down is only a possible selection. Do not flash its cell
/// while an ordinary click is waiting for release; highlighting starts once a
/// drag has actually moved the cursor away from the anchor.
fn visible_selection(selection: Option<&Selection>, dragged: bool) -> Option<&Selection> {
    dragged.then_some(selection).flatten()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum MouseGrab {
    #[default]
    Off,
    Button,
    Any,
}

/// Control sessions keep at least button-event tracking even when remote mouse
/// reporting is off. Otherwise the hosting Herdr consumes wheel events as
/// local scrollback and this wrapper cannot turn them into semantic
/// `terminal.scroll` messages. Any-event tracking is enabled only when the
/// remote explicitly reports that it needs hover motion.
fn desired_mouse_grab(mode: Mode, state: &TerminalSessionState) -> MouseGrab {
    match mode {
        Mode::Control
            if mouse_routing(state) == MouseRouting::Remote
                && state.mouse_any_motion == Some(true) =>
        {
            MouseGrab::Any
        }
        Mode::Control => MouseGrab::Button,
        Mode::Observe if mouse_routing(state) == MouseRouting::Remote => MouseGrab::Button,
        Mode::Observe => MouseGrab::Off,
    }
}

/// Return only the DEC mode changes needed for one tracking transition.
///
/// Keep 1002 and 1003 mutually exclusive. Ghostty exposes both individual DEC
/// mode bits and a derived encoder mode; disabling the most recently enabled
/// mode can leave that derived mode at `none` even when the other DEC bit is
/// still set. Disable the old mode before enabling the new one so hosting
/// terminals can still encode wheel and button reports after a downgrade.
fn mouse_grab_transition(from: MouseGrab, to: MouseGrab) -> &'static str {
    match (from, to) {
        (MouseGrab::Off, MouseGrab::Button) => "\x1b[?1002h\x1b[?1006h",
        (MouseGrab::Off, MouseGrab::Any) => "\x1b[?1003h\x1b[?1006h",
        (MouseGrab::Button, MouseGrab::Off) => "\x1b[?1002l\x1b[?1006l",
        (MouseGrab::Button, MouseGrab::Any) => "\x1b[?1002l\x1b[?1003h",
        (MouseGrab::Any, MouseGrab::Off) => "\x1b[?1003l\x1b[?1006l",
        (MouseGrab::Any, MouseGrab::Button) => "\x1b[?1003l\x1b[?1002h",
        (MouseGrab::Off, MouseGrab::Off)
        | (MouseGrab::Button, MouseGrab::Button)
        | (MouseGrab::Any, MouseGrab::Any) => "",
    }
}

fn cursor_key_transition(current: bool, reported: Option<bool>) -> Option<(bool, &'static str)> {
    let want = reported?;
    (want != current).then_some((want, if want { "\x1b[?1h" } else { "\x1b[?1l" }))
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
    /// predictive local echo — draws keystrokes optimistically, frame-verified
    predict: Predictor,
    /// Authoritative input modes from this terminal-session generation. Reset
    /// on every reconnect; stale state must never decide a new gesture.
    terminal_state: TerminalSessionState,
    /// Avoid repeating compatibility warnings for every mouse report.
    unknown_state_warned: bool,
    pixel_mouse_warned: bool,
    /// Local DEC mouse tracking currently requested from the hosting pane.
    mouse_grab: MouseGrab,
    /// Incremental decoder because PTY reads may split one SGR report anywhere.
    mouse_input: SgrMouseInputParser,
    /// Flush a lone Escape or ambiguous CSI prefix promptly as keyboard input.
    /// A confirmed partial mouse report stays buffered until it can reassemble.
    mouse_input_flush_at: Option<Instant>,
    /// Visible-grid selection owned by this wrapper while it captures the mouse.
    selection: Option<Selection>,
    /// Grid captured at button-down. A same-size frame can repaint underneath
    /// an active gesture; copy from this snapshot only while selected text still
    /// matches the live grid.
    selection_source: Option<Grid>,
    /// Kept separately from normalized grid endpoints so dragging from the
    /// leading cell to the spacer half of one wide glyph still counts.
    selection_dragged: bool,
    /// A selected cell repainted before release. Ignore the rest of that
    /// physical gesture so later motion cannot silently start a new selection
    /// with different contents.
    selection_cancelled: bool,
    /// The owner is frozen at mouse-down so a terminal-state update cannot
    /// split one physical gesture between local selection and the remote app.
    gesture_owner: Option<GestureOwner>,
    gesture_button: Option<MouseButton>,
    /// whether the local pane is currently in application cursor mode (?1h), held
    /// to match the remote's so forwarded arrows arrive in the form it expects
    app_cursor_keys: bool,
}

impl App {
    fn paint(&mut self) {
        if !self.tty {
            return;
        }
        if self.predict.take_dirty() {
            // cleared predictions may have left ghost chars — full repaint
            self.renderer.invalidate();
        }
        let (cols, rows) = term_size();
        let mut out = self.renderer.paint_with_selection(
            &self.grid,
            cols,
            rows,
            visible_selection(self.selection.as_ref(), self.selection_dragged),
        );
        // inject the prediction overlay inside the synchronized-update block
        let overlay = self.predict.overlay(&self.grid, cols, rows);
        if !overlay.is_empty() {
            const SYNC_END: &str = "\x1b[?2026l";
            if let Some(pos) = out.rfind(SYNC_END) {
                out.insert_str(pos, &overlay);
            } else {
                out.push_str(&overlay);
            }
        }
        write_stdout(&out);
    }

    fn hint(&mut self, text: &str) {
        self.renderer.status(text);
        self.paint();
        self.hint_clear_at = Some(Instant::now() + Duration::from_millis(1500));
    }

    fn clear_selection(&mut self) -> bool {
        let had_selection = self.selection.take().is_some();
        self.selection_source = None;
        self.selection_dragged = false;
        self.selection_cancelled = false;
        self.gesture_owner = None;
        self.gesture_button = None;
        had_selection
    }

    fn cancel_selection_gesture(&mut self) {
        let owner = self.gesture_owner;
        let button = self.gesture_button;
        self.selection = None;
        self.selection_source = None;
        self.selection_dragged = false;
        self.gesture_owner = owner;
        self.gesture_button = button;
        self.selection_cancelled = owner.is_some();
    }

    fn invalidate_selection_during_gesture(&mut self) {
        if self.gesture_owner == Some(GestureOwner::Local) {
            self.cancel_selection_gesture();
        } else {
            // Remote ownership still has to survive until its release. There is
            // no local selection to preserve or cancel in that case.
            self.selection = None;
            self.selection_source = None;
            self.selection_dragged = false;
            self.selection_cancelled = false;
        }
    }

    fn clear_selection_without_splitting_gesture(&mut self) -> bool {
        let had_selection = self.selection.is_some();
        if self.gesture_owner.is_some() {
            self.invalidate_selection_during_gesture();
        } else {
            self.clear_selection();
        }
        had_selection
    }

    fn new_gesture_owner(&mut self) -> GestureOwner {
        match mouse_routing(&self.terminal_state) {
            MouseRouting::Remote => GestureOwner::Remote,
            MouseRouting::Local => GestureOwner::Local,
            MouseRouting::Unknown => {
                if !self.unknown_state_warned {
                    self.unknown_state_warned = true;
                    self.hint(
                        "mouse state unavailable — update Herdr on the remote; selecting locally",
                    );
                }
                GestureOwner::Local
            }
            MouseRouting::PixelUnsupported => {
                if !self.pixel_mouse_warned {
                    self.pixel_mouse_warned = true;
                    self.hint(
                        "pixel mouse mode is not supported by this mirror; selecting locally",
                    );
                }
                GestureOwner::Local
            }
        }
    }

    /// Keep button tracking in control mode so wheel events always reach this
    /// wrapper and become remote semantic scrolls. Mirror explicit remote hover
    /// mode with 1003 only while controlling. Only writes on a change.
    fn sync_mouse_grab(&mut self) {
        if !self.tty {
            return;
        }
        let want = desired_mouse_grab(self.mode, &self.terminal_state);
        if want == self.mouse_grab {
            return;
        }
        let transition = mouse_grab_transition(self.mouse_grab, want);
        self.mouse_grab = want;
        write_stdout(transition);
    }

    /// Match the local pane's cursor-key mode to the remote's, so the arrow bytes
    /// herdr hands us are already the ones the remote app expects.
    ///
    /// Frames carry no DEC modes (see grid.rs), so a remote app in application
    /// cursor mode (DECCKM, what terminfo `smkx` sets) never moves the local
    /// pane out of normal mode: herdr encodes Up as CSI A, we forward it
    /// verbatim, and the remote app is listening for SS3 A. Rather than rewrite
    /// the bytes in flight, put the LOCAL pane in the same mode and let herdr's
    /// own encoder produce the right form: it also covers Home/End and anything
    /// else whose encoding turns on this mode.
    fn sync_cursor_key_mode(&mut self) {
        if !self.tty {
            return;
        }
        if let Some((want, transition)) =
            cursor_key_transition(self.app_cursor_keys, self.terminal_state.application_cursor)
        {
            self.app_cursor_keys = want;
            write_stdout(transition);
        }
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
                    let _ = s
                        .stdin
                        .write_all(b"{\"type\":\"terminal.release\"}\n")
                        .await;
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
                unsafe { libc::kill(s.pid, libc::SIGTERM) };
            });
        }
    }

    async fn connect(&mut self, m: Mode) {
        self.mode = m;
        self.clear_selection();
        // Input modes belong to one session generation. Unknown is deliberately
        // local until the new session reports authoritative state.
        self.terminal_state = TerminalSessionState::default();
        self.sync_mouse_grab();
        self.sync_cursor_key_mode();
        // re-earn prediction confidence against the new session's frames
        self.predict = Predictor::new();
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
                        let line = json!({ "type": "terminal.input", "bytes": B64.encode(&buf) })
                            .to_string()
                            + "\n";
                        let _ = s.stdin.write_all(line.as_bytes()).await;
                    }
                } else {
                    self.pending_input.clear();
                }
                self.session = Some(s);
                // always-control has no release, so no "ctrl+\ to release" hint
                self.renderer
                    .status(if m == Mode::Control && !self.args.always_control {
                        "CONTROL — ctrl+\\ to release"
                    } else {
                        ""
                    });
            }
            Err(e) => self.schedule_reconnect(m, &e.to_string()),
        }
    }

    fn schedule_reconnect(&mut self, m: Mode, reason: &str) {
        let delay = BACKOFF[self.backoff_idx.min(BACKOFF.len() - 1)];
        self.backoff_idx += 1;
        let suffix = if reason.is_empty() {
            String::new()
        } else {
            format!(" — {reason}")
        };
        self.renderer.status(&format!(
            "reconnecting in {}s ({}){suffix}",
            delay / 1000,
            m.as_str()
        ));
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
        self.clear_selection();
        self.switching_to = Some(m);
        self.stop_session();
        self.renderer.invalidate();
        // immediate feedback for the mode-switch gap (stop + 200ms + reconnect)
        self.renderer.status(if m == Mode::Control {
            "taking control…"
        } else {
            "releasing…"
        });
        self.paint();
        self.switch_at = Some(Instant::now() + SWITCH_GAP);
    }

    fn handle_frame(&mut self, gen: u64, frame: Frame) {
        if self.session.as_ref().map(|s| s.gen) != Some(gen) {
            return; // stale frame from a replaced session
        }
        let Some(bytes) = &frame.bytes else { return };
        self.backoff_idx = 0;
        if self.hint_clear_at.is_none() {
            self.renderer.status("");
        }
        let next_width = frame.width.unwrap_or(self.grid.width);
        let next_height = frame.height.unwrap_or(self.grid.height);
        if (next_width, next_height) != (self.grid.width, self.grid.height) {
            self.invalidate_selection_during_gesture();
        }
        self.grid.resize(next_width, next_height);
        if frame.full == Some(true) {
            self.grid.clear();
        }
        if let Ok(decoded) = B64.decode(bytes) {
            self.grid.apply(&String::from_utf8_lossy(&decoded));
            // reconcile predictive echo against the authoritative frame
            self.predict.on_frame(&self.grid);
        }
        if !self.selection_source_matches() {
            if self.gesture_owner.is_some() {
                self.cancel_selection_gesture();
            } else {
                self.clear_selection();
            }
        }
        if self.args.dump {
            let lines: Vec<String> = self
                .grid
                .text_lines()
                .into_iter()
                .filter(|l| !l.is_empty())
                .collect();
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

    fn handle_state(&mut self, gen: u64, state: TerminalSessionState) {
        if self.session.as_ref().map(|session| session.gen) != Some(gen) {
            return;
        }
        self.terminal_state = state;
        self.sync_mouse_grab();
        self.sync_cursor_key_mode();
    }

    fn handle_closed(&mut self, gen: u64, reason: String) {
        if self.session.as_ref().map(|session| session.gen) != Some(gen) {
            return;
        }
        let suffix = if reason.is_empty() {
            String::new()
        } else {
            format!(": {reason}")
        };
        self.renderer
            .status(&format!("remote terminal closed{suffix}"));
        self.paint();
    }

    fn handle_exit(&mut self, gen: u64, exited_mode: Mode, reason: String, uptime: Duration) {
        if self.session.as_ref().map(|s| s.gen) != Some(gen) {
            return; // an old child we already replaced/killed
        }
        self.session = None;
        self.clear_selection();
        let reason_line = reason
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .unwrap_or("")
            .to_string();
        // control that dies quickly twice is failing (refused/dropped): fall
        // back to observe so the pane stays viewable; a keystroke retries
        if exited_mode == Mode::Control {
            self.control_failures = if uptime < QUICK_CONTROL_FAILURE {
                self.control_failures + 1
            } else {
                0
            };
            if self.control_failures >= 2 {
                self.control_failures = 0;
                self.control_sticky = true;
                self.switch_mode(Mode::Observe);
                let suffix = if reason_line.is_empty() {
                    String::new()
                } else {
                    format!(" ({reason_line})")
                };
                self.hint(&format!(
                    "control unavailable — viewing only{suffix}; type to retry"
                ));
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
        let items = self.mouse_input.push(&buf);
        // A bare Escape or ambiguous CSI prefix must not stick forever. Once
        // `ESC [ <` proves this is a mouse report, though, never time it out as
        // keyboard bytes: a scheduler delay between PTY reads must not inject
        // half a mouse sequence into the remote prompt.
        self.mouse_input_flush_at = (self.mouse_input.buffered_len() > 0
            && !self.mouse_input.pending_is_mouse_report())
        .then(|| Instant::now() + MOUSE_INPUT_FLUSH_DELAY);
        self.handle_input_items(items).await;
    }

    async fn flush_pending_mouse_input(&mut self) {
        self.mouse_input_flush_at = None;
        let items = self.mouse_input.flush_pending();
        self.handle_input_items(items).await;
    }

    async fn handle_input_items(&mut self, items: Vec<MouseInputItem>) {
        if items.is_empty() {
            return;
        }
        if self.mode == Mode::Observe || self.switching_to == Some(Mode::Observe) {
            let mut keyboard = Vec::new();
            let mut wheel = false;
            for item in items {
                match item {
                    MouseInputItem::Bytes(bytes) => keyboard.extend(bytes),
                    MouseInputItem::Mouse(event) => {
                        wheel |= matches!(mouse_kind(&event), MouseKind::Wheel { .. });
                    }
                }
            }
            if !keyboard.is_empty() {
                // Any keystroke takes control and is delivered once the session
                // is up. The wrapper lifecycle has no local quit key.
                self.control_sticky = false;
                self.pending_input.push(keyboard);
                self.switch_mode(Mode::Control);
                return;
            }
            // Wheel escalates only after a soft release; a stray wheel while
            // glancing should not grab the remote lock.
            if wheel {
                if self.control_sticky {
                    self.control_sticky = false;
                    self.switch_mode(Mode::Control);
                } else {
                    self.hint("read-only — type to take control");
                }
            }
            return;
        }

        // control mode
        self.last_input = Instant::now();
        if matches!(items.as_slice(), [MouseInputItem::Bytes(bytes)] if bytes == &[0x1c]) {
            // ctrl+\ — manual release. In always-control there's nothing to
            // release to, so swallow it (never forward it: ctrl+\ is SIGQUIT).
            if !self.args.always_control {
                self.control_sticky = false;
                self.switch_mode(Mode::Observe);
            }
            return;
        }
        if self.switching_to == Some(Mode::Control) || self.session.is_none() {
            // Spinning up or awaiting reconnect: queue keyboard bytes only.
            // Mouse reports must never become literal prompt input.
            let mut keyboard = Vec::new();
            for item in items {
                if let MouseInputItem::Bytes(bytes) = item {
                    keyboard.extend(bytes);
                }
            }
            if !keyboard.is_empty() {
                self.pending_input.push(keyboard);
            }
            if let Some((_, m)) = self.reconnect_at {
                self.reconnect_at = Some((Instant::now(), m));
            }
            return;
        }
        for item in items {
            match item {
                MouseInputItem::Bytes(bytes) => {
                    if !bytes.is_empty() {
                        if self.clear_selection_without_splitting_gesture() {
                            self.paint();
                        }
                        self.send_terminal_input(&bytes, true).await;
                    }
                }
                MouseInputItem::Mouse(event) => self.handle_mouse(event).await,
            }
        }
    }

    async fn send_terminal_input(&mut self, bytes: &[u8], predict: bool) {
        let msg = json!({ "type": "terminal.input", "bytes": B64.encode(bytes) });
        self.send(msg).await;
        if predict && self.predict.on_input(bytes, &self.grid) {
            self.paint();
        }
    }

    fn selection_grid_point(&self, event: &SgrMouseEvent) -> Option<crate::grid::GridPoint> {
        let grid = self.selection_source.as_ref().unwrap_or(&self.grid);
        let (cols, rows) = term_size();
        grid.point_at_viewport_clamped(
            event.column.saturating_sub(1) as usize,
            event.row.saturating_sub(1) as usize,
            cols,
            rows,
        )
    }

    fn mouse_grid_point_in(
        &self,
        grid: &Grid,
        event: &SgrMouseEvent,
    ) -> Option<crate::grid::GridPoint> {
        let (cols, rows) = term_size();
        grid.point_at_viewport(
            event.column.saturating_sub(1) as usize,
            event.row.saturating_sub(1) as usize,
            cols,
            rows,
        )
    }

    fn selection_source_matches(&self) -> bool {
        if !self.selection_dragged {
            return true;
        }
        match (self.selection, self.selection_source.as_ref()) {
            (Some(selection), Some(source)) => self.grid.selection_text_matches(source, &selection),
            _ => true,
        }
    }

    async fn send_terminal_mouse(
        &mut self,
        event: &SgrMouseEvent,
        kind: &'static str,
        button: Option<MouseButton>,
    ) {
        let mut message = json!({
            "type": "terminal.mouse",
            "kind": kind,
            "column": cell_coordinate(event.column),
            "row": cell_coordinate(event.row),
            "modifiers": mouse_modifiers(event.button),
        });
        if let Some(button) = button {
            message["button"] = serde_json::Value::String(button.as_str().to_string());
        }
        self.send(message).await;
    }

    async fn handle_mouse(&mut self, event: SgrMouseEvent) {
        match mouse_kind(&event) {
            MouseKind::Wheel { up } => {
                if self.clear_selection_without_splitting_gesture() {
                    self.paint();
                }
                self.send(json!({
                    "type": "terminal.scroll",
                    "direction": if up { "up" } else { "down" },
                    "lines": 3,
                    "source": "wheel",
                    "column": cell_coordinate(event.column),
                    "row": cell_coordinate(event.row),
                    "modifiers": mouse_modifiers(event.button),
                }))
                .await;
            }
            MouseKind::Down(button) => {
                let owner = self.new_gesture_owner();
                let had_selection = self.clear_selection();
                self.gesture_owner = Some(owner);
                self.gesture_button = Some(button);
                match owner {
                    GestureOwner::Local if button == MouseButton::Left => {
                        self.selection_dragged = false;
                        let source = self.grid.clone();
                        self.selection = self
                            .mouse_grid_point_in(&source, &event)
                            .map(Selection::new);
                        self.selection_source = self.selection.map(|_| source);
                        self.paint();
                    }
                    GestureOwner::Local => {
                        if had_selection {
                            self.paint();
                        }
                    }
                    GestureOwner::Remote => {
                        if had_selection {
                            self.paint();
                        }
                        self.send_terminal_mouse(&event, "down", Some(button)).await;
                    }
                }
            }
            MouseKind::Drag(button) => {
                let owner = match self.gesture_owner {
                    Some(owner) => owner,
                    None => {
                        let owner = self.new_gesture_owner();
                        self.gesture_owner = Some(owner);
                        self.gesture_button = Some(button);
                        owner
                    }
                };
                match owner {
                    GestureOwner::Local if button == MouseButton::Left => {
                        if self.selection_cancelled {
                            return;
                        }
                        if self.selection_source.is_none() {
                            self.selection_source = Some(self.grid.clone());
                        }
                        self.selection_dragged = true;
                        if let Some(point) = self.selection_grid_point(&event) {
                            match self.selection.as_mut() {
                                Some(selection) => selection.set_cursor(point),
                                None => self.selection = Some(Selection::new(point)),
                            }
                            if !self.selection_source_matches() {
                                self.cancel_selection_gesture();
                                self.paint();
                                return;
                            }
                            self.paint();
                        }
                    }
                    GestureOwner::Local => {}
                    GestureOwner::Remote => {
                        self.send_terminal_mouse(&event, "drag", Some(button)).await;
                    }
                }
            }
            MouseKind::Up(button) => {
                if self.gesture_button.is_some_and(|active| active != button) {
                    // One SGR stream can only describe one tracked drag here.
                    // A mismatched release must not end or reroute that gesture.
                    return;
                }
                let owner = match self.gesture_owner.take() {
                    Some(owner) => owner,
                    None => self.new_gesture_owner(),
                };
                self.gesture_button = None;
                if self.selection_cancelled {
                    self.selection_cancelled = false;
                    self.selection = None;
                    self.selection_source = None;
                    self.selection_dragged = false;
                    self.paint();
                    return;
                }
                match owner {
                    GestureOwner::Local if button == MouseButton::Left => {
                        if let Some(point) = self.selection_grid_point(&event) {
                            if let Some(selection) = self.selection.as_mut() {
                                self.selection_dragged |= point != selection.anchor;
                                selection.set_cursor(point);
                            }
                        }
                        let copied = self
                            .selection_source_matches()
                            .then(|| {
                                self.selection.filter(|_| self.selection_dragged).and_then(
                                    |selection| {
                                        self.selection_source
                                            .as_ref()
                                            .unwrap_or(&self.grid)
                                            .selected_text(&selection)
                                    },
                                )
                            })
                            .flatten()
                            .filter(|text| !text.is_empty());
                        if let Some(text) = copied {
                            write_stdout(&osc52_sequence(&text));
                        } else {
                            self.selection = None;
                            self.selection_source = None;
                            self.selection_dragged = false;
                        }
                        self.paint();
                    }
                    GestureOwner::Local => {}
                    GestureOwner::Remote => {
                        self.send_terminal_mouse(&event, "up", Some(button)).await;
                    }
                }
            }
            MouseKind::Moved => {
                let owner = match self.gesture_owner {
                    Some(owner) => owner,
                    None => self.new_gesture_owner(),
                };
                if owner == GestureOwner::Remote {
                    self.send_terminal_mouse(&event, "moved", None).await;
                }
            }
            MouseKind::Other => {}
        }
    }
}

// ---------------------------------------------------------------------------
// main

/// Removes the streamer pidfile on any exit path out of `run` (stale files
/// from a hard kill are harmless — the daemon checks the pid is alive).
struct PidfileGuard(std::path::PathBuf);
impl Drop for PidfileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub async fn run(args: Args) -> Result<()> {
    // announce ourselves so the daemon can tell its typed `exec` took
    // (see util::streamer_pid_path); --dump is a human diagnostic, not a
    // daemon-spawned streamer, so it must not claim the slot
    let _pidfile = (!args.dump).then(|| {
        let state_dir = crate::util::home_dir()
            .join(".local")
            .join("state")
            .join("herdr-mirror");
        let path = crate::util::streamer_pid_path(&state_dir, &args.ssh_target, &args.pane_target);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, std::process::id().to_string());
        PidfileGuard(path)
    });

    let tty = !args.dump && unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1;
    let raw = if tty {
        // 1002/1006: button-event mouse tracking with SGR encoding, so wheel and
        // clicks reach us instead of scrolling the hosting pane's scrollback
        write_stdout("\x1b[?1049h\x1b[2J\x1b[H\x1b[?1003l\x1b[?1002h\x1b[?1006h");
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
        predict: Predictor::new(),
        terminal_state: TerminalSessionState::default(),
        unknown_state_warned: false,
        pixel_mouse_warned: false,
        mouse_grab: if tty {
            MouseGrab::Button // startup wrote ?1002h when we're a tty
        } else {
            MouseGrab::Off
        },
        mouse_input: SgrMouseInputParser::new(),
        mouse_input_flush_at: None,
        selection: None,
        selection_source: None,
        selection_dragged: false,
        selection_cancelled: false,
        gesture_owner: None,
        gesture_button: None,
        // startup leaves the pane in normal cursor mode; terminal.state moves
        // it only when the remote reports application cursor mode explicitly
        app_cursor_keys: false,
    };
    // Control is authoritative on the remote: the server resizes the remote pty
    // to whatever we ask for, beating even a larger live client over there. So
    // entering Control with a size we cannot vouch for is what let a local herdr
    // with no client attached drag a healthy remote pane down to its 80x24
    // placeholder (#23). Observe never resizes anything, so it is the safe place
    // to wait: the first resize or keystroke proves a human and promotes us.
    // BEFORE connect: spawning the session awaits a process launch, and a
    // SIGWINCH arriving in that window is lost outright (its default disposition
    // is to be ignored). That window is exactly when a client attaching lays out
    // a freshly created pane — the resize we now promote on. Registered first,
    // tokio buffers it and delivers it once the loop starts.
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sighup = signal(SignalKind::hangup())?; // pane closed — don't orphan the ssh child
    let mut sigwinch = signal(SignalKind::window_change())?;

    app.connect(initial_mode(app.args.always_control, term_size()))
        .await;
    // the pane may have been laid out while the session was spawning; the signal
    // for that is buffered above, but check directly too
    if app.mode == Mode::Observe
        && initial_mode(app.args.always_control, term_size()) == Mode::Control
    {
        app.switch_mode(Mode::Control);
    } else if app.args.always_control && app.mode == Mode::Observe {
        // F3: otherwise the pane is inert with no explanation
        app.hint("read-only until this pane is sized — type to take control");
    }

    loop {
        // Earliest pending deadline: input framing, mode switch, reconnect,
        // hint clear, prediction, or idle release.
        let idle_at = (app.mode == Mode::Control
            && app.switching_to.is_none()
            && app.session.is_some()
            && !app.args.always_control
            && app.args.control_idle_secs > 0)
            .then(|| app.last_input + Duration::from_secs(app.args.control_idle_secs));
        let sleep = crate::util::sleep_until_earliest([
            app.switch_at,
            app.reconnect_at.map(|(t, _)| t),
            app.hint_clear_at,
            idle_at,
            app.predict.deadline(),
            app.mouse_input_flush_at,
        ]);

        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    None => break,
                    Some(Msg::Frame { gen, frame }) => app.handle_frame(gen, frame),
                    Some(Msg::State { gen, state }) => app.handle_state(gen, state),
                    Some(Msg::Closed { gen, reason }) => app.handle_closed(gen, reason),
                    Some(Msg::SessionExit { gen, mode, reason, uptime }) => app.handle_exit(gen, mode, reason, uptime),
                    Some(Msg::Stdin(buf)) => app.handle_stdin(buf).await,
                }
            }
            _ = sigwinch.recv() => {
                app.renderer.invalidate();
                app.invalidate_selection_during_gesture();
                // a resize means a client is laying this pane out, so the size is
                // now a real viewport: take control if that is what we're for.
                // control_sticky means control was refused twice in a row and we
                // told the user "type to retry" — a window drag must not turn
                // that into a reconnect storm.
                if app.args.always_control && app.mode == Mode::Observe && !app.control_sticky {
                    app.switch_mode(Mode::Control);
                }
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
                if app.mouse_input_flush_at.is_some_and(|t| t <= now) {
                    app.flush_pending_mouse_input().await;
                }
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
                if app.predict.deadline().is_some_and(|t| t <= now) {
                    app.predict.on_tick(); // wipe timed-out ghosts (no-echo prompts)
                    app.paint();
                }
            }
        }
    }

    // clean shutdown: release control if held, kill the ssh child, restore tty
    if let Some(mut s) = app.session.take() {
        if s.mode == Mode::Control {
            let _ = s
                .stdin
                .write_all(b"{\"type\":\"terminal.release\"}\n")
                .await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        unsafe { libc::kill(s.pid, libc::SIGTERM) };
    }
    if tty {
        // ?1l with the rest: leaving the hosting pane in application cursor mode
        // would misencode arrows for whatever runs there next
        write_stdout("\x1b[?1003l\x1b[?1002l\x1b[?1006l\x1b[?1l\x1b[?25h\x1b[?1049l");
    }
    if let Some(raw) = raw {
        raw.restore();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mouse(sequence: &[u8]) -> SgrMouseEvent {
        let mut parser = SgrMouseInputParser::new();
        parser
            .push(sequence)
            .into_iter()
            .find_map(|item| match item {
                MouseInputItem::Mouse(event) => Some(event),
                MouseInputItem::Bytes(_) => None,
            })
            .expect("mouse event")
    }

    #[test]
    fn wheel_is_classified_independently_of_terminal_state() {
        assert_eq!(
            mouse_kind(&mouse(b"\x1b[<64;10;5M")),
            MouseKind::Wheel { up: true }
        );
        assert_eq!(
            mouse_kind(&mouse(b"\x1b[<65;10;5M")),
            MouseKind::Wheel { up: false }
        );
        // Modifier bits do not turn a wheel into literal terminal input.
        assert_eq!(
            mouse_kind(&mouse(b"\x1b[<68;10;5M")),
            MouseKind::Wheel { up: true }
        );
        assert_eq!(
            mouse_kind(&mouse(b"\x1b[<81;10;5M")),
            MouseKind::Wheel { up: false }
        );
        assert_eq!(mouse_kind(&mouse(b"\x1b[<64;10;5m")), MouseKind::Other);
        assert_eq!(mouse_modifiers(64), 0);
        assert_eq!(mouse_modifiers(68), 1); // Shift
        assert_eq!(mouse_modifiers(72), 4); // Alt
        assert_eq!(mouse_modifiers(80), 2); // Control
        assert_eq!(mouse_modifiers(92), 7); // Shift + Alt + Control
        assert_eq!(cell_coordinate(0), 0);
        assert_eq!(cell_coordinate(1), 0);
        assert_eq!(cell_coordinate(10), 9);
        assert_eq!(cell_coordinate(u32::MAX), u16::MAX);
    }

    #[test]
    fn mouse_grab_mode_matches_session_and_terminal_state() {
        let local = TerminalSessionState {
            mouse_reporting: Some(false),
            ..TerminalSessionState::default()
        };
        let remote_button = TerminalSessionState {
            mouse_reporting: Some(true),
            mouse_pixel_reporting: Some(false),
            mouse_any_motion: Some(false),
            ..TerminalSessionState::default()
        };
        let remote_any = TerminalSessionState {
            mouse_reporting: Some(true),
            mouse_pixel_reporting: Some(false),
            mouse_any_motion: Some(true),
            ..TerminalSessionState::default()
        };

        assert_eq!(
            desired_mouse_grab(Mode::Control, &TerminalSessionState::default()),
            MouseGrab::Button
        );
        assert_eq!(desired_mouse_grab(Mode::Control, &local), MouseGrab::Button);
        assert_eq!(
            desired_mouse_grab(Mode::Control, &remote_button),
            MouseGrab::Button
        );
        assert_eq!(
            desired_mouse_grab(Mode::Control, &remote_any),
            MouseGrab::Any
        );
        assert_eq!(
            desired_mouse_grab(Mode::Observe, &TerminalSessionState::default()),
            MouseGrab::Off
        );
        assert_eq!(desired_mouse_grab(Mode::Observe, &local), MouseGrab::Off);
        assert_eq!(
            desired_mouse_grab(Mode::Observe, &remote_button),
            MouseGrab::Button
        );
        assert_eq!(
            desired_mouse_grab(Mode::Observe, &remote_any),
            MouseGrab::Button
        );
    }

    #[test]
    fn mouse_grab_transitions_toggle_button_and_any_motion_modes() {
        assert_eq!(mouse_grab_transition(MouseGrab::Off, MouseGrab::Off), "");
        assert_eq!(
            mouse_grab_transition(MouseGrab::Button, MouseGrab::Button),
            ""
        );
        assert_eq!(mouse_grab_transition(MouseGrab::Any, MouseGrab::Any), "");
        assert_eq!(
            mouse_grab_transition(MouseGrab::Off, MouseGrab::Button),
            "\x1b[?1002h\x1b[?1006h"
        );
        assert_eq!(
            mouse_grab_transition(MouseGrab::Off, MouseGrab::Any),
            "\x1b[?1003h\x1b[?1006h"
        );
        assert_eq!(
            mouse_grab_transition(MouseGrab::Button, MouseGrab::Any),
            "\x1b[?1002l\x1b[?1003h"
        );
        assert_eq!(
            mouse_grab_transition(MouseGrab::Any, MouseGrab::Button),
            "\x1b[?1003l\x1b[?1002h"
        );
        assert_eq!(
            mouse_grab_transition(MouseGrab::Button, MouseGrab::Off),
            "\x1b[?1002l\x1b[?1006l"
        );
        assert_eq!(
            mouse_grab_transition(MouseGrab::Any, MouseGrab::Off),
            "\x1b[?1003l\x1b[?1006l"
        );
    }

    #[test]
    fn mouse_routing_uses_only_authoritative_terminal_state() {
        assert_eq!(
            mouse_routing(&TerminalSessionState::default()),
            MouseRouting::Unknown
        );
        let local = TerminalSessionState {
            mouse_reporting: Some(false),
            ..TerminalSessionState::default()
        };
        assert_eq!(mouse_routing(&local), MouseRouting::Local);
        let remote = TerminalSessionState {
            mouse_reporting: Some(true),
            mouse_pixel_reporting: Some(false),
            mouse_any_motion: Some(false),
            ..TerminalSessionState::default()
        };
        assert_eq!(mouse_routing(&remote), MouseRouting::Remote);
        let missing_pixel = TerminalSessionState {
            mouse_reporting: Some(true),
            mouse_any_motion: Some(false),
            ..TerminalSessionState::default()
        };
        assert_eq!(mouse_routing(&missing_pixel), MouseRouting::Unknown);
        let missing_any_motion = TerminalSessionState {
            mouse_reporting: Some(true),
            mouse_pixel_reporting: Some(false),
            ..TerminalSessionState::default()
        };
        assert_eq!(mouse_routing(&missing_any_motion), MouseRouting::Unknown);
        let pixel = TerminalSessionState {
            mouse_reporting: Some(true),
            mouse_pixel_reporting: Some(true),
            ..TerminalSessionState::default()
        };
        assert_eq!(mouse_routing(&pixel), MouseRouting::PixelUnsupported);
    }

    #[test]
    fn terminal_state_parsing_is_tolerant_and_fail_local() {
        let full = serde_json::json!({
            "type": "terminal.state",
            "mouse_reporting": true,
            "mouse_pixel_reporting": false,
            "mouse_any_motion": true,
            "alternate_screen": true,
            "application_cursor": true,
            "scroll": { "offset_from_bottom": 4 },
        });
        let state = TerminalSessionState::from_value(&full);
        assert_eq!(state.mouse_reporting, Some(true));
        assert_eq!(state.mouse_pixel_reporting, Some(false));
        assert_eq!(state.mouse_any_motion, Some(true));
        assert_eq!(state.alternate_screen, Some(true));
        assert_eq!(state.application_cursor, Some(true));
        assert!(state.scroll.is_some());

        // Null, missing, and a wrong type all overwrite the corresponding
        // fields with unknown rather than retaining a stale remote route.
        let malformed = TerminalSessionState::from_value(&serde_json::json!({
            "type": "terminal.state",
            "mouse_reporting": true,
            "mouse_pixel_reporting": "no",
            "mouse_any_motion": null,
            "application_cursor": 1,
        }));
        assert_eq!(mouse_routing(&malformed), MouseRouting::Unknown);
    }

    #[test]
    fn reconnect_without_cursor_metadata_preserves_decckm_until_explicit_false() {
        assert_eq!(
            cursor_key_transition(false, Some(true)),
            Some((true, "\x1b[?1h"))
        );

        // connect() clears terminal_state for each generation. An older peer or
        // a delayed state message must not silently turn off the DECCKM carried
        // over from the previous generation.
        let current = true;
        assert_eq!(
            cursor_key_transition(current, TerminalSessionState::default().application_cursor),
            None
        );
        assert_eq!(
            cursor_key_transition(current, Some(false)),
            Some((false, "\x1b[?1l"))
        );
        assert_eq!(cursor_key_transition(false, Some(false)), None);
    }

    #[test]
    fn classifies_structured_mouse_phases_and_buttons() {
        assert_eq!(
            mouse_kind(&mouse(b"\x1b[<0;3;4M")),
            MouseKind::Down(MouseButton::Left)
        );
        assert_eq!(
            mouse_kind(&mouse(b"\x1b[<33;4;4M")),
            MouseKind::Drag(MouseButton::Middle)
        );
        assert_eq!(
            mouse_kind(&mouse(b"\x1b[<2;4;4m")),
            MouseKind::Up(MouseButton::Right)
        );
        assert_eq!(mouse_kind(&mouse(b"\x1b[<35;4;4M")), MouseKind::Moved);
    }

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("w9:p1"), "'w9:p1'");
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn clipboard_uses_bel_terminated_osc52() {
        assert_eq!(osc52_sequence("hello"), "\x1b]52;c;aGVsbG8=\x07");
    }

    #[test]
    fn selection_highlight_starts_only_after_a_drag() {
        let anchor = crate::grid::GridPoint::new(2, 3);
        let anchored = Selection::new(anchor);
        assert_eq!(visible_selection(Some(&anchored), false), None);

        let dragged = Selection::range(anchor, crate::grid::GridPoint::new(2, 4));
        assert_eq!(visible_selection(Some(&dragged), true), Some(&dragged));

        // A real drag across the two terminal columns occupied by one wide
        // glyph normalizes both endpoints to the same grid point. The explicit
        // gesture flag must still make that one-character selection visible.
        assert_eq!(visible_selection(Some(&anchored), true), Some(&anchored));
    }

    #[test]
    fn arg_parsing() {
        let argv: Vec<String> = [
            "work",
            "w9:p1",
            "--remote-bin",
            "/opt/herdr",
            "--cols",
            "176",
            "--rows",
            "66",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let a = parse_args(&argv).unwrap();
        assert_eq!(a.ssh_target, "work");
        assert_eq!(a.pane_target, "w9:p1");
        assert_eq!(a.remote_bin.as_deref(), Some("/opt/herdr"));
        assert_eq!((a.cols, a.rows), (176, 66));
        assert!(a.size_fixed);
        let legacy: Vec<String> = ["work", "w9:p1", "--ctl-path", "/state/work.ctl"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert!(
            parse_args(&legacy).is_ok(),
            "restored pre-upgrade argv must parse"
        );
        assert!(parse_args(&["onlyone".to_string()]).is_err());
        assert!(parse_args(&[
            "a".into(),
            "b".into(),
            "--visibility-file".into(),
            "x".into()
        ])
        .is_err());
    }

    // --- birth size trust (#23) ---
    //
    // herdr renders at 80x24 when no client is attached, and chrome only
    // subtracts, so anything larger in either axis is provably a real viewport.

    #[test]
    fn a_placeholder_sized_pane_is_never_trusted() {
        // what a mirror pane is born as when nobody is watching: 80x24 less a
        // 26-col sidebar and the tab bar
        assert!(!size_is_trusted((54, 23)));
        // and the extremes of that layout, in case chrome is configured away
        assert!(!size_is_trusted((80, 24)));
        assert!(!size_is_trusted((80, 23)));
    }

    #[test]
    fn an_ordinary_viewport_is_trusted_immediately() {
        assert!(size_is_trusted((141, 44)));
        assert!(size_is_trusted((133, 47)));
        // one axis is enough: a tall narrow pane cannot come from a 24-row floor
        assert!(size_is_trusted((60, 40)));
        assert!(size_is_trusted((200, 20)));
    }

    #[test]
    fn initial_mode_is_read_only_unless_the_size_vouches_for_itself() {
        // the whole composition: only a trusted size under always_control opens
        // writable, because Control is what can resize the remote
        assert_eq!(initial_mode(true, (141, 44)), Mode::Control);
        assert_eq!(
            initial_mode(true, (54, 23)),
            Mode::Observe,
            "placeholder-sized"
        );
        // without always_control we start read-only regardless, as before
        assert_eq!(initial_mode(false, (141, 44)), Mode::Observe);
        assert_eq!(initial_mode(false, (54, 23)), Mode::Observe);
    }

    #[test]
    fn a_small_client_is_not_trusted_at_birth_and_must_earn_control() {
        // A phone (45x18 -> pane 44x16) and moshi (50x25 -> 49x23) are real
        // viewports, but at birth they are indistinguishable from the placeholder
        // — that is the whole reason shape matching failed. They start read-only
        // and the first resize or keystroke promotes them, rather than being
        // allowed to impose a size we cannot vouch for.
        assert!(!size_is_trusted((44, 16)));
        assert!(!size_is_trusted((49, 23)));
    }
}
