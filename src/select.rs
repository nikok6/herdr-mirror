// Local drag-selection for mirror panes.
//
// herdr cannot do this for us. Its native selection only anchors when the pane
// has *no* mouse mode set (`AppState::handle_mouse` forwards the press and
// clears the selection otherwise), while the wheel only reaches a pane as a
// mouse report when at least one mode *is* set. Those are complementary
// conditions, so releasing the grab to get selection costs the wheel: it falls
// back to alternate scroll, which types one cursor key per notch into whatever
// is running (in an agent CLI, that walks the prompt history).
//
// So selection has to be ours. The streamer already receives every press, drag
// and release as SGR bytes while the grab is held, and it already owns the grid
// it painted, which means it has everything it needs: it can track the drag,
// highlight the span, and copy the text without herdr ever learning a selection
// happened. The wheel keeps its existing semantic-scroll path untouched.
//
// The part that makes this safe for *every* remote TUI, not just the ones that
// ignore the mouse, is that a press is deferred rather than stolen. Press and
// release carry different information and only the release says which gesture
// this was:
//
//     press    → buffer the raw bytes, start a tentative selection
//     drag     → the pointer left its cell, so this is a selection
//     release  → moved? copy it, the app sees nothing.
//                still? it was a click: replay press+release to the app.
//
// So lazygit keeps click-to-stage and htop keeps click-to-sort, because those
// are clicks and clicks still arrive. What they lose is the in-app *drag*
// gesture (vim's mouse visual-select, a TUI's pane-resize handle), which is
// claimed as a selection by definition, and the click now lands on release
// instead of on press. Deferring means there is no list of "apps that ignore
// the mouse" to keep, which is the whole reason this shape was chosen: such a
// list is unbounded, drifts, and fails silently when it is wrong.
//
// A shell foreground is still handled the old way (the grab is released and
// herdr selects natively), so none of this applies at a prompt.

use std::fmt::Write as _;

use crate::grid::{self, Grid};

/// A grid coordinate: (row, col), both 0-based, in grid space rather than
/// screen space. Grid space is the stable one: a frame that grows the content
/// scrolls the visible window, and a screen-space anchor would slide across the
/// text mid-drag.
pub type Pos = (usize, usize);

/// What a release turned out to mean.
#[derive(Debug, PartialEq, Eq)]
pub enum Released {
    /// the pointer covered ground: copy this span, the app sees nothing
    Selection((Pos, Pos)),
    /// the pointer never left its cell, so it was a click after all. These are
    /// the buffered press bytes followed by the release, to send on verbatim.
    Click(Vec<u8>),
    /// no press was pending (a stray release, e.g. the button went down before
    /// we had the grab). Forwarding a lone release to an app that never saw the
    /// press would be worse than dropping it.
    Nothing,
}

#[derive(Default)]
pub struct Select {
    span: Option<(Pos, Pos)>,
    dragging: bool,
    /// raw bytes of the press we are holding, replayed if this turns out to be
    /// a click rather than a drag
    pending_press: Vec<u8>,
    /// whether the pointer has left its starting cell since the press. Tracked
    /// separately from `span` because a drag that wanders and comes back is
    /// still a drag, not a click.
    moved: bool,
    /// set whenever the highlight changed, so the caller can invalidate the
    /// renderer's row cache — the overlay paints outside it, and without a
    /// repaint an old highlight survives under the new one
    dirty: bool,
}

impl Select {
    pub fn new() -> Select {
        Select::default()
    }

    /// Take the "highlight changed" flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Begin a tentative selection and hold the press bytes. Nothing is sent to
    /// the app yet: until the release we do not know whether this is a click.
    pub fn press(&mut self, at: Pos, raw: &[u8]) {
        self.dirty |= self.span.is_some();
        self.span = Some((at, at));
        self.dragging = true;
        self.moved = false;
        self.pending_press.clear();
        self.pending_press.extend_from_slice(raw);
    }

    pub fn drag(&mut self, at: Pos) {
        if let Some((_, cursor)) = &mut self.span {
            if *cursor != at {
                *cursor = at;
                self.moved = true;
                self.dirty = true;
            }
        }
    }

