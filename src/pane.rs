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
//   --max-cols N        cap the size control asks the remote for (default:
//   --max-rows N        uncapped — control fills the local pane). Set for a
//                       remote with its own display: the remote keeps its own
//                       geometry and the rest of the local pane stays blank.
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
use crate::predict::Predictor;

// ---------------------------------------------------------------------------
// args

#[derive(Debug, Clone)]
pub struct Args {
    pub ssh_target: String,
    pub pane_target: String,
    /// Configured remote herdr path. `None` = auto-resolve on the remote
    /// (PATH, then `~/.local/bin/herdr`). See `config::remote_herdr_expr`.
    pub remote_bin: Option<String>,
    pub cols: usize,
    pub rows: usize,
    pub dump: bool,
    pub session: Option<String>,
    /// auto-release control after this much input idle; 0 disables
    pub control_idle_secs: u64,
    /// start and stay in control: writable, no idle release, and sized to the
    /// local pane so it fills. Set by the daemon from per-host config.
    pub always_control: bool,
    /// Legacy Herdr only: permit one bounded `--takeover` retry after the exact
    /// existing-controller rejection. Defaults off.
    pub takeover_on_reconnect: bool,
    /// Stable configured host key used to derive controller ownership and
    /// duplicate-streamer identity. Direct invocations default to ssh_target.
    pub controller_scope: Option<String>,
    /// Daemon-computed cross-process key. Recomputed and verified by pane mode.
    pub streamer_key: Option<String>,
    pub terminal_reconnect: bool,
    controller_id: Option<String>,
    /// upper bound on the size control asks the remote for. `None` = uncapped
    /// (fill the local pane). Set by the daemon from per-host config; observe
    /// is never capped, since it doesn't resize anything.
    pub max_cols: Option<usize>,
    pub max_rows: Option<usize>,
    /// daemon's ssh ControlMaster socket for this host; foreground polls reuse it
    /// (`ssh -S <path>`) to skip a handshake. None → polls connect directly.
    ///
    pub ctl_path: Option<String>,
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
        always_control: false,
        takeover_on_reconnect: false,
        controller_scope: None,
        streamer_key: None,
        terminal_reconnect: false,
        controller_id: None,
        max_cols: None,
        max_rows: None,
        ctl_path: None,
        container: None,
    };
    let mut container_name: Option<String> = None;
    let mut container_folder: Option<String> = None;
    let mut docker_bin = "docker".to_string();
    let mut positional: Vec<String> = Vec::new();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        let mut next = |flag: &str| -> Result<String> {
            it.next().cloned().ok_or_else(|| err(format!("{flag} needs a value")))
        };
        match a.as_str() {
            "--remote-bin" => args.remote_bin = Some(next("--remote-bin")?),
            "--cols" => {
                args.cols = next("--cols")?.parse().map_err(|_| err("--cols must be a number"))?;
            }
            "--rows" => {
                args.rows = next("--rows")?.parse().map_err(|_| err("--rows must be a number"))?;
            }
            "--session" => args.session = Some(next("--session")?),
            "--control-idle" => {
                args.control_idle_secs =
                    next("--control-idle")?.parse().map_err(|_| err("--control-idle must be a number"))?
            }
            "--always-control" => args.always_control = true,
            "--takeover-on-reconnect" => args.takeover_on_reconnect = true,
            "--controller-scope" => args.controller_scope = Some(next("--controller-scope")?),
            "--streamer-key" => args.streamer_key = Some(next("--streamer-key")?),
            "--terminal-reconnect" => args.terminal_reconnect = true,
            // 0 is unset here for the same reason config treats it that way:
            // a zero cap would ask the remote for a zero-column terminal, which
            // herdr rejects outright, killing the session twice over and
            // stranding the pane in "control unavailable" over a typo.
            "--max-cols" => {
                args.max_cols = Some(next("--max-cols")?.parse().map_err(|_| err("--max-cols must be a number"))?)
                    .filter(|&n| n > 0)
            }
            "--max-rows" => {
                args.max_rows = Some(next("--max-rows")?.parse().map_err(|_| err("--max-rows must be a number"))?)
                    .filter(|&n| n > 0)
            }
            "--ctl-path" => args.ctl_path = Some(next("--ctl-path")?),
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
            "usage: herdr-mirror pane <ssh-target> <pane-target> [--remote-bin PATH] [--session NAME] [--cols N --rows N] [--max-cols N --max-rows N] [--dump]",
        ));
    }
    args.container = match (container_name, container_folder) {
        (Some(_), Some(_)) => return Err(err("--container and --container-folder are exclusive")),
        (Some(n), None) => {
            Some(ContainerArg { kind: crate::config::HostKind::DockerContainer(n), docker_bin })
        }
        (None, Some(f)) => {
            Some(ContainerArg { kind: crate::config::HostKind::DockerFolder(f), docker_bin })
        }
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
    #[serde(rename = "type")]
    kind: String,
    seq: Option<u64>,
    full: Option<bool>,
    width: Option<usize>,
    height: Option<usize>,
    bytes: Option<String>,
    reason: Option<String>,
    nonce: Option<u64>,
    code: Option<String>,
    index: Option<usize>,
    total_bytes: Option<usize>,
    claim_token: Option<u64>,
}

enum Msg {
    Frame { gen: u64, frame: Frame },
    SessionExit { gen: u64, mode: Mode, reason: String, uptime: Duration },
    Stdin(Vec<u8>),
    /// result of a background foreground-process poll: Some(true)=shell,
    /// Some(false)=TUI, None=poll failed (keep last value)
    Foreground(Option<bool>),
    Paste(crate::paste::Outcome),
    Drop(crate::paste::DropResult),
}

struct Session {
    gen: u64,
    mode: Mode,
    stdin: ChildStdin,
    supervisor: crate::child_supervisor::ChildSupervisor,
}

#[derive(Clone, Copy)]
struct ReconnectClaim {
    token: Option<u64>,
    generation: u64,
}

