// Predictive local echo (mosh-style, simplified) for control-mode mirror panes.
//
// Keystroke echo over the mirror costs a network round-trip plus a frame
// render (~100ms coast-to-coast), which reads as lag when typing. This module
// draws printable keystrokes into the local pane immediately and reconciles
// them against the authoritative frames when they arrive.
//
// Safety valve: predictions are VERIFIED silently before being displayed —
// optimistic echo turns on only after CONFIRM_THRESHOLD consecutive
// keystrokes are confirmed by frames, and one contradiction turns it back
// off. Full-screen apps (vim normal mode, htop) and no-echo prompts
// (passwords) therefore self-disable, and a timeout wipes unconfirmed ghosts
// so predicted password characters never linger on screen.

use std::time::Duration;

use tokio::time::Instant; // matches the pane loop's deadline type

use crate::grid::Grid;

const CONFIRM_THRESHOLD: u32 = 2;
const STREAK_CAP: u32 = 10;
const MAX_PENDING: usize = 32;
const TIMEOUT: Duration = Duration::from_millis(1200);

struct Pending {
    row: usize,
    col: usize,
    ch: char,
    at: Instant,
}

/// Position inside a terminal escape sequence. Sequences are commands (arrows,
/// shift+enter's CSI-u, mouse reports forwarded to a remote TUI), but their
/// bodies are printable ASCII, so a byte-at-a-time classifier happily echoes
/// the *encoding* of a keypress as ghost text.
#[derive(Default, Clone, Copy, PartialEq)]
enum EscState {
    #[default]
    Ground,
    /// saw ESC; still ambiguous with a bare Escape keypress
    Esc,
    /// ESC [ ... : arrows, CSI-u, SGR mouse, function keys
    Csi,
    /// ESC O x : application-mode arrows
    Ss3,
    /// ESC ] ... : terminated by BEL or ST
    Osc,
    OscEsc,
}

/// a malformed run must never swallow real typing forever
const MAX_ESC_LEN: usize = 64;

#[derive(Default)]
pub struct Predictor {
    pending: Vec<Pending>,
    /// consecutive frame-confirmed predictions; display gate
    streak: u32,
    /// a prediction was cleared without a frame repainting it — the renderer
    /// must invalidate so ghost characters get wiped
    dirty: bool,
    /// escape-sequence position. Persists across calls: a mouse burst can be
    /// split mid-sequence by the reader's 1024-byte boundary.
    esc: EscState,
    esc_len: usize,
}

// debug tracing (temporary): HERDR_MIRROR_PREDICT_LOG=1 or presence of the
// marker file enables appending predictor events to ~/.local/state/herdr-mirror/predict.log
fn dbg(msg: &str) {
    use std::io::Write as _;
    let Some(home) = std::env::var_os("HOME") else { return };
    let dir = std::path::Path::new(&home).join(".local/state/herdr-mirror");
    if !dir.join("predict-debug-on").exists() {
        return;
    }
    if let Ok(mut f) =
        std::fs::OpenOptions::new().create(true).append(true).open(dir.join("predict.log"))
    {
        let _ = writeln!(f, "[{}] {}", std::process::id(), msg);
    }
}

impl Predictor {
    pub fn new() -> Predictor {
        Predictor::default()
    }

    fn displaying(&self) -> bool {
        self.streak >= CONFIRM_THRESHOLD
    }