    /// Resolve the gesture. A pointer that never left its starting cell is a
    /// click, so the held press and this release go to the app untouched;
    /// anything that moved is a selection and the app sees neither.
    pub fn release(&mut self, at: Pos, raw: &[u8]) -> Released {
        self.drag(at);
        self.dragging = false;
        if self.span.is_none() {
            return Released::Nothing;
        }
        if !self.moved {
            let mut click = std::mem::take(&mut self.pending_press);
            click.extend_from_slice(raw);
            self.clear();
            return Released::Click(click);
        }
        let (anchor, cursor) = self.span.expect("checked above");
        self.pending_press.clear();
        if anchor == cursor {
            // dragged out and back to the starting cell: the user changed their
            // mind. Copying the one character under the pointer would be a
            // surprise, and replaying a click is wrong because this was a drag.
            self.clear();
            return Released::Nothing;
        }
        Released::Selection(ordered(anchor, cursor))
    }

    /// Peek at the repaint flag without consuming it.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Dismiss a finished highlight, e.g. because the user typed.
    ///
    /// Deliberately a no-op while the button is still down. `clear` would throw
    /// away the held press, and the release that follows would then find nothing
    /// pending and drop the click entirely — so a keystroke landing in the same
    /// stdin read as a press (typing while clicking, or a focus report coalesced
    /// with one) would silently eat the click.
    pub fn dismiss(&mut self) -> bool {
        if self.dragging {
            return false;
        }
        self.clear()
    }

    /// Drop the highlight and cancel any gesture in flight. Returns whether
    /// anything was showing.
    ///
    /// A press held at this point is discarded rather than replayed: the app
    /// never saw it go down, so delivering a click it did not ask for (because
    /// the foreground changed underneath, or the session restarted) would be
    /// inventing input.
    pub fn clear(&mut self) -> bool {
        self.dragging = false;
        self.moved = false;
        self.pending_press.clear();
        let had = self.span.take().is_some();
        self.dirty |= had;
        had
    }

    /// Map a 1-based SGR mouse coordinate onto the grid. Clamped into the
    /// painted content: a click in the blank gutter to the right of a narrow
    /// remote, or below its last row, should extend the selection to the edge
    /// rather than being ignored.
    pub fn locate(grid: &Grid, out_rows: usize, x: u32, y: u32) -> Pos {
        let offset = grid::window_offset(grid, out_rows);
        let row = (y.max(1) as usize - 1 + offset).min(grid.height.saturating_sub(1));
        let col = (x.max(1) as usize - 1).min(grid.width.saturating_sub(1));
        // clicking the right half of a wide char addresses the same glyph as
        // clicking its left half
        (row, grid.glyph_start(row, col))
    }

    /// Screen rows the highlight currently covers, as an inclusive range. The
    /// caller uses this to repaint exactly those rows rather than the whole
    /// pane when the selection moves.
    pub fn painted_rows(
        &self,
        grid: &Grid,
        out_rows: usize,
        reserved: usize,
    ) -> Option<(usize, usize)> {
        let (start, end) = ordered(self.span?.0, self.span?.1);
        if start == end {
            return None;
        }
        let offset = grid::window_offset(grid, out_rows);
        let last = out_rows.saturating_sub(reserved).checked_sub(1)?;
        let top = start.0.saturating_sub(offset).min(last);
        let bottom = end.0.checked_sub(offset)?.min(last);
        (start.0 >= offset || end.0 >= offset).then_some((top, bottom))
    }

    /// ANSI overlay drawing the highlight. Window math matches `Renderer::paint`
    /// and `Predictor::overlay` via `grid::window_offset`.
    ///
    /// `reserved` is the number of rows at the bottom the renderer has taken for
    /// its status line; painting into those would erase the hint.
    pub fn overlay(&self, grid: &Grid, out_cols: usize, out_rows: usize, reserved: usize) -> String {
        let Some((anchor, cursor)) = self.span else {
            return String::new();
        };
        let (start, end) = ordered(anchor, cursor);
        if start == end {
            return String::new();
        }
        let offset = grid::window_offset(grid, out_rows);
        let limit = out_cols.min(grid.width);
        let rows_avail = out_rows.saturating_sub(reserved);
        if limit == 0 || rows_avail == 0 {
            return String::new();
        }
        let mut out = String::new();
        for row in start.0..=end.0 {
            let Some(wr) = row.checked_sub(offset) else { continue };
            if wr >= rows_avail {
                break;
            }
            // a start column landing on the spacer half of a wide char would
            // paint a blank over its left half, so snap back to the glyph
            let from = if row == start.0 { grid.glyph_start(row, start.1) } else { 0 };
            let to = if row == end.0 { end.1 } else { limit - 1 };
            if from >= limit || from > to {
                continue;
            }
            let to = to.min(limit - 1);
            let _ = write!(out, "\x1b[{};{}H", wr + 1, from + 1);
            // Reverse video over the run, re-emitting each cell's OSC 8 link so
            // a highlighted URL stays clickable. The cell's own SGR is
            // deliberately not re-emitted: inverting on top of an existing
            // inverse would make the selected text look unselected.
            out.push_str("\x1b[7m");
            let mut prev_link: Option<&str> = None;
            let mut c = from;
            while c <= to {
                let ch = grid.ch_at(row, c);
                let w = grid::cw(ch);
                // Unlike the renderer's identical-looking guard, `to` is the
                // selection edge, not the screen edge: the cell past it is
                // visible and owned by someone else, so a wide char that would
                // straddle it is left unhighlighted rather than blanked.
                if w == 2 && c + 1 > to {
                    break;
                }
                let link = grid.link_at(row, c);
                if prev_link != link {
                    match link {
                        Some(uri) => {
                            let _ = write!(out, "\x1b]8;;{uri}\x1b\\");
                        }
                        None => out.push_str("\x1b]8;;\x1b\\"),
                    }
                    prev_link = link;
                }
                out.push(ch);
                c += w;
            }
            // never leave a link open: it would swallow whatever is painted next
            if prev_link.is_some() {
                out.push_str("\x1b]8;;\x1b\\");
            }
            out.push_str("\x1b[0m");
        }
        out
    }
}