impl Session {
    async fn stop(mut self, release: bool) {
        if release && self.mode == Mode::Control {
            let _ = tokio::time::timeout(
                SESSION_WRITE_TIMEOUT,
                self.stdin
                    .write_all(b"{\"type\":\"terminal.release\"}\n"),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.supervisor.cancel_and_wait().await;
    }
}

/// POSIX single-quote: an embedded ' can't break the remote shell parse.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn spawn_session(
    args: &Args,
    mode: Mode,
    takeover: bool,
    reconnect_claim: Option<ReconnectClaim>,
    size: (usize, usize),
    gen: u64,
    tx: mpsc::Sender<Msg>,
) -> Result<Session> {
    let (cols, rows) = size;
    // Configured paths stay unquoted so remote-shell ~ expands; auto mode is an
    // `sh -c` resolver that takes the trailing words as "$@" (see
    // config::remote_herdr_expr).
    let bin = crate::config::remote_herdr_expr(args.remote_bin.as_deref(), args.session.as_deref());
    let mut cmd = format!(
        "exec {} terminal session {} {} --cols {} --rows {}",
        bin,
        mode.as_str(),
        sh_quote(&args.pane_target),
        cols,
        rows
    );
    if mode == Mode::Control && takeover {
        cmd.push_str(" --takeover");
    }
    if args.terminal_reconnect {
        cmd.push_str(" --lease-ms 20000");
        if mode == Mode::Control {
            if let Some(controller_id) = &args.controller_id {
                cmd.push_str(" --controller-id ");
                cmd.push_str(&sh_quote(controller_id));
                if let Some(claim) = reconnect_claim {
                    if let Some(claim_token) = claim.token {
                        cmd.push_str(" --replace-claim-token ");
                        cmd.push_str(&claim_token.to_string());
                    }
                    cmd.push_str(" --controller-generation ");
                    cmd.push_str(&claim.generation.to_string());
                }
            }
        }
    }
    // ssh and docker differ only in how the command is carried; the streaming
    // contract (piped stdio, herdr's frames on stdout) is identical
    let mut builder = match &args.container {
        None => {
            let mut c = tokio::process::Command::new("ssh");
            c.args(crate::remote::SSH_COMMON_OPTS).arg(&args.ssh_target).arg(cmd);
            c
        }
        Some(ct) => {
            // resolve per spawn so a rebuilt container is picked up on
            // reconnect. Bounded: this runs on the pane's single-threaded
            // runtime, so a wedged Docker daemon must not be able to freeze
            // input, rendering or signal handling.
            let id = crate::docker::resolve_blocking(
                &ct.docker_bin,
                &ct.kind,
                Duration::from_secs(5),
            )?;
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
    let stdin = child.stdin.take().ok_or_else(|| err("no child stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| err("no child stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| err("no child stderr"))?;
    let (supervisor, mut exited) = crate::child_supervisor::ChildSupervisor::start(child);
    let started = Instant::now();
    let structured = args.terminal_reconnect;

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
                close_reason = match (&frame.code, &frame.reason) {
                    (Some(code), Some(reason)) => format!("{code}: {reason}"),
                    (Some(code), None) => code.clone(),
                    (None, Some(reason)) => reason.clone(),
                    (None, None) => String::new(),
                };
            }
            if tx.send(Msg::Frame { gen, frame }).await.is_err() {
                break;
            }
        }
        let _ = crate::child_supervisor::wait_for_exit(&mut exited).await;
        stderr_task.abort();
        let tail = err_tail.lock().unwrap().trim().to_string();
        let reason = if close_reason.is_empty() {
            if tail.is_empty() && structured {
                "transport_closed".to_owned()
            } else {
                tail
            }
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
        stdin,
        supervisor,
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

/// Clamp a local terminal size to the per-host control caps. Split out from
/// `control_size` so the arithmetic is testable without an `App`.
fn cap_size(
    (cols, rows): (usize, usize),
    max_cols: Option<usize>,
    max_rows: Option<usize>,
) -> (usize, usize) {
    (
        max_cols.map_or(cols, |cap| cols.min(cap)),
        max_rows.map_or(rows, |cap| rows.min(cap)),
    )
}

/// Size to request for an observe stream. Split out from `App::observe_size` so
/// the floor is testable.
///
/// `--cols/--rows` are a floor, never an exact request. As a floor they still do
/// their original job: the request must be >= the remote PTY size or the server
/// clips its bottom rows away, and the daemon's numbers already carry a margin.
/// As an exact request they are wrong — the daemon samples the *remote* pane's
/// rect when it spawns the streamer, and a headless remote reports the no-client
/// placeholder, so the numbers are small. Control then resizes the remote pty to
/// this pane and nothing shrinks it back on release, so asking for the daemon's
/// numbers again would stream a crop of a screen that has since grown, painted
/// into the corner of a much larger pane.
fn observe_size_for(args: &Args, term: (usize, usize)) -> (usize, usize) {
    (args.cols.max(term.0), args.rows.max(term.1))
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

/// How a parsed mouse event should be routed while in control mode.
#[derive(Debug, PartialEq, Eq)]
enum MouseAction {
    /// wheel: send as a semantic terminal.scroll (server decides app vs scrollback)
    Scroll { up: bool },
    /// click/drag on a remote TUI: forward the raw SGR sequence
    ForwardRaw,
    /// click/drag at a shell (or unclassified): drop, keep mouse local
    Drop,
}

/// Wheel always scrolls semantically, regardless of the foreground
/// classification — the remote herdr server knows the real app's mouse mode
/// and is a better judge than this side's process-name heuristic (e.g. a TUI
/// that doesn't consume wheel events, like an agent CLI). Non-wheel
/// clicks/drags keep the existing foreground-based routing.
fn mouse_action(remote_is_shell: Option<bool>, btn: u32, press: bool) -> MouseAction {
    if press && (btn == 64 || btn == 65) {
        MouseAction::Scroll { up: btn == 64 }
    } else if btn == 66 || btn == 67 {
        MouseAction::Drop
    } else if remote_is_shell == Some(false) {
        MouseAction::ForwardRaw
    } else {
        MouseAction::Drop
    }
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

/// Cap on a partially-received bracketed paste. Past this we stop waiting for
/// a terminator and flush what we have: a huge paste is still forwarded, it
/// just loses the drop treatment rather than buffering without bound.
const MAX_PASTE_BYTES: usize = 1024 * 1024;

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Input held back while an upload is in flight (see `route_input`).
enum Queued {
    Raw(Vec<u8>),
    Body(Vec<u8>),
}

#[derive(Debug, PartialEq)]
pub(crate) enum PasteSplit {
    /// a paste has begun but not finished; nothing to do yet
    Pending,
    /// no paste involved: forward as-is
    Passthrough(Vec<u8>),
    Complete { before: Vec<u8>, body: Vec<u8>, after: Vec<u8> },
}

fn find_seq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Longest suffix of `hay` that is a *proper* prefix of `needle`, so a marker
/// straddling two reads is held rather than forwarded in pieces.
fn trailing_partial(hay: &[u8], needle: &[u8]) -> usize {
    (1..needle.len().min(hay.len()) + 1)
        .rev()
        .find(|&n| n < needle.len() && hay[hay.len() - n..] == needle[..n])
        .unwrap_or(0)
}

/// Pull the next complete bracketed paste out of `buf` + `chunk`.
///
/// Pure so the framing can be table-tested without a pane: every interesting
/// case (split reads, a START with no END, bytes either side, several pastes
/// in one read, the oversize flush) lives here rather than in the event loop.
pub(crate) fn split_paste(buf: &mut Vec<u8>, chunk: Vec<u8>) -> PasteSplit {
    // fast path for ordinary typing: nothing buffered and no marker starting
    if buf.is_empty()
        && find_seq(&chunk, PASTE_START).is_none()
        && trailing_partial(&chunk, PASTE_START) == 0
    {
        return PasteSplit::Passthrough(chunk);
    }
    buf.extend_from_slice(&chunk);

    // a paste that never terminates must not buffer without bound
    if buf.len() > MAX_PASTE_BYTES {
        return PasteSplit::Passthrough(std::mem::take(buf));
    }

    let Some(start) = find_seq(buf, PASTE_START) else {
        // hold only a possible partial marker; release everything before it
        let keep = trailing_partial(buf, PASTE_START);
        let cut = buf.len() - keep;
        if cut == 0 {
            return PasteSplit::Pending;
        }
        let tail = buf.split_off(cut);
        let head = std::mem::replace(buf, tail);
        return PasteSplit::Passthrough(head);
    };
    let Some(end) = find_seq(&buf[start..], PASTE_END).map(|i| start + i) else {
        return PasteSplit::Pending;
    };

    let all = std::mem::take(buf);
    PasteSplit::Complete {
        before: all[..start].to_vec(),
        body: all[start + PASTE_START.len()..end].to_vec(),
        after: all[end + PASTE_END.len()..].to_vec(),
    }
}

fn has_mouse_seq(bytes: &[u8]) -> bool {
    bytes.windows(3).any(|w| w == [0x1b, b'[', b'<'])
}


// ---------------------------------------------------------------------------
// the wrapper state machine

const BACKOFF: [u64; 4] = [1000, 2000, 5000, 10000];

/// Rung used once the remote pane is known to be gone. Deliberately still a
/// retry and not a stop: the daemon owns mirror lifecycle and reaps a pane
/// whose remote has been absent for two converge polls, so the streamer's job
/// here is to wait quietly for that rather than to decide it is finished. It
/// also keeps "gone" recoverable, which it sometimes is — a remote herdr
/// restarting renumbers pane ids while session restore runs.
const GONE_BACKOFF_MS: u64 = 60_000;

const SWITCH_GAP: Duration = Duration::from_millis(200);
const QUICK_CONTROL_FAILURE: Duration = Duration::from_secs(4);
const WAKE_RETRY_WINDOW: Duration = Duration::from_secs(30);
const WAKE_RETRY_DELAY: Duration = Duration::from_secs(1);
const SESSION_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// Is this failure "the pane we stream is gone", as opposed to any other
/// failure that happens to mention something missing?
///
/// Matches herdr's whole sentence (`headless.rs`: "terminal target {t} not
/// found") with OUR target in it, rather than loose substrings. That keeps it
/// off the remote-bin resolver's `exec: …/herdr: not found`, which means herdr
/// is absent on that host — a different problem with a different fix — and
/// stops a target being confused with one that merely shares its prefix
/// (`w1:p1` vs `w1:p10`, both real ids in herdr's base-32 alphabet).
///
/// Note the pane dying *underneath* a live stream reports differently
/// ("terminal attach ended: terminal {term_id} not found", carrying herdr's
/// internal terminal id, not ours). That deliberately does not match: the next
/// attempt asks for the pane target and gets the canonical sentence a second
/// later, so this fires one cycle behind rather than being loosened.
fn target_gone(reason: &str, pane_target: &str) -> bool {
    if pane_target.is_empty() {
        return false;
    }
    reason
        .to_ascii_lowercase()
        .contains(&format!("terminal target {} not found", pane_target.to_ascii_lowercase()))
}

fn reconnect_flags_unsupported(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    ["--controller-id", "--lease-ms", "--replace-claim-token"]
        .iter()
        .any(|flag| reason.contains(flag))
        && ["unknown", "unexpected", "unrecognized"]
            .iter()
            .any(|word| reason.contains(word))
}

/// Delay before the next attempt, and the ladder position to keep.
///
/// Pure so the rung and the ladder-resume are testable without a live pane.
/// A gone target does NOT consume a rung: if the pane comes back and later
/// fails transiently, that failure should start where the fast ladder left
/// off rather than at the top.
fn reconnect_delay(gone: bool, idx: usize) -> (u64, usize) {
    if gone {
        return (GONE_BACKOFF_MS, idx);
    }
    (BACKOFF[idx.min(BACKOFF.len() - 1)], idx + 1)
}

fn control_input_needs_buffer(
    switching_to: Option<Mode>,
    session_present: bool,
    session_ready: bool,
) -> bool {
    switching_to == Some(Mode::Control) || !session_present || !session_ready
}

fn bound_reconnect_delay(delay_ms: u64, wake_retry_until: Option<Instant>, now: Instant) -> u64 {
    if wake_retry_until.is_some_and(|deadline| now < deadline) {
        delay_ms.min(WAKE_RETRY_DELAY.as_millis() as u64)
    } else {
        delay_ms
    }
}

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
    wake_retry_until: Option<Instant>,
    /// consecutive quick control failures → fall back to observe
    control_failures: u32,
    control_sticky: bool,
    takeover_policy: crate::ownership::LegacyTakeoverPolicy,
    next_control_takeover: bool,
    session_ready: bool,
    claim_token: Option<u64>,
    claim_token_key: String,
    state_dir: std::path::PathBuf,
    heartbeat_status_visible: bool,
    ownership_hint: Option<String>,
    frame_assembler: crate::frame_stream::FrameAssembler,
    frame_resync_pending: bool,
    last_frame_seq: Option<u64>,
    liveness: crate::liveness::Liveness,
    pending_input: crate::pending_input::PendingInput,
    last_input: Instant,
    hint_clear_at: Option<Instant>,
    /// predictive local echo — draws keystrokes optimistically, frame-verified
    predict: Predictor,
    /// remote pane foreground: Some(true)=shell (keep mouse local, no garbage),
    /// Some(false)=TUI (forward clicks), None=unknown (fail safe to local).
    /// Refreshed lazily on mouse activity via `herdr pane process-info`.
    remote_is_shell: Option<bool>,
    /// last time a foreground poll was kicked off (throttles the ssh handshakes)
    fg_poll_at: Option<Instant>,
    /// scheduled delayed re-poll to catch a foreground change the last input just
    /// caused (e.g. quitting a TUI back to a shell); bypasses the throttle
    settle_at: Option<Instant>,
    /// whether the local mouse grab (?1002h) is currently on. Released at a shell
    /// so herdr does native selection/scroll; re-grabbed for a TUI so clicks can
    /// be forwarded.
    mouse_grabbed: bool,
    /// whether the local pane is currently in application cursor mode (?1h), held
    /// to match the remote's so forwarded arrows arrive in the form it expects
    app_cursor_keys: bool,
    paste_inflight: bool,
    /// partially-received bracketed paste (see `intercept_paste`)
    paste_buf: Vec<u8>,
    /// input held back while an upload is in flight, flushed in order after
    paste_queue: Vec<Queued>,
    /// the payload that started the in-flight upload, so it can be forwarded
    /// unchanged when every path turns out to exist on the remote already
    paste_original: Option<Vec<u8>>,
}

/// minimum spacing between foreground polls — each is an ssh handshake, so we
/// poll lazily (only around mouse activity) and no faster than this
const FG_POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// after input settles, re-poll once this much later to catch a foreground
/// change the input caused (e.g. a TUI just exited); bypasses FG_POLL_INTERVAL
const SETTLE_DELAY: Duration = Duration::from_millis(350);

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
        let mut out = self.renderer.paint(&self.grid, cols, rows);
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

    /// A hint with no expiry, for work whose duration we don't know: an upload
    /// can outlast the usual 1.5s and the pane would otherwise look idle while
    /// it is busy. Whoever set it replaces it when the work resolves.
    fn hint_sticky(&mut self, text: &str) {
        self.renderer.status(text);
        self.paint();
        self.hint_clear_at = None;
    }

    /// Kick a background poll of the remote pane's foreground process, throttled
    /// so a mouse burst doesn't spawn an ssh per event. The result arrives as
    /// Msg::Foreground and updates `remote_is_shell`.
    fn spawn_foreground_poll(&mut self, force: bool) {
        let now = Instant::now();
        if !force && self.fg_poll_at.is_some_and(|t| now.duration_since(t) < FG_POLL_INTERVAL) {
            return;
        }
        self.fg_poll_at = Some(now);
        let tx = self.tx.clone();
        let ssh = self.args.ssh_target.clone();
        let bin = self.args.remote_bin.clone();
        let session = self.args.session.clone();
        let pane = self.args.pane_target.clone();
        let ctl = self.args.ctl_path.clone();
        let container = self.args.container.clone();
        tokio::spawn(async move {
            let v = crate::foreground::poll(
                &ssh,
                bin.as_deref(),
                session.as_deref(),
                &pane,
                ctl.as_deref(),
                container.as_ref(),
            )
            .await;
            let _ = tx.send(Msg::Foreground(v)).await;
        });
    }

    /// Match the local mouse grab to the classification: release it at a shell so
    /// herdr does native selection/scroll, keep it grabbed for a TUI (or while
    /// unknown) so clicks can be forwarded. Only writes on a change.
    fn sync_mouse_grab(&mut self) {
        if !self.tty {
            return;
        }
        // grab unless we've confirmed the foreground is a shell
        let want = self.remote_is_shell != Some(true);
        if want == self.mouse_grabbed {
            return;
        }
        self.mouse_grabbed = want;
        write_stdout(if want { "\x1b[?1002h\x1b[?1006h" } else { "\x1b[?1002l" });
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
    ///
    /// The classification is the same shell/TUI proxy the mouse grab uses, for
    /// the same reason: the API exposes no input modes to ask for directly.
    fn sync_cursor_key_mode(&mut self) {
        if !self.tty {
            return;
        }
        // a shell prompt reads arrows in normal mode; a TUI is the case that
        // sets smkx, so mirror application mode unless we've confirmed a shell
        let want = self.remote_is_shell == Some(false);
        if want == self.app_cursor_keys {
            return;
        }
        self.app_cursor_keys = want;
        write_stdout(if want { "\x1b[?1h" } else { "\x1b[?1l" });
    }

    fn observe_size(&self) -> (usize, usize) {
        observe_size_for(&self.args, if self.tty { term_size() } else { (0, 0) })
    }

    /// Size to enter (and stay in) control at. Control is authoritative on the
    /// remote — the server resizes the remote pty to whatever we ask for — so a
    /// host whose remote has its own display caps this and renders the remote at
    /// its own geometry, leaving the rest of the local pane blank rather than
    /// reflowing a screen someone over there is reading. Uncapped by default,
    /// which is the pre-existing fill-the-pane behaviour.
    fn control_size(&self) -> (usize, usize) {
        cap_size(term_size(), self.args.max_cols, self.args.max_rows)
    }

    /// Stop the child (clean release first for control) — never leave an
    /// orphan holding the remote attach lock.
    fn stop_session(&mut self) {
        if let Some(session) = self.session.take() {
            tokio::spawn(session.stop(true));
        }
    }

    async fn connect(&mut self, m: Mode) {
        self.mode = m;
        self.reconnect_at = None;
        self.switching_to = None;
        // re-earn prediction confidence against the new session's frames
        self.predict = Predictor::new();
        self.frame_assembler = crate::frame_stream::FrameAssembler::default();
        self.frame_resync_pending = false;
        self.last_frame_seq = None;
        self.liveness.disconnected();
        let (cols, rows) = match m {
            Mode::Observe => self.observe_size(),
            Mode::Control => self.control_size(),
        };
        if let Some(mut session) = self.session.take() {
            session.supervisor.cancel_and_wait().await;
        }
        self.next_gen += 1;
        let takeover = m == Mode::Control && std::mem::take(&mut self.next_control_takeover);
        let reconnect_claim = if m == Mode::Control && self.args.terminal_reconnect {
            match crate::claim_token::next_generation(&self.state_dir, &self.claim_token_key) {
                Ok(generation) => Some(ReconnectClaim {
                    token: self.claim_token,
                    generation,
                }),
                Err(error) => {
                    self.schedule_reconnect(m, &format!("cannot allocate controller generation: {error}"));
                    return;
                }
            }
        } else {
            None
        };
        match spawn_session(
            &self.args,
            m,
            takeover,
            reconnect_claim,
            (cols, rows),
            self.next_gen,
            self.tx.clone(),
        ) {
            Ok(s) => {
                self.session_ready = false;
                self.heartbeat_status_visible = self.args.terminal_reconnect;
                self.liveness
                    .connected(Instant::now(), std::time::SystemTime::now());
                if m == Mode::Control {
                    self.last_input = Instant::now();
                }
                self.session = Some(s);
                // warm the foreground classification before the user mouses
                self.spawn_foreground_poll(false);
                // always-control has no release, so no "ctrl+\ to release" hint
                let status = if self.args.terminal_reconnect {
                    "connecting — waiting for terminal.ready"
                } else if m == Mode::Observe {
                    self.ownership_hint.as_deref().unwrap_or("")
                } else if !self.args.always_control {
                    "CONTROL — ctrl+\\ to release"
                } else {
                    ""
                };
                self.renderer.status(status);
            }
            Err(e) => self.schedule_reconnect(m, &e.to_string()),
        }
    }

    fn schedule_reconnect(&mut self, m: Mode, reason: &str) {
        if self.args.dump && !reason.is_empty() {
            eprintln!("herdr-mirror: {m:?} session reconnecting: {reason}");
        }
        // Only slow down once we are back in observe. In control the existing
        // quick-failure fallback needs its fast retries to reach two failures
        // and drop the pane to observe within seconds; a 60s rung there would
        // leave an always_control pane stuck in control for a minute.
        let gone = m == Mode::Observe && target_gone(reason, &self.args.pane_target);
        let (delay, idx) = reconnect_delay(gone, self.backoff_idx);
        let delay = bound_reconnect_delay(delay, self.wake_retry_until, Instant::now());
        self.backoff_idx = idx;

        if gone {
            // Repainted every cycle on purpose: handle_frame paints herdr's
            // raw close reason before us on each attempt, so saying this once
            // would leave the misleading "terminal closed" line on screen from
            // the second cycle onward. The renderer diffs rows, so an
            // unchanged line costs one row write a minute.
            self.renderer.status(&format!("remote pane {} is gone", self.args.pane_target));
            // and nothing may expire it out from under us: the control→observe
            // fallback sets a 1.5s hint just before this path runs
            self.hint_clear_at = None;
        } else {
            let suffix = if reason.is_empty() { String::new() } else { format!(" — {reason}") };
            self.renderer
                .status(&format!("reconnecting in {}s ({}){suffix}", delay / 1000, m.as_str()));
        }
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

    async fn mark_session_ready(&mut self, claim_token: Option<u64>) {
        if self.session_ready {
            return;
        }
        self.session_ready = true;
        if self.mode == Mode::Control {
            self.claim_token = claim_token.or(self.claim_token);
            if let Some(token) = claim_token {
                if let Err(error) =
                    crate::claim_token::save(&self.state_dir, &self.claim_token_key, token)
                {
                    eprintln!("herdr-mirror: cannot persist terminal claim token: {error}");
                }
            }
        }
        let now = Instant::now();
        self.liveness.ready(now, std::time::SystemTime::now());
        if self.args.terminal_reconnect {
            self.heartbeat_status_visible = false;
            self.renderer
                .status(self.ownership_hint.as_deref().unwrap_or(""));
            self.paint();
        }
        if self.mode != Mode::Control {
            return;
        }
        self.ownership_hint = None;
        let drained = self.pending_input.drain_ready(now);
        if let Some(session) = self.session.as_mut() {
            for buf in drained.chunks {
                let line = json!({ "type": "terminal.input", "bytes": B64.encode(&buf) })
                    .to_string()
                    + "\n";
                let _ = tokio::time::timeout(
                    SESSION_WRITE_TIMEOUT,
                    session.stdin.write_all(line.as_bytes()),
                )
                .await;
            }
        }
        if drained.dropped_bytes > 0 {
            self.hint(&format!(
                "dropped {} buffered input bytes after reconnect",
                drained.dropped_bytes
            ));
        }
    }

    fn apply_terminal_frame(
        &mut self,
        seq: Option<u64>,
        full: bool,
        width: usize,
        height: usize,
        bytes: &[u8],
    ) {
        self.backoff_idx = 0;
        self.renderer
            .status(self.ownership_hint.as_deref().unwrap_or(""));
        self.grid.resize(width, height);
        if full {
            self.grid.clear();
        }
        self.grid.apply(&String::from_utf8_lossy(bytes));
        self.predict.on_frame(&self.grid);
        if self.args.dump {
            let lines: Vec<String> = self
                .grid
                .text_lines()
                .into_iter()
                .filter(|line| !line.is_empty())
                .collect();
            println!(
                "--- frame seq={seq:?} full={full:?} {width}x{height} ---\n{}",
                lines.join("\n")
            );
        } else {
            self.paint();
        }
    }

    async fn handle_frame(&mut self, gen: u64, frame: Frame) {
        if self.session.as_ref().map(|s| s.gen) != Some(gen) {
            return; // stale frame from a replaced session
        }
        match frame.kind.as_str() {
            "terminal.ready" => self.mark_session_ready(frame.claim_token).await,
            "terminal.heartbeat_ack"
                if frame
                    .nonce
                    .is_some_and(|nonce| self.liveness.acknowledge(nonce, Instant::now()))
                    && std::mem::take(&mut self.heartbeat_status_visible) =>
            {
                self.renderer
                    .status(self.ownership_hint.as_deref().unwrap_or(""));
                self.paint();
            }
            "terminal.heartbeat_ack" => {}
            "terminal.frame.start" => {
                let full = frame.full.unwrap_or(false);
                if self
                    .frame_assembler
                    .start(
                        frame.seq.unwrap_or(0),
                        frame.width.unwrap_or(0),
                        frame.height.unwrap_or(0),
                        full,
                        frame.total_bytes.unwrap_or(0),
                    )
                    .is_err()
                {
                    self.request_frame_resync().await;
                } else if full {
                    self.frame_resync_pending = false;
                }
            }
            "terminal.frame.chunk" => {
                let output_ack = frame.seq.zip(frame.index);
                let result = match (frame.seq, frame.index, frame.bytes.as_deref()) {
                    (Some(seq), Some(index), Some(bytes)) => {
                        self.frame_assembler.chunk(seq, index, bytes)
                    }
                    _ => Err(err("malformed terminal frame chunk")),
                };
                if let Some((seq, index)) = output_ack {
                    self.send(json!({
                        "type": "terminal.output_ack",
                        "seq": seq,
                        "index": index,
                    }))
                    .await;
                }
                if result.is_err() {
                    self.request_frame_resync().await;
                }
            }
            "terminal.frame.end" => {
                let result = frame
                    .seq
                    .ok_or_else(|| err("malformed terminal frame end"))
                    .and_then(|seq| self.frame_assembler.finish(seq));
                match result {
                    Ok(complete) => {
                        if !complete.full
                            && self
                                .last_frame_seq
                                .is_some_and(|last| complete.seq != last.saturating_add(1))
                        {
                            self.request_frame_resync().await;
                            return;
                        }
                        self.last_frame_seq = Some(complete.seq);
                        self.mark_session_ready(frame.claim_token).await;
                        self.apply_terminal_frame(
                            Some(complete.seq),
                            complete.full,
                            complete.width,
                            complete.height,
                            &complete.bytes,
                        );
                    }
                    Err(_) => self.request_frame_resync().await,
                }
            }
            "terminal.frame" => {
                let Some(bytes) = frame
                    .bytes
                    .as_deref()
                    .and_then(|bytes| B64.decode(bytes).ok())
                else {
                    return;
                };
                if !frame.full.unwrap_or(false)
                    && frame.seq.zip(self.last_frame_seq).is_some_and(|(seq, last)| {
                        seq != last.saturating_add(1)
                    })
                {
                    self.request_frame_resync().await;
                    return;
                }
                self.last_frame_seq = frame.seq.or(self.last_frame_seq);
                self.mark_session_ready(frame.claim_token).await;
                self.apply_terminal_frame(
                    frame.seq,
                    frame.full.unwrap_or(false),
                    frame.width.unwrap_or(self.grid.width),
                    frame.height.unwrap_or(self.grid.height),
                    &bytes,
                );
            }
            "terminal.closed" => {
                let detail = frame
                    .code
                    .as_deref()
                    .or(frame.reason.as_deref())
                    .unwrap_or("closed");
                self.renderer
                    .status(&format!("remote terminal closed: {detail}"));
                self.paint();
            }
            _ => {}
        }
    }

    fn handle_exit(&mut self, gen: u64, exited_mode: Mode, reason: String, uptime: Duration) {
        if self.session.as_ref().map(|s| s.gen) != Some(gen) {
            return; // an old child we already replaced/killed
        }
        self.session = None;
        self.session_ready = false;
        self.liveness.disconnected();
        let reason_line =
            reason.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("").to_string();
        if self.args.terminal_reconnect && reconnect_flags_unsupported(&reason_line) {
            self.args.terminal_reconnect = false;
            self.args.controller_id = None;
            self.claim_token = None;
            self.liveness.disable();
            self.heartbeat_status_visible = false;
            self.ownership_hint = Some(
                "remote Herdr no longer supports reconnect leases — using safe legacy mode"
                    .into(),
            );
            self.schedule_reconnect(exited_mode, &reason_line);
            return;
        }
        // control that dies quickly twice is failing (refused/dropped): fall
        // back to observe so the pane stays viewable; a keystroke retries
        if exited_mode == Mode::Control {
            if let Some(decision) = self.takeover_policy.decide(
                &reason_line,
                self.args.always_control,
                self.args.takeover_on_reconnect,
                Instant::now(),
            ) {
                match decision {
                    crate::ownership::CollisionDecision::RetryWithTakeover => {
                        self.next_control_takeover = true;
                        self.renderer.status("controller occupied — taking over…");
                        self.paint();
                        self.reconnect_at = Some((Instant::now() + SWITCH_GAP, Mode::Control));
                    }
                    crate::ownership::CollisionDecision::ObserveSticky => {
                        self.control_failures = 0;
                        self.control_sticky = true;
                        self.ownership_hint = Some(
                            if reason_line.ends_with(crate::ownership::TAKEN_OVER_REASON) {
                                "control taken by another client — viewing only".into()
                            } else {
                                "control held elsewhere — viewing only; takeover is disabled".into()
                            },
                        );
                        self.switch_mode(Mode::Observe);
                    }
                }
                return;
            }
            if !self.args.terminal_reconnect {
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
        }
        self.schedule_reconnect(exited_mode, &reason_line);
    }

    async fn send(&mut self, msg: serde_json::Value) -> bool {
        if let Some(s) = self.session.as_mut() {
            let line = msg.to_string() + "\n";
            return matches!(
                tokio::time::timeout(
                    SESSION_WRITE_TIMEOUT,
                    s.stdin.write_all(line.as_bytes())
                )
                .await,
                Ok(Ok(()))
            );
        }
        false
    }

    async fn request_frame_resync(&mut self) {
        if self.frame_resync_pending {
            return;
        }
        self.frame_resync_pending = true;
        self.send(json!({ "type": "terminal.resync" })).await;
    }

    async fn poll_liveness(&mut self) {
        match self
            .liveness
            .poll(Instant::now(), std::time::SystemTime::now())
        {
            Some(crate::liveness::Action::Heartbeat { nonce, wake_probe }) => {
                if wake_probe {
                    self.wake_retry_until = Some(Instant::now() + WAKE_RETRY_WINDOW);
                    self.heartbeat_status_visible = true;
                    self.renderer
                        .status("wake detected — probing terminal path…");
                    self.paint();
                }
                self.send(json!({
                    "type": "terminal.heartbeat",
                    "nonce": nonce,
                }))
                .await;
            }
            Some(crate::liveness::Action::Reconnect { waiting_for_ready }) => {
                self.renderer.status(if waiting_for_ready {
                    "terminal ready timeout — reconnecting…"
                } else {
                    "terminal heartbeat timeout — reconnecting…"
                });
                self.paint();
                self.connect(self.mode).await;
            }
            None => {}
        }
    }

    /// Drain every complete paste in this chunk, in order.
    ///
    /// Deliberately a loop, not a one-shot: two drops land in a single read
    /// with nothing between them (a drop carries no terminator at all — which
    /// is precisely why `run` asks for DECSET 2004), so handling only the
    /// first would silently swallow the second, and leave its markers in the
    /// tail to be forwarded raw at the remote.
    async fn handle_stdin(&mut self, chunk: Vec<u8>) {
        let mut chunk = chunk;
        loop {
            match split_paste(&mut self.paste_buf, chunk) {
                PasteSplit::Pending => return,
                PasteSplit::Passthrough(bytes) => return self.route_input(bytes).await,
                PasteSplit::Complete { before, body, after } => {
                    self.route_input(before).await;
                    self.route_paste_body(body).await;
                    if after.is_empty() {
                        return;
                    }
                    chunk = after;
                }
            }
        }
    }

    /// Ordinary input, held back while an upload is in flight so the pasted
    /// remote paths cannot be overtaken by whatever was typed after them.
    async fn route_input(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        if self.paste_inflight {
            self.paste_queue.push(Queued::Raw(bytes));
            return;
        }
        self.handle_stdin_inner(bytes).await;
    }

    /// A complete paste body, markers already stripped. A file drop is
    /// uploaded; anything else is forwarded verbatim as if it had been typed.
    async fn route_paste_body(&mut self, body: Vec<u8>) {
        if self.paste_inflight {
            self.paste_queue.push(Queued::Body(body));
            return;
        }
        // lossy only for the probe; the forward path keeps the original bytes
        let Some(paths) = crate::paste::dropped_paths(&String::from_utf8_lossy(&body)) else {
            self.handle_stdin_inner(body).await;
            return;
        };

        self.paste_inflight = true;
        self.paste_original = Some(body);
        self.hint_sticky(&format!("uploading {} file(s)…", paths.len()));
        let tx = self.tx.clone();
        let ssh = self.args.ssh_target.clone();
        let ctl = self.args.ctl_path.clone();
        let container = self.args.container.clone();
        tokio::spawn(async move {
            let result =
                crate::paste::files_to_remote(&paths, &ssh, ctl.as_deref(), container.as_ref())
                    .await;
            let _ = tx.send(Msg::Drop(result)).await;
        });
    }

    async fn handle_drop(&mut self, result: crate::paste::DropResult) {
        self.paste_inflight = false;
        let original = self.paste_original.take();
        if let Some(text) = &result.text {
            self.deliver_input(crate::paste::bracketed(text)).await;
            self.hint(&format!("→ {text}"));
        } else if result.unchanged {
            // every path already exists over there, so the user meant those
            // files: forward what they actually dropped
            if let Some(body) = original {
                self.handle_stdin_inner(body).await;
            }
        }
        if let Some(e) = result.error {
            self.hint(&format!("drop failed: {e}"));
        }
        self.drain_paste_queue().await;
    }

    /// Flush input held during an upload. Stops if a queued drop starts a new
    /// upload, leaving the remainder queued behind it so order is preserved.
    async fn drain_paste_queue(&mut self) {
        let mut items = std::mem::take(&mut self.paste_queue).into_iter();
        while let Some(item) = items.next() {
            match item {
                Queued::Raw(b) => self.handle_stdin_inner(b).await,
                Queued::Body(b) => {
                    self.route_paste_body(b).await;
                    if self.paste_inflight {
                        self.paste_queue.extend(items);
                        return;
                    }
                }
            }
        }
    }

    async fn handle_stdin_inner(&mut self, buf: Vec<u8>) {
        if buf.len() == 1 && buf[0] == 0x16 && !self.paste_inflight {
            self.paste_inflight = true;
            let tx = self.tx.clone();
            let ssh = self.args.ssh_target.clone();
            let ctl = self.args.ctl_path.clone();
            let container = self.args.container.clone();
            tokio::spawn(async move {
                let outcome =
                    crate::paste::clipboard_to_remote(&ssh, ctl.as_deref(), container.as_ref())
                        .await;
                let _ = tx.send(Msg::Paste(outcome)).await;
            });
            return;
        }
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
            self.ownership_hint = None;
            self.pending_input.push(Instant::now(), buf);
            self.switch_mode(Mode::Control);
            return;
        }

        // control mode
        self.last_input = Instant::now();
        if buf.len() == 1 && buf[0] == 0x1c {
            // ctrl+\ — manual release. In always-control there's nothing to
            // release to, so swallow it (never forward it: ctrl+\ is SIGQUIT).
            if !self.args.always_control {
                self.control_sticky = false;
                self.switch_mode(Mode::Observe);
            }
            return;
        }
        if control_input_needs_buffer(
            self.switching_to,
            self.session.is_some(),
            self.session_ready,
        ) {
            // spinning up or awaiting reconnect: queue the keystroke (flushed
            // on connect) and, if in backoff, reconnect now
            self.pending_input.push(Instant::now(), buf);
            if let Some((_, m)) = self.reconnect_at {
                self.reconnect_at = Some((Instant::now(), m));
            }
            return;
        }
        // refresh the foreground classification on any input while active.
        // keyboard reaches us even when the grab is released at a shell, so this
        // is what catches a shell→TUI switch — a released grab means mouse events
        // stop arriving here, so mouse alone can never trigger the re-poll.
        self.spawn_foreground_poll(false);
        // and re-check shortly after input settles, to catch a change the input
        // just caused (e.g. `:q` quitting vim — the poll above still sees vim)
        self.settle_at = Some(Instant::now() + SETTLE_DELAY);
        // wheel becomes a semantic scroll (server decides app vs scrollback);
        // clicks/drags forward to the remote pty only when the foreground is a
        // TUI — at a shell they're dropped so they don't garbage the prompt
        let mut rest: Vec<u8> = Vec::with_capacity(buf.len());
        let mut i = 0usize;
        let mut scrolls: Vec<serde_json::Value> = Vec::new();
        while i < buf.len() {
            if let Some((btn, x, y, press, len)) = parse_mouse(&buf, i) {
                match mouse_action(self.remote_is_shell, btn, press) {
                    MouseAction::Scroll { up } => {
                        scrolls.push(json!({
                            "type": "terminal.scroll",
                            "direction": if up { "up" } else { "down" },
                            "lines": 3,
                            "source": "wheel",
                            "column": x.saturating_sub(1),
                            "row": y.saturating_sub(1),
                            "modifiers": 0,
                        }));
                    }
                    MouseAction::ForwardRaw => rest.extend_from_slice(&buf[i..i + len]),
                    MouseAction::Drop => {}
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
            // optimistic local echo: draw the keystroke now, verify on frame
            if self.predict.on_input(&rest, &self.grid) {
                self.paint();
            }
        }
    }

    async fn deliver_input(&mut self, buf: Vec<u8>) {
        if self.mode == Mode::Observe || self.switching_to == Some(Mode::Observe) {
            self.control_sticky = false;
            self.pending_input.push(Instant::now(), buf);
            self.switch_mode(Mode::Control);
            return;
        }
        self.last_input = Instant::now();
        if control_input_needs_buffer(
            self.switching_to,
            self.session.is_some(),
            self.session_ready,
        ) {
            self.pending_input.push(Instant::now(), buf);
            if let Some((_, m)) = self.reconnect_at {
                self.reconnect_at = Some((Instant::now(), m));
            }
            return;
        }
        self.send(json!({ "type": "terminal.input", "bytes": B64.encode(&buf) })).await;
    }

    async fn handle_paste(&mut self, outcome: crate::paste::Outcome) {
        self.paste_inflight = false;
        match outcome {
            crate::paste::Outcome::NoImage => self.deliver_input(vec![0x16]).await,
            crate::paste::Outcome::Pasted(path) => {
                self.deliver_input(crate::paste::bracketed(&path)).await;
                self.hint(&format!("→ {path}"));
            }
            crate::paste::Outcome::Failed(e) => {
                self.hint(&format!("image paste failed: {e}"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// main

pub async fn run(mut args: Args, state_dir: std::path::PathBuf) -> Result<()> {
    // Claim the cross-process streamer slot before opening SSH. --dump is a
    // human diagnostic and deliberately does not participate in daemon healing.
    let scope = args.controller_scope.as_deref().unwrap_or(&args.ssh_target);
    let transport = if args.container.is_some() {
        "docker"
    } else {
        "ssh"
    };
    let identity = crate::streamer_lock::StreamerIdentity {
        transport,
        controller_scope: scope,
        target: &args.ssh_target,
        session: args.session.as_deref(),
        pane: &args.pane_target,
    };
    let computed = identity.key();
    if args.terminal_reconnect {
        args.controller_id = Some(crate::controller_identity::controller_id(&state_dir, scope)?);
    }
    let claim_token = if args.terminal_reconnect {
        match crate::claim_token::load(&state_dir, &computed) {
            Ok(token) => token,
            Err(error) => {
                eprintln!("herdr-mirror: ignoring invalid terminal claim token: {error}");
                None
            }
        }
    } else {
        None
    };
    let _streamer_lock = if args.dump {
        None
    } else {
        if args
            .streamer_key
            .as_deref()
            .is_some_and(|key| key != computed)
        {
            return Err(err("streamer key does not match pane identity"));
        }
        Some(crate::streamer_lock::StreamerLock::acquire(
            &state_dir, &identity,
        )?)
    };

    let tty = !args.dump && unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1;
    let raw = if tty {
        // 1002/1006: button-event mouse tracking with SGR encoding, so wheel and
        // clicks reach us instead of scrolling the hosting pane's scrollback
        // 2004 (bracketed paste) is asked for purely to get framing: herdr
        // only wraps a paste when the pane's app has enabled it, and a file
        // drop otherwise arrives as bare text with no terminator at all. See
        // `intercept_paste`; the markers never reach the remote.
        write_stdout("\x1b[?1049h\x1b[2J\x1b[H\x1b[?1002h\x1b[?1006h\x1b[?2004h");
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

    let now = Instant::now();
    let terminal_reconnect = args.terminal_reconnect;
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
        wake_retry_until: None,
        control_failures: 0,
        control_sticky: false,
        takeover_policy: crate::ownership::LegacyTakeoverPolicy::default(),
        next_control_takeover: false,
        session_ready: false,
        claim_token,
        claim_token_key: computed,
        state_dir,
        heartbeat_status_visible: false,
        ownership_hint: None,
        frame_assembler: crate::frame_stream::FrameAssembler::default(),
        frame_resync_pending: false,
        last_frame_seq: None,
        liveness: crate::liveness::Liveness::new(
            terminal_reconnect,
            now,
            std::time::SystemTime::now(),
        ),
        pending_input: crate::pending_input::PendingInput::default(),
        last_input: now,
        hint_clear_at: None,
        predict: Predictor::new(),
        remote_is_shell: None,
        fg_poll_at: None,
        settle_at: None,
        mouse_grabbed: tty, // startup wrote ?1002h when we're a tty
        // startup leaves the pane in normal cursor mode; the first classification
        // moves it if the remote turns out to be a TUI
        app_cursor_keys: false,
        paste_inflight: false,
        paste_buf: Vec::new(),
        paste_queue: Vec::new(),
        paste_original: None,
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

    app.connect(initial_mode(app.args.always_control, term_size())).await;
    // the pane may have been laid out while the session was spawning; the signal
    // for that is buffered above, but check directly too
    if app.mode == Mode::Observe && initial_mode(app.args.always_control, term_size()) == Mode::Control
    {
        app.switch_mode(Mode::Control);
    } else if app.args.always_control && app.mode == Mode::Observe {
        // F3: otherwise the pane is inert with no explanation
        app.hint("read-only until this pane is sized — type to take control");
    }

    loop {
        // Every pane event comes back through this loop, so sleep/wake clock
        // divergence is noticed immediately when any activity resumes. With no
        // activity, the two-second heartbeat deadline is the upper bound.
        app.poll_liveness().await;
        // earliest pending deadline: mode-switch gap, reconnect, hint clear, idle release
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
            app.settle_at,
            app.liveness.heartbeat_at(),
            app.liveness.watchdog_at(),
        ]);

        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    None => break,
                    Some(Msg::Frame { gen, frame }) => app.handle_frame(gen, frame).await,
                    Some(Msg::SessionExit { gen, mode, reason, uptime }) => app.handle_exit(gen, mode, reason, uptime),
                    Some(Msg::Stdin(buf)) => app.handle_stdin(buf).await,
                    // keep the last good classification if a poll failed (None)
                    Some(Msg::Foreground(v)) => if v.is_some() {
                        app.remote_is_shell = v;
                        app.sync_mouse_grab();
                        app.sync_cursor_key_mode();
                    },
                    Some(Msg::Paste(outcome)) => app.handle_paste(outcome).await,
                    Some(Msg::Drop(result)) => app.handle_drop(result).await,
                }
            }
            _ = sigwinch.recv() => {
                app.renderer.invalidate();
                // a resize means a client is laying this pane out, so the size is
                // now a real viewport: take control if that is what we're for.
                // control_sticky means control was refused twice in a row and we
                // told the user "type to retry" — a window drag must not turn
                // that into a reconnect storm.
                if app.args.always_control && app.mode == Mode::Observe && !app.control_sticky {
                    app.switch_mode(Mode::Control);
                }
                if app.mode == Mode::Control {
                    // capped like the initial connect: a local window drag must
                    // not push a capped host past its ceiling either
                    let (cols, rows) = app.control_size();
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
                if app.settle_at.is_some_and(|t| t <= now) {
                    app.settle_at = None;
                    app.spawn_foreground_poll(true); // forced: bypass the throttle
                }
                if app.predict.deadline().is_some_and(|t| t <= now) {
                    app.predict.on_tick(); // wipe timed-out ghosts (no-echo prompts)
                    app.paint();
                }
            }
        }
    }

    // clean shutdown: release control if held, kill the ssh child, restore tty
    if let Some(session) = app.session.take() {
        session.stop(true).await;
    }
    if tty {
        // ?1l with the rest: leaving the hosting pane in application cursor mode
        // would misencode arrows for whatever runs there next
        write_stdout("\x1b[?2004l\x1b[?1002l\x1b[?1006l\x1b[?1l\x1b[?25h\x1b[?1049l");
    }
    if let Some(raw) = raw {
        raw.restore();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Uncapped must stay byte-identical to the old `term_size()` call, or
    /// every existing headless-remote config silently changes behaviour.
    #[test]
    fn uncapped_control_size_is_the_local_size() {
        assert_eq!(cap_size((253, 50), None, None), (253, 50));
    }

    #[test]
    fn caps_only_bite_when_the_local_pane_is_bigger() {
        // the real case: local 253 cols vs a laptop that renders at 212
        assert_eq!(cap_size((253, 50), Some(212), Some(58)), (212, 50));
        // a local pane smaller than the cap is left alone — a cap is a ceiling,
        // never a demand for a size the local window can't show
        assert_eq!(cap_size((120, 30), Some(212), Some(58)), (120, 30));
        // one axis capped, the other free
        assert_eq!(cap_size((253, 50), Some(212), None), (212, 50));
        assert_eq!(cap_size((253, 90), None, Some(58)), (253, 58));
        // equal is not clamped away
        assert_eq!(cap_size((212, 58), Some(212), Some(58)), (212, 58));
    }

    #[test]
    fn wheel_always_semantic_scroll_even_on_tui_foreground() {
        // remote foreground classified as a TUI (e.g. `claude`) — wheel must
        // still produce a semantic scroll, not a raw forward, or it silently
        // does nothing when the TUI doesn't consume mouse wheel input
        assert_eq!(mouse_action(Some(false), 64, true), MouseAction::Scroll { up: true });
        assert_eq!(mouse_action(Some(false), 65, true), MouseAction::Scroll { up: false });
        // unclassified/shell foreground: wheel still scrolls
        assert_eq!(mouse_action(None, 64, true), MouseAction::Scroll { up: true });
        assert_eq!(mouse_action(Some(true), 65, true), MouseAction::Scroll { up: false });
        // non-wheel clicks/drags keep the existing foreground-based routing
        assert_eq!(mouse_action(Some(false), 0, true), MouseAction::ForwardRaw); // TUI click
        assert_eq!(mouse_action(Some(true), 0, true), MouseAction::Drop); // shell click
        assert_eq!(mouse_action(None, 0, true), MouseAction::Drop); // unclassified click
    }

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
    fn observe_size_treats_daemon_sizes_as_a_floor() {
        // what the daemon spawns a streamer with for a headless remote: the
        // no-client placeholder rect plus its margin
        let argv: Vec<String> = ["work", "w1:p1", "--cols", "70", "--rows", "31"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let a = parse_args(&argv).unwrap();
        // control has already resized the remote pty to this pane, and release
        // does not shrink it back — observing at 70x31 would stream a crop
        assert_eq!(observe_size_for(&a, (314, 92)), (314, 92));
        // a pane smaller than the remote still gets the daemon's margin
        assert_eq!(observe_size_for(&a, (40, 20)), (70, 31));
        // --dump has no tty: exactly what was asked for
        assert_eq!(observe_size_for(&a, (0, 0)), (70, 31));
    }

    #[test]
    fn a_zero_cap_on_the_cli_is_unset_not_a_zero_request() {
        // herdr rejects a 0-column terminal, so a typo would kill the session
        // twice and strand the pane in "control unavailable"
        let argv: Vec<String> = ["h", "w1:p1", "--max-cols", "0", "--max-rows", "0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let a = parse_args(&argv).unwrap();
        assert_eq!(a.max_cols, None);
        assert_eq!(a.max_rows, None);

        let argv: Vec<String> =
            ["h", "w1:p1", "--max-cols", "212"].iter().map(|s| s.to_string()).collect();
        assert_eq!(parse_args(&argv).unwrap().max_cols, Some(212));
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
        assert_eq!(a.remote_bin.as_deref(), Some("/opt/herdr"));
        assert_eq!((a.cols, a.rows), (176, 66));
        assert!(parse_args(&["onlyone".to_string()]).is_err());
        assert!(parse_args(&["a".into(), "b".into(), "--visibility-file".into(), "x".into()]).is_err());
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
        assert_eq!(initial_mode(true, (54, 23)), Mode::Observe, "placeholder-sized");
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

    // --- paste framing -----------------------------------------------------
    // The bugs these pin were all real: a one-shot version dropped the second
    // drop in a read, leaked markers from the tail, and corrupted non-UTF-8.

    const S: &[u8] = b"\x1b[200~";
    const E: &[u8] = b"\x1b[201~";

    fn split(buf: &mut Vec<u8>, chunk: &[u8]) -> PasteSplit {
        split_paste(buf, chunk.to_vec())
    }

    fn seq(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn ordinary_typing_passes_straight_through() {
        let mut buf = Vec::new();
        assert_eq!(split(&mut buf, b"a"), PasteSplit::Passthrough(b"a".to_vec()));
        assert_eq!(split(&mut buf, b"\x1b[A"), PasteSplit::Passthrough(b"\x1b[A".to_vec()));
        assert!(buf.is_empty(), "typing must not buffer");
    }

    #[test]
    fn paste_split_across_reads_reassembles() {
        let mut buf = Vec::new();
        assert_eq!(split(&mut buf, &seq(&[S, b"he"])), PasteSplit::Pending);
        assert_eq!(split(&mut buf, b"llo"), PasteSplit::Pending);
        assert_eq!(
            split(&mut buf, E),
            PasteSplit::Complete { before: vec![], body: b"hello".to_vec(), after: vec![] }
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn start_marker_split_across_reads_is_not_leaked() {
        // the 6-byte introducer straddling a read boundary must be held, not
        // forwarded in pieces (which would print ESC[20 at the remote)
        let mut buf = Vec::new();
        assert_eq!(split(&mut buf, b"\x1b[20"), PasteSplit::Pending);
        assert_eq!(
            split(&mut buf, &seq(&[b"0~/tmp/a.png", E])),
            PasteSplit::Complete { before: vec![], body: b"/tmp/a.png".to_vec(), after: vec![] }
        );
    }

    #[test]
    fn bytes_around_the_markers_are_preserved() {
        let mut buf = Vec::new();
        assert_eq!(
            split(&mut buf, &seq(&[b"pre", S, b"mid", E, b"post"])),
            PasteSplit::Complete {
                before: b"pre".to_vec(),
                body: b"mid".to_vec(),
                after: b"post".to_vec(),
            }
        );
    }

    #[test]
    fn two_pastes_in_one_read_are_drained_in_order() {
        // the regression: only the first was handled and the rest discarded,
        // so a second drop vanished and its markers reached the remote raw
        let mut buf = Vec::new();
        let PasteSplit::Complete { body, after, .. } =
            split(&mut buf, &seq(&[S, b"one", E, S, b"two", E]))
        else {
            panic!("expected first paste")
        };
        assert_eq!(body, b"one");
        assert_eq!(
            split(&mut buf, &after),
            PasteSplit::Complete { before: vec![], body: b"two".to_vec(), after: vec![] },
            "feeding the tail back must yield the second paste"
        );
    }

    #[test]
    fn keystroke_after_a_paste_survives() {
        let mut buf = Vec::new();
        let PasteSplit::Complete { after, .. } = split(&mut buf, &seq(&[S, b"/tmp/a", E, b"\r"]))
        else {
            panic!("expected paste")
        };
        assert_eq!(after, b"\r", "the trailing keystroke must not be eaten");
    }

    #[test]
    fn unterminated_paste_flushes_at_the_cap_without_duplicating() {
        let mut buf = Vec::new();
        assert_eq!(split(&mut buf, S), PasteSplit::Pending);
        let big = vec![b'x'; MAX_PASTE_BYTES + 1];
        let PasteSplit::Passthrough(out) = split(&mut buf, &big) else {
            panic!("expected a flush")
        };
        assert_eq!(out.len(), S.len() + big.len(), "every byte exactly once");
        assert!(buf.is_empty(), "buffer must not keep a copy");
    }

    #[test]
    fn non_utf8_paste_body_is_forwarded_byte_exact() {
        // the body is only lossily decoded to probe for paths; what gets
        // forwarded must be the original bytes
        let mut buf = Vec::new();
        let PasteSplit::Complete { body, .. } = split(&mut buf, &seq(&[S, b"caf\xe9", E])) else {
            panic!("expected paste")
        };
        assert_eq!(body, b"caf\xe9", "0xE9 must not become U+FFFD");
    }

    #[test]
    fn target_gone_matches_only_this_pane_being_gone() {
        // the real sentence, captured from herdr against a missing pane
        assert!(target_gone(
            "terminal session observe failed: terminal target w9Z:p99 not found",
            "w9Z:p99"
        ));
        assert!(target_gone(
            "terminal session control failed: terminal target w1:p1 not found",
            "w1:p1"
        ));

        // the false positive that matters most: herdr absent on the remote.
        // The auto-resolver execs `$(command -v herdr || echo ~/.local/bin/herdr)`,
        // so a host without herdr fails with a shell not-found — a different
        // problem that a slow rung would wrongly paper over.
        assert!(!target_gone("sh: 1: exec: /home/u/.local/bin/herdr: not found", "w9Z:p99"));

        // a target that merely shares our prefix: p1 and p10 are both real ids
        assert!(!target_gone(
            "terminal session observe failed: terminal target w1:p10 not found",
            "w1:p1"
        ));
        // ...and another pane's disappearance is not ours
        assert!(!target_gone(
            "terminal session observe failed: terminal target w1:p4 not found",
            "w9Z:p99"
        ));

        // ordinary transients stay on the fast ladder
        assert!(!target_gone("api timeout: session.snapshot", "w9Z:p99"));
        assert!(!target_gone("ssh timeout", "w9Z:p99"));
        assert!(!target_gone("", "w9Z:p99"));
        // an empty target must not turn `contains` into "matches everything"
        assert!(!target_gone("terminal target w1:p1 not found", ""));
    }

    #[test]
    fn remote_downgrade_is_detected_only_from_reconnect_flag_rejection() {
        assert!(reconnect_flags_unsupported(
            "error: unexpected argument '--controller-id' found"
        ));
        assert!(reconnect_flags_unsupported("unknown option: --lease-ms"));
        assert!(!reconnect_flags_unsupported("ssh: connection timed out"));
        assert!(!reconnect_flags_unsupported("unknown host"));
    }

    #[test]
    fn a_gone_target_slows_down_without_consuming_the_ladder() {
        // the fix: 10s forever becomes one attempt a minute
        assert_eq!(reconnect_delay(true, 0), (GONE_BACKOFF_MS, 0));
        assert_eq!(reconnect_delay(true, 3), (GONE_BACKOFF_MS, 3));

        // the fast ladder is unchanged and still clamps at its last rung
        assert_eq!(reconnect_delay(false, 0), (1000, 1));
        assert_eq!(reconnect_delay(false, 1), (2000, 2));
        assert_eq!(reconnect_delay(false, 3), (10000, 4));
        assert_eq!(reconnect_delay(false, 99), (10000, 100));

        // a gone spell must not burn rungs: a transient afterwards resumes
        // where the ladder was, rather than restarting at 1s
        let (_, idx) = reconnect_delay(false, 0);
        let (_, idx) = reconnect_delay(true, idx);
        assert_eq!(reconnect_delay(false, idx), (2000, 2));
    }

    #[test]
    fn control_input_stays_buffered_until_the_candidate_is_ready() {
        assert!(control_input_needs_buffer(None, true, false));
        assert!(control_input_needs_buffer(Some(Mode::Control), true, true));
        assert!(control_input_needs_buffer(None, false, false));
        assert!(!control_input_needs_buffer(None, true, true));
    }

    #[test]
    fn wake_recovery_caps_every_backoff_rung_for_thirty_seconds() {
        let now = Instant::now();
        let wake_retry_until = Some(now + WAKE_RETRY_WINDOW);
        for delay in [1_000, 2_000, 5_000, 10_000, GONE_BACKOFF_MS] {
            assert_eq!(
                bound_reconnect_delay(delay, wake_retry_until, now),
                1_000
            );
        }
        assert_eq!(
            bound_reconnect_delay(10_000, wake_retry_until, now + WAKE_RETRY_WINDOW),
            10_000
        );
    }

}