    /// Consume one byte of an escape sequence. Returns true when the byte
    /// belonged to one, and so must never become a prediction.
    fn consume_escape(&mut self, b: u8, changed: &mut bool) -> bool {
        if self.esc == EscState::Ground {
            if b != 0x1b {
                return false;
            }
            // same reasoning as the control-byte arm: the line's fate is
            // unknowable locally, so drop optimism and let frames drive
            if !self.pending.is_empty() {
                self.dirty = true;
                *changed = true;
                self.pending.clear();
            }
            self.esc = EscState::Esc;
            self.esc_len = 0;
            return true;
        }
        self.esc_len += 1;
        if self.esc_len > MAX_ESC_LEN {
            // never seen a terminator: assume garbage and hand the byte back
            self.esc = EscState::Ground;
            return false;
        }
        match self.esc {
            EscState::Esc => {
                self.esc = match b {
                    b'[' => EscState::Csi,
                    b'O' => EscState::Ss3,
                    b']' => EscState::Osc,
                    // two-byte escape (alt+key): a command, not typed text
                    _ => EscState::Ground,
                };
            }
            EscState::Csi => {
                if (0x40..=0x7e).contains(&b) {
                    self.esc = EscState::Ground; // final byte
                } else if !(0x20..=0x3f).contains(&b) {
                    // C0 control inside a CSI: malformed. Bail rather than risk
                    // eating the keystrokes that follow.
                    self.esc = EscState::Ground;
                    return false;
                }
            }
            EscState::Ss3 => self.esc = EscState::Ground,
            EscState::Osc => {
                if b == 0x07 {
                    self.esc = EscState::Ground;
                } else if b == 0x1b {
                    self.esc = EscState::OscEsc;
                }
            }
            EscState::OscEsc => {
                self.esc = if b == b'\\' { EscState::Ground } else { EscState::Osc };
            }
            EscState::Ground => return false,
        }
        true
    }

    /// Feed user keystrokes (after mouse extraction). Escape sequences are
    /// consumed whole and never predicted. Returns true when the visible
    /// overlay may have changed and an immediate repaint helps.
    pub fn on_input(&mut self, bytes: &[u8], grid: &Grid) -> bool {
        let mut changed = false;
        for &b in bytes {
            if self.consume_escape(b, &mut changed) {
                continue;
            }
            match b {
                0x20..=0x7e => {
                    if self.pending.len() >= MAX_PENDING {
                        continue;
                    }
                    let (row, col) = match self.pending.last() {
                        Some(p) => (p.row, p.col + 1),
                        None => (grid.cursor_row, grid.cursor_col),
                    };
                    if row >= grid.height || col >= grid.width {
                        dbg(&format!(
                            "skip '{}' off-grid: pos=({row},{col}) grid={}x{} cursor=({},{})",
                            b as char, grid.width, grid.height, grid.cursor_row, grid.cursor_col
                        ));
                        continue; // off-grid (or pre-first-frame): don't predict
                    }
                    dbg(&format!("push '{}' at ({row},{col}) streak={}", b as char, self.streak));
                    self.pending.push(Pending { row, col, ch: b as char, at: Instant::now() });
                    changed = true;
                }
                0x7f | 0x08 => {
                    // backspace only cancels our own optimism; erasing real
                    // remote content is the frame's job
                    if self.pending.pop().is_some() {
                        self.dirty = true;
                        changed = true;
                    }
                }
                _ => {
                    // enter / escape sequences / control chars: the line's
                    // fate is unknowable locally — drop optimism, frames drive
                    if !self.pending.is_empty() {
                        self.dirty = true;
                        changed = true;
                        self.pending.clear();
                    }
                }
            }
        }
        // A chunk ending on a bare ESC is the Escape KEY, not a truncated
        // sequence: terminals emit sequences in a single write. Resetting keeps
        // the next chunk's first letter predictable (Escape then typing in vim),
        // while deeper states still carry over so a split mouse burst stays safe.
        if self.esc == EscState::Esc {
            self.esc = EscState::Ground;
        }
        changed
    }

