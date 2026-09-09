// Local drag-select for mirror panes.
//
// While the remote foreground is a TUI the wrapper holds the local mouse grab
// (?1002h) so clicks and wheel can be forwarded. herdr therefore hands the
// whole drag to this pane instead of starting its own selection. Forwarding
// the drag is a dead end: the remote app may highlight on its side, but its
// OSC 52 copy is consumed by the remote herdr (frames carry cells only), so the
// local clipboard never changes.
//
// So the drag is owned here. A left press is held back; if motion follows, the
// gesture becomes a selection over the decoded grid and the release copies its
// text through OSC 52 to the hosting terminal (the same path herdr's own copy
// uses from a local pane). A press released in place is a click and is
// forwarded as one gesture, so TUI clicks keep working.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use crate::grid::{cw, Grid};

/// Two left presses on the same cell within this window are a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub row: usize,
    pub col: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SelAction {
    /// not a plain left-button event: leave it to the normal routing
    Pass,
    /// swallowed (selection bookkeeping only)
    Consumed,
    /// a press released in place: the held press plus its release, to forward
    /// as one click gesture
    Click(Vec<u8>),
    /// a drag ended or a token was double-clicked: copy this text
    Copy(String),
}

/// SGR button codes: plain left press/release, and left-held motion (bit 32).
const LEFT: u32 = 0;
const LEFT_MOTION: u32 = 32;

#[derive(Default)]
pub struct Selection {
    /// a left press whose meaning (click or drag) is not known yet
    pending_press: Option<(Pos, Vec<u8>)>,
    anchor: Option<Pos>,
    head: Option<Pos>,
    dragging: bool,
    dirty: bool,
    /// the previous left press, to recognize a double-click
    last_press: Option<(Pos, Instant)>,
    /// a double-click already acted; its release carries nothing
    swallow_release: bool,
}

impl Selection {
    pub fn new() -> Selection {
        Selection::default()
    }

    /// Feed one SGR mouse event in local pane coordinates (1-based, as
    /// reported). `out_rows` is the local pane height, for the same
    /// bottom-anchored window math the renderer uses.
    pub fn on_mouse(
        &mut self,
        btn: u32,
        x: u32,
        y: u32,
        press: bool,
        raw: &[u8],
        grid: &Grid,
        out_rows: usize,
        now: Instant,
    ) -> SelAction {
        let pos = Pos {
            row: (y.max(1) as usize - 1) + grid.window_offset(out_rows),
            col: x.max(1) as usize - 1,
        };
        match (btn, press) {
            (LEFT, true) => {
                let double = self
                    .last_press
                    .is_some_and(|(p, at)| p == pos && now.duration_since(at) <= DOUBLE_CLICK);
                self.clear();
                self.last_press = Some((pos, now));
                if double {
                    // herdr copies a double-clicked token; do the same over the grid
                    if let Some((c0, c1)) = token_span(grid, pos) {
                        self.anchor = Some(Pos { row: pos.row, col: c0 });
                        self.head = Some(Pos { row: pos.row, col: c1 });
                        self.dirty = true;
                        self.swallow_release = true;
                        self.last_press = None;
                        return SelAction::Copy(self.text(grid));
                    }
                }
                self.pending_press = Some((pos, raw.to_vec()));
                SelAction::Consumed
            }
            (LEFT_MOTION, true) => {
                if let Some((start, _)) = &self.pending_press {
                    if *start != pos {
                        let start = *start;
                        self.pending_press = None;
                        self.anchor = Some(start);
                        self.head = Some(pos);
                        self.dragging = true;
                        self.dirty = true;
                    }
                    // else: jitter inside the pressed cell, still a click so far
                    SelAction::Consumed
                } else if self.dragging {
                    if self.head != Some(pos) {
                        self.head = Some(pos);
                        self.dirty = true;
                    }
                    SelAction::Consumed
                } else {
                    SelAction::Pass
                }
            }
            (LEFT, false) => {
                if std::mem::take(&mut self.swallow_release) {
                    return SelAction::Consumed;
                }
                if let Some((_, mut bytes)) = self.pending_press.take() {
                    bytes.extend_from_slice(raw);
                    return SelAction::Click(bytes);
                }
                if self.dragging {
                    self.dragging = false;
                    if self.head != Some(pos) {
                        self.head = Some(pos);
                        self.dirty = true;
                    }
                    return SelAction::Copy(self.text(grid));
                }
                SelAction::Pass
            }
            _ => SelAction::Pass,
        }
    }