fn ordered(a: Pos, b: Pos) -> (Pos, Pos) {
    if a <= b { (a, b) } else { (b, a) }
}

/// herdr rejects a clipboard write past this, so a selection bigger than it
/// would be dropped on the floor rather than truncated. A selection can only
/// ever be one screenful, so this is a guard, not a real limit.
const MAX_CLIPBOARD_BYTES: usize = 192 * 1024;

/// OSC 52 clipboard write: `ESC ] 52 ; c ; <base64> BEL`.
///
/// Written to the streamer's own stdout, which is the mirror pane, so herdr
/// picks it up as a clipboard write from the pane app and puts it wherever the
/// user's clipboard actually is. That last part is why this is OSC 52 and not a
/// `pbcopy` call: the streamer runs beside the herdr *server*, which is not
/// necessarily the machine the user is sitting at. When the client is attached
/// over ssh, herdr forwards the sequence on to the real terminal
/// (`selection::write_osc52_bytes`); when it is local, herdr writes the system
/// clipboard natively. Shelling out would get the ssh case silently wrong,
/// setting a clipboard on a machine nobody is looking at.
///
/// BEL rather than ST for the terminator, matching what herdr emits: some
/// terminals still only honour that form.
pub fn osc52(text: &str) -> Option<String> {
    use base64::Engine as _;
    // Whitespace-only, not just empty: a drag across N blank rows yields N-1
    // newlines, which is non-empty and would replace the user's clipboard with
    // nothing useful. Dragging through the empty space above an agent's prompt
    // is an easy way to do that by accident.
    if text.chars().all(char::is_whitespace) || text.len() > MAX_CLIPBOARD_BYTES {
        return None;
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    Some(format!("\x1b]52;c;{b64}\x07"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid_with(lines: &[&str], width: usize) -> Grid {
        let mut g = Grid::new();
        g.resize(width, lines.len());
        let mut ansi = String::new();
        for (r, line) in lines.iter().enumerate() {
            let _ = write!(ansi, "\x1b[{};1H{line}", r + 1);
        }
        g.apply(&ansi);
        g
    }

    const DOWN: &[u8] = b"\x1b[<0;3;2M";
    const UP: &[u8] = b"\x1b[<0;7;2m";

    #[test]
    fn a_drag_is_a_selection_and_the_app_sees_nothing() {
        let mut s = Select::new();
        s.press((1, 2), DOWN);
        s.drag((1, 6));
        assert_eq!(s.release((1, 6), UP), Released::Selection(((1, 2), (1, 6))));
    }

    #[test]
    fn a_click_is_replayed_to_the_app_press_then_release() {
        // the press was held back, so the app must receive both halves in order
        // and unaltered — this is what keeps click-to-stage working in lazygit
        let mut s = Select::new();
        s.press((3, 4), DOWN);
        let mut expected = DOWN.to_vec();
        expected.extend_from_slice(UP);
        assert_eq!(s.release((3, 4), UP), Released::Click(expected));
        assert!(s.span.is_none(), "a click must leave no highlight behind");
    }

    #[test]
    fn a_drag_that_wanders_back_cancels() {
        // returning to the starting cell must not become a click (the app would
        // get one the user never made) nor a one-character copy
        let mut s = Select::new();
        s.press((1, 2), DOWN);
        s.drag((1, 9));
        s.drag((1, 2));
        assert_eq!(s.release((1, 2), UP), Released::Nothing);
        assert!(s.span.is_none());
    }

    #[test]
    fn a_stray_release_is_dropped_not_forwarded() {
        // the button went down before we had the grab: the app never saw the
        // press, so handing it a lone release would be inventing input
        let mut s = Select::new();
        assert_eq!(s.release((1, 1), UP), Released::Nothing);
    }

    #[test]
    fn a_held_press_is_discarded_when_the_selection_is_cleared() {
        // typing, or a foreground change, cancels the gesture. The app never
        // saw the press, so it must not later receive a click for it.
        let mut s = Select::new();
        s.press((1, 2), DOWN);
        s.clear();
        assert_eq!(s.release((1, 2), UP), Released::Nothing);
    }

    #[test]
    fn a_backwards_drag_normalizes() {
        let mut s = Select::new();
        s.press((4, 8), DOWN);
        s.drag((2, 1));
        assert_eq!(s.release((2, 1), UP), Released::Selection(((2, 1), (4, 8))));
    }

    #[test]
    fn extraction_is_inclusive_and_trims_row_tails() {
        let g = grid_with(&["hello world", "second line"], 20);
        // within one row, both endpoints included
        assert_eq!(g.selection_text((0, 0), (0, 4)), "hello");
        // across rows: the first row runs to its content end, not to `width`
        assert_eq!(g.selection_text((0, 6), (1, 5)), "world\nsecond");
    }

    #[test]
    fn overlay_covers_the_span_and_nothing_else() {
        let g = grid_with(&["abcdef"], 6);
        let mut s = Select::new();
        s.press((0, 1), DOWN);
        s.release((0, 3), UP);
        let out = s.overlay(&g, 6, 1, 0);
        assert!(out.contains("\x1b[1;2H\x1b[7mbcd\x1b[0m"), "got: {out:?}");
        assert!(!out.contains('a') && !out.contains('e'), "highlight leaked: {out:?}");
    }

    #[test]
    fn overlay_follows_the_bottom_anchored_window() {
        // content taller than the pane: the window shows the last rows, so a
        // selection on grid row 9 must paint on screen row 3, not row 10
        let g = grid_with(&["", "", "", "", "", "", "", "", "", "target"], 10);
        let mut s = Select::new();
        s.press((9, 0), DOWN);
        s.release((9, 5), UP);
        let out = s.overlay(&g, 10, 3, 0);
        assert!(out.contains("\x1b[3;1H\x1b[7mtarget"), "got: {out:?}");
    }

    #[test]
    fn overlay_skips_rows_scrolled_out_of_view() {
        let g = grid_with(&["", "", "", "", "", "", "", "", "top", "bottom"], 10);
        let mut s = Select::new();
        s.press((0, 0), DOWN);
        s.release((9, 5), UP);
        let out = s.overlay(&g, 10, 2, 0);
        // only grid rows 8 and 9 are on screen
        assert!(out.contains("top"), "got: {out:?}");
        assert!(!out.contains("\x1b[0;"), "row 0 must not paint above the pane: {out:?}");
    }

    #[test]
    fn wide_chars_do_not_drift_the_highlight() {
        let g = grid_with(&["\u{d55c}\u{ae00}!"], 6);
        let mut s = Select::new();
        s.press((0, 0), DOWN);
        s.release((0, 4), UP);
        let out = s.overlay(&g, 6, 1, 0);
        // without width handling the blank spacer cells print too, pushing the
        // run two columns wide and painting over the text to the right
        assert!(out.contains("\u{d55c}\u{ae00}!"), "got: {out:?}");
    }

    #[test]
    fn osc52_wraps_base64_and_refuses_the_unsendable() {
        assert_eq!(osc52("hi").as_deref(), Some("\x1b]52;c;aGk=\x07"));
        // empty writes clear the clipboard in OSC 52, which a no-op selection
        // must never do
        assert_eq!(osc52(""), None);
        // and neither must a drag across blank rows, which yields bare newlines
        // — non-empty, but replacing the clipboard with them is pure loss
        assert_eq!(osc52("\n\n\n"), None);
        assert_eq!(osc52("   \n  "), None);
        // herdr drops an oversize write, so don't emit one
        assert_eq!(osc52(&"x".repeat(MAX_CLIPBOARD_BYTES + 1)), None);
    }

    #[test]
    fn a_keystroke_dismisses_a_finished_highlight_but_not_a_gesture_in_flight() {
        // finished: the highlight goes away, as it would in a local pane
        let mut s = Select::new();
        s.press((1, 2), DOWN);
        s.drag((1, 8));
        s.release((1, 8), UP);
        assert!(s.dismiss(), "a released selection should be dismissed");

        // in flight: the button is still down, so the held press must survive or
        // the release that follows finds nothing and the click is eaten
        let mut s = Select::new();
        s.press((1, 2), DOWN);
        assert!(!s.dismiss(), "must not cancel a press that is still down");
        let mut expected = DOWN.to_vec();
        expected.extend_from_slice(UP);
        assert_eq!(s.release((1, 2), UP), Released::Click(expected));
    }

    #[test]
    fn a_wide_char_at_the_selection_edge_is_left_alone_not_blanked() {
        // `to` is the selection edge, not the screen edge: the cell past it is
        // visible and belongs to someone else, so blanking a straddling wide
        // char would erase a glyph the user can see
        let g = grid_with(&["ab\u{d55c}cd"], 8);
        let mut s = Select::new();
        s.press((0, 0), DOWN);
        s.drag((0, 2)); // ends on the left half of 한
        let out = s.overlay(&g, 8, 1, 0);
        assert!(out.contains("ab"), "got: {out:?}");
        assert!(!out.contains("ab "), "wide char blanked at the edge: {out:?}");
    }

    #[test]
    fn clicking_the_right_half_of_a_wide_char_addresses_the_glyph() {
        let g = grid_with(&["\u{d55c}\u{ae00}"], 4);
        // column 1 is the spacer of 한, and must resolve to 한 itself
        assert_eq!(Select::locate(&g, 1, 2, 1), (0, 0));
        assert_eq!(Select::locate(&g, 1, 1, 1), (0, 0));
        // 글 starts at column 2
        assert_eq!(Select::locate(&g, 1, 3, 1), (0, 2));
    }

    #[test]
    fn the_overlay_keeps_a_hyperlink_clickable() {
        let mut g = Grid::new();
        g.resize(8, 1);
        g.apply("\x1b[1;1H\x1b]8;;https://e.com/x\x1b\\PR\x1b[1;3H\x1b]8;;\x1b\\ ok");
        let mut s = Select::new();
        s.press((0, 0), DOWN);
        s.drag((0, 4));
        let out = s.overlay(&g, 8, 1, 0);
        assert!(out.contains("\x1b]8;;https://e.com/x\x1b\\"), "link dropped: {out:?}");
        assert!(out.contains("\x1b]8;;\x1b\\"), "link never closed: {out:?}");
    }

    #[test]
    fn the_overlay_leaves_the_status_row_alone() {
        // the window is bottom-anchored, so a downward drag ends exactly where
        // the renderer puts its hint
        let g = grid_with(&["one", "two", "three"], 6);
        let mut s = Select::new();
        s.press((0, 0), DOWN);
        s.drag((2, 4));
        assert!(s.overlay(&g, 6, 3, 0).contains("\x1b[3;"), "row 3 should paint with no status");
        assert!(!s.overlay(&g, 6, 3, 1).contains("\x1b[3;"), "row 3 belongs to the status line");
    }

    #[test]
    fn painted_rows_reports_the_band_actually_drawn() {
        let g = grid_with(&["a", "b", "c", "d"], 4);
        let mut s = Select::new();
        s.press((1, 0), DOWN);
        s.drag((2, 0));
        assert_eq!(s.painted_rows(&g, 4, 0), Some((1, 2)));
        // a status row shrinks the band rather than letting it spill
        assert_eq!(s.painted_rows(&g, 4, 1), Some((1, 2)));
        // nothing to repaint when there is no span
        s.clear();
        assert_eq!(s.painted_rows(&g, 4, 0), None);
    }

    #[test]
    fn locate_clamps_into_the_grid() {
        let g = grid_with(&["ab", "cd"], 2);
        // past the right edge and below the last row
        assert_eq!(Select::locate(&g, 2, 99, 99), (1, 1));
        // 1-based input, 0-based output
        assert_eq!(Select::locate(&g, 2, 1, 1), (0, 0));
    }

    #[test]
    fn dirty_tracks_visible_changes() {
        let mut s = Select::new();
        s.press((0, 0), DOWN);
        s.take_dirty();
        s.drag((0, 0)); // no movement
        assert!(!s.take_dirty(), "an unmoved drag should not force a repaint");
        s.drag((0, 3));
        assert!(s.take_dirty());
        s.clear();
        assert!(s.take_dirty(), "clearing must repaint away the old highlight");
    }
}