    /// Reconcile against a freshly applied frame: confirm in order, bust on
    /// contradiction or timeout.
    pub fn on_frame(&mut self, grid: &Grid) {
        let now = Instant::now();
        while let Some(p) = self.pending.first() {
            let cell =
                grid.rows.get(p.row).and_then(|r| r.get(p.col)).and_then(|c| c.as_ref());
            if cell.map(|c| c.ch) == Some(p.ch) {
                dbg(&format!("confirm '{}' at ({},{}) streak->{}", p.ch, p.row, p.col, self.streak + 1));
                self.pending.remove(0);
                self.streak = (self.streak + 1).min(STREAK_CAP);
                continue;
            }
            let cursor_passed = grid.cursor_row > p.row
                || (grid.cursor_row == p.row && grid.cursor_col > p.col);
            if (cursor_passed && cell.is_some()) || now.duration_since(p.at) > TIMEOUT {
                dbg(&format!(
                    "BUST '{}' at ({},{}): cell={:?} cursor=({},{}) passed={} aged={:?}",
                    p.ch, p.row, p.col, cell.map(|c| c.ch),
                    grid.cursor_row, grid.cursor_col, cursor_passed, now.duration_since(p.at)
                ));
                self.bust();
            }
            break;
        }
    }

    /// Earliest instant at which a pending prediction times out.
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.first().map(|p| p.at + TIMEOUT)
    }

    /// Expire timed-out predictions (main-loop deadline arm).
    pub fn on_tick(&mut self) {
        if self.pending.first().is_some_and(|p| p.at.elapsed() > TIMEOUT) {
            self.bust();
        }
    }

    fn bust(&mut self) {
        self.streak = 0;
        if !self.pending.is_empty() {
            self.dirty = true;
        }
        self.pending.clear();
    }

    /// Take the "needs renderer invalidation" flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// ANSI overlay for displayed predictions. Window math mirrors
    /// Renderer::paint (bottom-anchored).
    pub fn overlay(&self, grid: &Grid, out_cols: usize, out_rows: usize) -> String {
        use std::fmt::Write as _;
        if !self.displaying() || self.pending.is_empty() {
            return String::new();
        }
        let bottom = grid.content_bottom.max(grid.cursor_row);
        let offset_r = (bottom + 1).saturating_sub(out_rows);
        let mut out = String::new();
        let mut last: Option<(usize, usize)> = None;
        for p in &self.pending {
            if p.row < offset_r {
                continue;
            }
            let wr = p.row - offset_r;
            if wr >= out_rows || p.col >= out_cols {
                continue;
            }
            // draw the optimistic echo plain (no underline): prediction is meant
            // to be invisible when right; the confirming frame overwrites it.
            let _ = write!(out, "\x1b[{};{}H\x1b[0m{}\x1b[0m", wr + 1, p.col + 1, p.ch);
            last = Some((wr, p.col));
        }
        if let Some((wr, col)) = last {
            if grid.cursor_visible {
                // park the cursor after the last predicted char
                let _ = write!(out, "\x1b[{};{}H", wr + 1, (col + 2).min(out_cols));
            }
        }
        out
    }

    #[cfg(test)]
    fn age_all(&mut self, d: Duration) {
        for p in &mut self.pending {
            p.at -= d;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid_at(text: &str, cursor_col: usize) -> Grid {
        let mut g = Grid::new();
        g.resize(20, 2);
        g.apply(&format!("\x1b[1;1H{text}\x1b[1;{}H", cursor_col + 1));
        g
    }

    #[test]
    fn silent_until_confirmed_then_displays() {
        let mut p = Predictor::new();
        let g = grid_at("$ ", 2);
        assert!(p.on_input(b"ls", &g));
        // not yet confident: overlay hidden while predictions verify silently
        assert_eq!(p.overlay(&g, 20, 2), "");
        // frame echoes both chars → confirmed → display gate opens
        let g2 = grid_at("$ ls", 4);
        p.on_frame(&g2);
        assert!(p.pending.is_empty());
        p.on_input(b" -l", &g2);
        assert!(p.overlay(&g2, 20, 2).contains('l'));
    }

    #[test]
    fn contradiction_busts_and_hides() {
        let mut p = Predictor::new();
        let g = grid_at("$ ls", 4);
        p.on_frame(&g); // no-op
        p.streak = 5; // pretend confident
        p.on_input(b"x", &g);
        assert!(!p.overlay(&g, 20, 2).is_empty());
        // frame shows a different char there and the cursor moved past
        let g2 = grid_at("$ lsQ", 5);
        p.on_frame(&g2);
        assert_eq!(p.streak, 0);
        assert!(p.pending.is_empty());
        assert!(p.take_dirty());
        assert_eq!(p.overlay(&g2, 20, 2), "");
    }

    #[test]
    fn backspace_pops_enter_clears() {
        let mut p = Predictor::new();
        let g = grid_at("$ ", 2);
        p.on_input(b"ab", &g);
        assert_eq!(p.pending.len(), 2);
        p.on_input(&[0x7f], &g);
        assert_eq!(p.pending.len(), 1);
        p.on_input(b"\r", &g);
        assert!(p.pending.is_empty());
    }

    /// confident predictor at a shell prompt: the state every ghost-garbage
    /// report starts from
    fn confident() -> (Predictor, Grid) {
        let mut p = Predictor::new();
        p.streak = 5;
        (p, grid_at("$ ", 2))
    }

    #[test]
    fn shift_enter_is_not_predicted() {
        let (mut p, g) = confident();
        // ghostty encodes shift+enter as CSI-u; every byte after ESC is printable
        p.on_input(b"\x1b[13;2u", &g);
        assert!(p.pending.is_empty());
        assert_eq!(p.overlay(&g, 20, 2), "");
    }

    #[test]
    fn arrow_and_function_keys_are_not_predicted() {
        let (mut p, g) = confident();
        p.on_input(b"\x1b[A", &g); // cursor-mode arrow
        p.on_input(b"\x1bOB", &g); // application-mode arrow (SS3)
        p.on_input(b"\x1b[15~", &g); // F5
        p.on_input(b"\x1bb", &g); // alt+b, readline word-back
        assert!(p.pending.is_empty());
        assert_eq!(p.overlay(&g, 20, 2), "");
    }

    #[test]
    fn forwarded_mouse_report_is_not_predicted() {
        let (mut p, g) = confident();
        // remote foreground is a TUI, so pane.rs forwards SGR mouse bytes raw
        p.on_input(b"\x1b[<0;33;15M", &g); // click
        p.on_input(b"\x1b[<64;33;15M", &g); // wheel up
        p.on_input(b"\x1b[<65;33;15M", &g); // wheel down
        p.on_input(b"\x1b[<35;40;12M", &g); // motion
        assert!(p.pending.is_empty());
    }

    #[test]
    fn mouse_report_split_across_reads_is_not_predicted() {
        let (mut p, g) = confident();
        p.on_input(b"\x1b[<0;33", &g); // read boundary lands mid-sequence
        p.on_input(b";15M", &g);
        assert!(p.pending.is_empty());
        p.on_input(b"x", &g); // and real typing still predicts after
        assert_eq!(p.pending.len(), 1);
    }

    #[test]
    fn literally_typed_sequence_text_still_predicts() {
        let (mut p, g) = confident();
        // the same characters from the keyboard carry no ESC, so they are text
        p.on_input(b"[13;2u", &g);
        assert_eq!(p.pending.iter().map(|x| x.ch).collect::<String>(), "[13;2u");
        assert!(p.overlay(&g, 20, 2).contains('u'));
    }

    #[test]
    fn bare_escape_does_not_eat_the_next_chunk() {
        let (mut p, g) = confident();
        p.on_input(&[0x1b], &g); // Escape key in vim, arrives alone
        p.on_input(b"ls", &g);
        assert_eq!(p.pending.len(), 2);
    }

    #[test]
    fn timeout_wipes_ghosts() {
        let mut p = Predictor::new();
        let g = grid_at("Password: ", 10);
        p.streak = 5; // confident from earlier shell typing
        p.on_input(b"hunter2", &g);
        assert!(!p.overlay(&g, 20, 2).is_empty());
        p.age_all(Duration::from_millis(1500));
        assert!(p.deadline().is_some());
        p.on_tick();
        assert!(p.pending.is_empty());
        assert_eq!(p.streak, 0);
        assert!(p.take_dirty()); // forces repaint that erases the ghosts
    }
}