    /// Drop the held press and any retained highlight.
    pub fn clear(&mut self) {
        if self.anchor.is_some() {
            self.dirty = true;
        }
        self.pending_press = None;
        self.anchor = None;
        self.head = None;
        self.dragging = false;
        self.swallow_release = false;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Take the "needs renderer invalidation" flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Selection bounds in grid coordinates, row-major ordered.
    fn bounds(&self) -> Option<(Pos, Pos)> {
        let (a, h) = (self.anchor?, self.head?);
        Some(if a <= h { (a, h) } else { (h, a) })
    }

    /// Row-major span of `row` inside the selection: inclusive column bounds.
    fn span(&self, row: usize, width: usize, a: Pos, b: Pos) -> Option<(usize, usize)> {
        if row < a.row || row > b.row || width == 0 {
            return None;
        }
        let c0 = if row == a.row { a.col } else { 0 };
        let c1 = if row == b.row { b.col } else { width - 1 };
        let c1 = c1.min(width - 1);
        (c0 <= c1).then_some((c0, c1))
    }

    /// The selected text: rows joined by newlines, trailing blanks trimmed.
    pub fn text(&self, grid: &Grid) -> String {
        let Some((a, b)) = self.bounds() else { return String::new() };
        let mut lines: Vec<String> = Vec::new();
        for row in a.row..=b.row.min(grid.height.saturating_sub(1)) {
            let Some((c0, c1)) = self.span(row, grid.width, a, b) else { continue };
            let cells = &grid.rows[row];
            let mut line = String::new();
            let mut c = c0;
            while c <= c1 {
                match cells.get(c).and_then(|c| c.as_ref()) {
                    Some(cell) => {
                        line.push(cell.ch);
                        c += cw(cell.ch);
                    }
                    None => {
                        line.push(' ');
                        c += 1;
                    }
                }
            }
            lines.push(line.trim_end().to_string());
        }
        lines.join("\n")
    }

    /// ANSI overlay that paints the selection in reverse video. Window math
    /// mirrors Renderer::paint (bottom-anchored).
    pub fn overlay(&self, grid: &Grid, out_cols: usize, out_rows: usize) -> String {
        let Some((a, b)) = self.bounds() else { return String::new() };
        let offset_r = grid.window_offset(out_rows);
        let width = grid.width.min(out_cols);
        let mut out = String::new();
        for row in a.row..=b.row {
            if row < offset_r || row >= grid.height {
                continue;
            }
            let wr = row - offset_r;
            if wr >= out_rows {
                break;
            }
            let Some((c0, c1)) = self.span(row, width, a, b) else { continue };
            let cells = &grid.rows[row];
            let mut s = String::new();
            let mut c = c0;
            while c <= c1 {
                match cells.get(c).and_then(|c| c.as_ref()) {
                    Some(cell) => {
                        // a wide char that would straddle the edge is blanked,
                        // as the renderer does
                        if cw(cell.ch) == 2 && c + 1 > c1 {
                            s.push(' ');
                            c += 1;
                        } else {
                            s.push(cell.ch);
                            c += cw(cell.ch);
                        }
                    }
                    None => {
                        s.push(' ');
                        c += 1;
                    }
                }
            }
            let _ = write!(out, "\x1b[{};{}H\x1b[0;7m{}\x1b[0m", wr + 1, c0 + 1, s);
        }
        if out.is_empty() {
            return out;
        }
        // the renderer parked the cursor before this overlay ran; put it back
        let cr = grid.cursor_row as isize - offset_r as isize;
        if grid.cursor_visible && cr >= 0 && (cr as usize) < out_rows {
            let _ = write!(out, "\x1b[{};{}H", cr + 1, grid.cursor_col.min(out_cols.saturating_sub(1)) + 1);
        }
        out
    }
}

/// Inclusive column span of the whitespace-delimited token under `pos`, if any.
fn token_span(grid: &Grid, pos: Pos) -> Option<(usize, usize)> {
    let cells = grid.rows.get(pos.row)?;
    let ch_at = |c: usize| cells.get(c).and_then(|c| c.as_ref()).map(|c| c.ch);
    // the spacer after a wide char is None; treat it as part of that char
    let is_token = |c: usize| match ch_at(c) {
        Some(ch) => !ch.is_whitespace(),
        None => c > 0 && ch_at(c - 1).is_some_and(|p| cw(p) == 2),
    };
    if pos.col >= grid.width || !is_token(pos.col) {
        return None;
    }
    let mut c0 = pos.col;
    while c0 > 0 && is_token(c0 - 1) {
        c0 -= 1;
    }
    let mut c1 = pos.col;
    while c1 + 1 < grid.width && is_token(c1 + 1) {
        c1 += 1;
    }
    Some((c0, c1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(lines: &[&str]) -> Grid {
        let width = lines.iter().map(|l| l.chars().map(cw).sum::<usize>()).max().unwrap_or(1).max(1);
        let mut g = Grid::new();
        g.resize(width, lines.len());
        let mut ansi = String::new();
        for (r, l) in lines.iter().enumerate() {
            ansi.push_str(&format!("\x1b[{};1H{}", r + 1, l));
        }
        g.apply(&ansi);
        g
    }

    fn sgr(btn: u32, x: u32, y: u32, press: bool) -> Vec<u8> {
        format!("\x1b[<{btn};{x};{y}{}", if press { 'M' } else { 'm' }).into_bytes()
    }

    fn feed(s: &mut Selection, g: &Grid, rows: usize, btn: u32, x: u32, y: u32, press: bool) -> SelAction {
        feed_at(s, g, rows, btn, x, y, press, Instant::now())
    }

    fn feed_at(s: &mut Selection, g: &Grid, rows: usize, btn: u32, x: u32, y: u32, press: bool, now: Instant) -> SelAction {
        let raw = sgr(btn, x, y, press);
        s.on_mouse(btn, x, y, press, &raw, g, rows, now)
    }

    #[test]
    fn double_click_copies_the_token_and_swallows_its_release() {
        let g = grid(&["path/to/file.rs:42 next"]);
        let mut s = Selection::new();
        let t0 = Instant::now();
        assert_eq!(feed_at(&mut s, &g, 1, 0, 5, 1, true, t0), SelAction::Consumed);
        assert!(matches!(feed_at(&mut s, &g, 1, 0, 5, 1, false, t0), SelAction::Click(_)));
        let t1 = t0 + Duration::from_millis(200);
        assert_eq!(feed_at(&mut s, &g, 1, 0, 5, 1, true, t1), SelAction::Copy("path/to/file.rs:42".into()));
        assert_eq!(feed_at(&mut s, &g, 1, 0, 5, 1, false, t1), SelAction::Consumed);
        assert!(s.overlay(&g, 30, 1).contains("\x1b[0;7mpath/to/file.rs:42\x1b[0m"));
        // a third press after the window is an ordinary click again
        let t2 = t1 + Duration::from_millis(900);
        assert_eq!(feed_at(&mut s, &g, 1, 0, 5, 1, true, t2), SelAction::Consumed);
        assert!(matches!(feed_at(&mut s, &g, 1, 0, 5, 1, false, t2), SelAction::Click(_)));
    }

    #[test]
    fn double_click_on_blank_is_a_plain_click() {
        let g = grid(&["ab   cd"]);
        let mut s = Selection::new();
        let t0 = Instant::now();
        feed_at(&mut s, &g, 1, 0, 4, 1, true, t0);
        feed_at(&mut s, &g, 1, 0, 4, 1, false, t0);
        assert_eq!(feed_at(&mut s, &g, 1, 0, 4, 1, true, t0 + Duration::from_millis(100)), SelAction::Consumed);
        assert!(matches!(feed_at(&mut s, &g, 1, 0, 4, 1, false, t0 + Duration::from_millis(100)), SelAction::Click(_)));
    }

    #[test]
    fn press_released_in_place_is_one_forwarded_click() {
        let g = grid(&["hello world"]);
        let mut s = Selection::new();
        assert_eq!(feed(&mut s, &g, 1, 0, 3, 1, true), SelAction::Consumed);
        // jitter inside the same cell keeps it a click
        assert_eq!(feed(&mut s, &g, 1, 32, 3, 1, true), SelAction::Consumed);
        let mut expect = sgr(0, 3, 1, true);
        expect.extend(sgr(0, 3, 1, false));
        assert_eq!(feed(&mut s, &g, 1, 0, 3, 1, false), SelAction::Click(expect));
        assert!(s.overlay(&g, 20, 1).is_empty());
    }

    #[test]
    fn drag_copies_the_grid_text_and_keeps_the_highlight() {
        let g = grid(&["hello world", "second line", "third"]);
        let mut s = Selection::new();
        feed(&mut s, &g, 3, 0, 7, 1, true);
        assert_eq!(feed(&mut s, &g, 3, 32, 9, 1, true), SelAction::Consumed);
        assert!(s.take_dirty());
        assert_eq!(feed(&mut s, &g, 3, 32, 3, 2, true), SelAction::Consumed);
        assert_eq!(feed(&mut s, &g, 3, 0, 3, 2, false), SelAction::Copy("world\nsec".into()));
        let ov = s.overlay(&g, 20, 3);
        assert!(ov.contains("\x1b[1;7H\x1b[0;7mworld\x1b[0m"), "{ov:?}");
        assert!(ov.contains("\x1b[2;1H\x1b[0;7msec\x1b[0m"), "{ov:?}");
        // a keystroke clears the retained highlight
        s.clear();
        assert!(s.take_dirty());
        assert!(s.overlay(&g, 20, 3).is_empty());
    }

    #[test]
    fn backwards_drag_orders_the_range() {
        let g = grid(&["abc", "def"]);
        let mut s = Selection::new();
        feed(&mut s, &g, 2, 0, 2, 2, true);
        feed(&mut s, &g, 2, 32, 2, 1, true);
        assert_eq!(feed(&mut s, &g, 2, 0, 2, 1, false), SelAction::Copy("bc\nde".into()));
    }

    #[test]
    fn coordinates_follow_the_bottom_anchored_window() {
        // grid is 4 rows, the local pane shows the bottom 2: local row 1 is grid row 2
        let g = grid(&["r0", "r1", "r2", "r3"]);
        let mut s = Selection::new();
        feed(&mut s, &g, 2, 0, 1, 1, true);
        feed(&mut s, &g, 2, 32, 2, 1, true);
        assert_eq!(feed(&mut s, &g, 2, 0, 2, 1, false), SelAction::Copy("r2".into()));
        let ov = s.overlay(&g, 10, 2);
        assert!(ov.starts_with("\x1b[1;1H\x1b[0;7mr2\x1b[0m"), "{ov:?}");
    }

    #[test]
    fn wide_chars_copy_once() {
        let g = grid(&["日本 ok"]);
        let mut s = Selection::new();
        feed(&mut s, &g, 1, 0, 1, 1, true);
        feed(&mut s, &g, 1, 32, 7, 1, true);
        assert_eq!(feed(&mut s, &g, 1, 0, 7, 1, false), SelAction::Copy("日本 ok".into()));
    }

    #[test]
    fn other_buttons_pass_through() {
        let g = grid(&["abc"]);
        let mut s = Selection::new();
        assert_eq!(feed(&mut s, &g, 1, 64, 1, 1, true), SelAction::Pass); // wheel
        assert_eq!(feed(&mut s, &g, 1, 2, 1, 1, true), SelAction::Pass); // right
        assert_eq!(feed(&mut s, &g, 1, 16, 1, 1, true), SelAction::Pass); // ctrl+left
        assert_eq!(feed(&mut s, &g, 1, 32, 1, 1, true), SelAction::Pass); // stray motion
        assert_eq!(feed(&mut s, &g, 1, 0, 1, 1, false), SelAction::Pass); // stray release
    }
}
