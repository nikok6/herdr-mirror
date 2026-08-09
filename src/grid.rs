// Cell grid + renderer for the pane wrapper.
//
// Frame wire format (verified against preview 2026-06-30): the frame ANSI is
// strictly per-cell — ESC[r;cH ESC[..m <char> — with a trailing cursor CUP and
// ?25h/l visibility. No scroll regions, no relative moves. So a cell grid plus
// this small parser is a complete decoder; no VT emulator needed.

use std::fmt::Write as _;
use std::rc::Rc;
use unicode_width::UnicodeWidthChar;

use crate::scroll::{scrollbar_thumb, ScrollInfo, ScrollbarThumb};

/// Terminal display width of a char (CJK/Hangul are 2 columns).
fn cw(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(1).max(1)
}

/// One cell in the decoded remote grid. Coordinates are zero-based and refer
/// to the full streamed grid, not the bottom-anchored local viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridPoint {
    pub row: usize,
    pub col: usize,
}

impl GridPoint {
    pub fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }
}

/// A linear terminal selection. Direction is preserved so callers can update
/// the moving endpoint without reordering it on every drag event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: GridPoint,
    pub cursor: GridPoint,
}

impl Selection {
    pub fn new(anchor: GridPoint) -> Self {
        Self {
            anchor,
            cursor: anchor,
        }
    }

    pub fn range(anchor: GridPoint, cursor: GridPoint) -> Self {
        Self { anchor, cursor }
    }

    pub fn set_cursor(&mut self, cursor: GridPoint) {
        self.cursor = cursor;
    }

    pub fn ordered(&self) -> (GridPoint, GridPoint) {
        if (self.anchor.row, self.anchor.col) <= (self.cursor.row, self.cursor.col) {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    pub fn contains(&self, point: GridPoint) -> bool {
        let (start, end) = self.ordered();
        if point.row < start.row || point.row > end.row {
            return false;
        }
        if start.row == end.row {
            point.col >= start.col && point.col <= end.col
        } else if point.row == start.row {
            point.col >= start.col
        } else if point.row == end.row {
            point.col <= end.col
        } else {
            true
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct Cell {
    /// Rc: runs of cells share one SGR allocation
    pub sgr: Rc<str>,
    pub ch: char,
}

#[derive(Default)]
pub struct Grid {
    pub rows: Vec<Vec<Option<Cell>>>,
    pub width: usize,
    pub height: usize,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub cursor_visible: bool,
    /// 0-based last row with non-blank content
    pub content_bottom: usize,
    /// reused per-frame decode buffer (frames arrive many times a second)
    scratch: Vec<char>,
}

impl Clone for Grid {
    fn clone(&self) -> Self {
        Self {
            rows: self.rows.clone(),
            width: self.width,
            height: self.height,
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
            cursor_visible: self.cursor_visible,
            content_bottom: self.content_bottom,
            // Scratch is only a reusable frame-decode allocation. A selection
            // snapshot never needs the previous frame's ANSI payload.
            scratch: Vec::new(),
        }
    }
}

impl Grid {
    pub fn new() -> Grid {
        Grid {
            cursor_visible: true,
            ..Default::default()
        }
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        if width == self.width && height == self.height {
            return;
        }
        self.width = width;
        self.height = height;
        self.clear();
    }

    pub fn clear(&mut self) {
        self.rows = vec![vec![None; self.width]; self.height];
        self.content_bottom = 0;
    }

    pub fn apply(&mut self, ansi: &str) {
        let mut chars = std::mem::take(&mut self.scratch);
        chars.clear();
        chars.extend(ansi.chars());
        let mut row = 0usize;
        let mut col = 0usize;
        let mut sgr: Rc<str> = Rc::from("");
        let mut i = 0usize;
        while i < chars.len() {
            if chars[i] == '\x1b' {
                if let Some((params, final_ch, len)) = parse_csi(&chars[i..]) {
                    match final_ch {
                        'H' => {
                            let mut it = params
                                .split(';')
                                .map(|n| n.parse::<usize>().unwrap_or(1).max(1));
                            row = it.next().unwrap_or(1) - 1;
                            col = it.next().unwrap_or(1) - 1;
                        }
                        'm' => {
                            sgr = Rc::from(chars[i..i + len].iter().collect::<String>());
                        }
                        'J' => self.clear(),
                        'h' | 'l' if params == "?25" => self.cursor_visible = final_ch == 'h',
                        _ => {}
                    }
                    i += len;
                    continue;
                }
                if let Some(len) = parse_osc(&chars[i..]) {
                    i += len;
                    continue;
                }
                i += 2; // two-byte escape (charset selection etc.)
                continue;
            }
            let ch = chars[i];
            if ch >= ' ' || ch == '\t' {
                let ch = if ch == '\t' { ' ' } else { ch };
                let w = cw(ch);
                if row < self.height && col < self.width {
                    self.rows[row][col] = Some(Cell {
                        sgr: sgr.clone(),
                        ch,
                    });
                    // wide char spans two columns: clear any stale cell in the
                    // spacer slot so delta frames cannot leave mixed glyphs
                    if w == 2 && col + 1 < self.width {
                        self.rows[row][col + 1] = None;
                    }
                }
                col += w;
            }
            i += 1;
        }
        self.scratch = chars;
        // the scan position after the last CUP is the cursor: the frame ends
        // with an explicit cursor CUP followed only by visibility toggles
        self.cursor_row = row;
        self.cursor_col = col;
        // recompute (not just grow): a delta frame can erase content with
        // spaces, and a stale bottom would anchor the window onto blank rows
        self.content_bottom = self
            .rows
            .iter()
            .rposition(|cells| {
                cells
                    .iter()
                    .any(|c| c.as_ref().is_some_and(|c| c.ch != ' '))
            })
            .unwrap_or(0);
    }

    pub fn text_lines(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|cells| {
                cells
                    .iter()
                    .map(|c| c.as_ref().map(|c| c.ch).unwrap_or(' '))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// First grid row shown in a local viewport of `out_rows` rows. Keep this
    /// as the single source of truth for both painting and mouse hit-testing:
    /// a streamed observe frame can be taller than the pane displaying it.
    pub fn viewport_row_offset(&self, out_rows: usize) -> usize {
        let bottom = self.content_bottom.max(self.cursor_row);
        (bottom + 1).saturating_sub(out_rows)
    }

    /// Map a zero-based local viewport cell onto the decoded grid. The second
    /// column occupied by a wide glyph maps back to that glyph's leading cell,
    /// so a drag starting on either half selects the same character.
    pub fn point_at_viewport(
        &self,
        viewport_col: usize,
        viewport_row: usize,
        out_cols: usize,
        out_rows: usize,
    ) -> Option<GridPoint> {
        if viewport_col >= out_cols.min(self.width) || viewport_row >= out_rows {
            return None;
        }
        let point = GridPoint::new(
            self.viewport_row_offset(out_rows)
                .checked_add(viewport_row)?,
            viewport_col,
        );
        self.normalize_point(point)
    }

    /// Map a drag endpoint onto the nearest visible grid cell. Terminal mouse
    /// protocols keep reporting while a held pointer moves beyond the pane;
    /// clamping makes that extend the selection to the viewport edge instead
    /// of silently copying only the last in-bounds point.
    pub fn point_at_viewport_clamped(
        &self,
        viewport_col: usize,
        viewport_row: usize,
        out_cols: usize,
        out_rows: usize,
    ) -> Option<GridPoint> {
        let offset = self.viewport_row_offset(out_rows);
        let visible_cols = out_cols.min(self.width);
        let visible_rows = out_rows.min(self.height.saturating_sub(offset));
        if visible_cols == 0 || visible_rows == 0 {
            return None;
        }
        self.point_at_viewport(
            viewport_col.min(visible_cols - 1),
            viewport_row.min(visible_rows - 1),
            out_cols,
            out_rows,
        )
    }

    /// Extract visible-grid text in reading order. Streamed frames do not
    /// expose soft-wrap metadata, so every crossed visual row is separated by
    /// a newline. Padding at the right edge of each row is omitted.
    pub fn selected_text(&self, selection: &Selection) -> Option<String> {
        let normalized = Selection::range(
            self.normalize_point(selection.anchor)?,
            self.normalize_point(selection.cursor)?,
        );
        let (start, end) = normalized.ordered();
        let mut lines = Vec::with_capacity(end.row - start.row + 1);

        for row in start.row..=end.row {
            let first_col = if row == start.row { start.col } else { 0 };
            let last_col = if row == end.row {
                end.col
            } else {
                self.width.saturating_sub(1)
            };
            let mut line = String::new();
            let mut col = first_col;
            while col <= last_col {
                match self.rows[row][col].as_ref() {
                    Some(cell) => {
                        line.push(cell.ch);
                        col = col.saturating_add(cw(cell.ch));
                    }
                    None if self.is_wide_spacer(row, col) => col += 1,
                    None => {
                        line.push(' ');
                        col += 1;
                    }
                }
            }
            lines.push(line.trim_end().to_string());
        }

        Some(lines.join("\n"))
    }

    /// Whether every character currently covered by `selection` still matches
    /// the grid captured when the gesture began. Frames can repaint while the
    /// user is dragging; copying newly painted text under an old highlight is
    /// a silent clipboard corruption, so callers cancel that gesture instead.
    pub fn selection_text_matches(&self, source: &Grid, selection: &Selection) -> bool {
        if (self.width, self.height) != (source.width, source.height) {
            return false;
        }
        let (start, end) = selection.ordered();
        if end.row >= self.height || end.col >= self.width {
            return false;
        }

        for row in start.row..=end.row {
            let first_col = if row == start.row { start.col } else { 0 };
            let last_col = if row == end.row {
                end.col
            } else {
                self.width.saturating_sub(1)
            };
            for col in first_col..=last_col {
                let current = self.rows[row][col]
                    .as_ref()
                    .map(|cell| cell.ch)
                    .unwrap_or(' ');
                let original = source.rows[row][col]
                    .as_ref()
                    .map(|cell| cell.ch)
                    .unwrap_or(' ');
                if current != original {
                    return false;
                }
            }
        }
        true
    }

    fn normalize_point(&self, point: GridPoint) -> Option<GridPoint> {
        if point.row >= self.height || point.col >= self.width {
            return None;
        }
        if self.is_wide_spacer(point.row, point.col) {
            Some(GridPoint::new(point.row, point.col - 1))
        } else {
            Some(point)
        }
    }

    fn is_wide_spacer(&self, row: usize, col: usize) -> bool {
        col > 0
            && self
                .rows
                .get(row)
                .and_then(|cells| cells.get(col))
                .is_some_and(Option::is_none)
            && self
                .rows
                .get(row)
                .and_then(|cells| cells.get(col - 1))
                .and_then(Option::as_ref)
                .is_some_and(|cell| cw(cell.ch) == 2)
    }
}

/// CSI (ECMA-48): ESC [ <params 0x30-0x3F> <intermediates 0x20-0x2F> <final
/// 0x40-0x7E>. Returns (params, final, char len).
///
/// Intermediates and the non-alphabetic finals matter even though no sequence
/// using them touches the grid: an unrecognized sequence is *skipped*, while an
/// unparsed one falls through to the two-byte ESC skip in `apply` and prints its
/// own tail as text. That is where `CSI 2 SP q` (DECSCUSR, set cursor style —
/// emitted by every TUI that picks a cursor shape) lands in the pane as a
/// literal `2 q`.
fn parse_csi(chars: &[char]) -> Option<(String, char, usize)> {
    if chars.len() < 3 || chars[0] != '\x1b' || chars[1] != '[' {
        return None;
    }
    let mut params = String::new();
    let mut in_intermediates = false;
    for (idx, &c) in chars.iter().enumerate().skip(2).take(62) {
        match c {
            // parameter bytes: digits and ;:?<=>, but only before intermediates
            '\u{30}'..='\u{3f}' if !in_intermediates => params.push(c),
            // intermediate bytes: space, !, ", #, $, …
            '\u{20}'..='\u{2f}' => in_intermediates = true,
            // final byte: alphabetic plus @[\]^_`{|}~
            '\u{40}'..='\u{7e}' => return Some((params, c, idx + 1)),
            _ => return None,
        }
    }
    None
}

/// OSC: ESC ] … (BEL | ESC \). Returns char len.
fn parse_osc(chars: &[char]) -> Option<usize> {
    if chars.len() < 2 || chars[0] != '\x1b' || chars[1] != ']' {
        return None;
    }
    let mut i = 2;
    while i < chars.len() {
        match chars[i] {
            '\x07' => return Some(i + 1),
            '\x1b' if chars.get(i + 1) == Some(&'\\') => return Some(i + 2),
            '\x1b' => return None,
            _ => i += 1,
        }
    }
    None
}

// ---------------------------------------------------------------------------
// renderer: paints a window of the grid onto the local terminal

#[derive(Default)]
pub struct Renderer {
    last_rows: Vec<Option<String>>,
    status_text: String,
}

fn scrollbar_glyph(thumb: Option<ScrollbarThumb>, row: usize) -> Option<&'static str> {
    let thumb = thumb?;
    if row >= thumb.top && row - thumb.top < thumb.len {
        // Match Herdr's focused-pane symbols. The host terminal supplies the
        // actual palette, so the track is dimmed instead of guessing Herdr's
        // configured overlay colors.
        Some("\x1b[0m▐")
    } else {
        Some("\x1b[0;2m▕")
    }
}

impl Renderer {
    pub fn new() -> Renderer {
        Renderer::default()
    }

    pub fn invalidate(&mut self) {
        self.last_rows.clear();
    }

    pub fn status(&mut self, text: &str) {
        self.status_text = text.to_string();
        self.last_rows.pop(); // force bottom row repaint
    }

    /// Build the ANSI to paint the grid without a selection overlay.
    #[cfg(test)]
    pub fn paint(&mut self, grid: &Grid, out_cols: usize, out_rows: usize) -> String {
        self.paint_with_selection_and_scrollbar(grid, out_cols, out_rows, None, None)
    }

    /// Build the ANSI to paint the grid into an out_cols × out_rows terminal.
    /// Bottom-anchored window: agent TUIs live at the bottom of the screen.
    #[allow(dead_code)] // compatibility entry point; pane mode now supplies scroll state
    pub fn paint_with_selection(
        &mut self,
        grid: &Grid,
        out_cols: usize,
        out_rows: usize,
        selection: Option<&Selection>,
    ) -> String {
        self.paint_with_selection_and_scrollbar(grid, out_cols, out_rows, selection, None)
    }

    /// Build the ANSI for the grid, selection, and source Herdr scrollbar.
    ///
    /// A source frame contains only the terminal grid; Herdr's native
    /// scrollbar occupies a separate column. Recreate it only when the local
    /// pane has a real spare column. Hiding the bar is preferable to silently
    /// replacing the source application's rightmost cell.
    pub fn paint_with_selection_and_scrollbar(
        &mut self,
        grid: &Grid,
        out_cols: usize,
        out_rows: usize,
        selection: Option<&Selection>,
        scroll: Option<ScrollInfo>,
    ) -> String {
        let scrollbar = scroll
            .filter(|metrics| metrics.max_offset_from_bottom > 0 && grid.width < out_cols)
            .and_then(|metrics| scrollbar_thumb(metrics, out_rows));
        let offset_r = grid.viewport_row_offset(out_rows);
        let mut out = String::from("\x1b[?2026h\x1b[?25l");
        // paint every local row (missing rows blank-fill), or the pane stays
        // blank before the first frame and the status row is unreachable
        let row_count = out_rows;
        if self.last_rows.len() < row_count {
            self.last_rows.resize(row_count, None);
        }
        for r in 0..row_count {
            let empty = Vec::new();
            let cells = grid.rows.get(r + offset_r).unwrap_or(&empty);
            let mut line = String::new();
            let mut prev_style: Option<(&str, bool)> = None;
            let limit = out_cols.min(grid.width);
            let mut c = 0usize;
            while c < limit {
                let cell = cells.get(c).and_then(|c| c.as_ref());
                let sgr = cell.map(|c| &*c.sgr).unwrap_or("\x1b[0m");
                let selected = selection
                    .is_some_and(|selection| selection.contains(GridPoint::new(r + offset_r, c)));
                if prev_style != Some((sgr, selected)) {
                    // Re-emitting the cell's original SGR is not enough to
                    // leave a selection: common color-only sequences such as
                    // `31m` do not disable reverse video. Explicitly clear the
                    // overlay before restoring the next cell's own style.
                    if prev_style.is_some_and(|(_, was_selected)| was_selected) && !selected {
                        line.push_str("\x1b[27m");
                    }
                    line.push_str(if sgr.is_empty() { "\x1b[0m" } else { sgr });
                    if selected {
                        line.push_str("\x1b[7m");
                    }
                    prev_style = Some((sgr, selected));
                }
                let ch = cell.map(|c| c.ch).unwrap_or(' ');
                let w = cw(ch);
                // a wide char that would straddle the right edge is blanked
                if w == 2 && c + 1 >= limit {
                    line.push(' ');
                    c += 1;
                    continue;
                }
                line.push(ch);
                // wide char occupies two columns: skip its spacer cell so the
                // painted columns stay aligned with the grid (Hangul/CJK fix)
                c += w;
            }
            let is_status_row = r == out_rows - 1 && !self.status_text.is_empty();
            let painted = if is_status_row {
                format!("\x1b[0;7m {} \x1b[0m\x1b[K", self.status_text)
            } else if limit >= out_cols {
                // A row painted to the full width leaves the cursor IN the last
                // column with the terminal's deferred-wrap flag set — it has not
                // moved to the next line yet. EL erases from the cursor, so it
                // would wipe the cell just written, costing one character on
                // every full-width row (i.e. every wrapped line). Nothing needs
                // clearing anyway: the loop paints all `limit` columns, blank
                // cells included.
                format!("{line}\x1b[0m")
            } else {
                format!("{line}\x1b[0m\x1b[K")
            };
            // The scrollbar spans the complete pane, including a status row.
            // Including its glyph in the cached row makes a moved or removed
            // bar repaint exactly the affected rows; the base row's EL clears
            // the old gutter cell when scroll metrics disappear.
            let painted = match scrollbar_glyph(scrollbar, r) {
                Some(glyph) if out_cols > 0 => {
                    format!("{painted}\x1b[{};{}H{glyph}\x1b[0m", r + 1, out_cols)
                }
                _ => painted,
            };
            if self.last_rows.get(r).map(|p| p.as_deref()) != Some(Some(painted.as_str())) {
                let _ = write!(out, "\x1b[{};1H", r + 1);
                out.push_str(&painted);
                self.last_rows[r] = Some(painted);
            }
        }
        let cr = grid.cursor_row as isize - offset_r as isize;
        if grid.cursor_visible && cr >= 0 && (cr as usize) < out_rows && self.status_text.is_empty()
        {
            // The decoded grid is the content boundary even when no thumb is
            // visible yet (the stable primary-screen gutter exists before the
            // first scrollback line) or scroll metadata is unavailable.
            let content_cols = out_cols.min(grid.width);
            if content_cols > 0 {
                let _ = write!(
                    out,
                    "\x1b[{};{}H\x1b[?25h",
                    cr + 1,
                    grid.cursor_col.min(content_cols - 1) + 1
                );
            }
        }
        out.push_str("\x1b[?2026l");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_per_cell_frame() {
        let mut g = Grid::new();
        g.resize(10, 4);
        g.apply("\x1b[1;1H\x1b[0mhi\x1b[3;2H\x1b[31mX\x1b[2;1H\x1b[?25h");
        assert_eq!(g.text_lines(), vec!["hi", "", " X", ""]);
        assert_eq!(g.content_bottom, 2);
        assert_eq!((g.cursor_row, g.cursor_col), (1, 0));
        assert!(g.cursor_visible);
        assert_eq!(&*g.rows[2][1].as_ref().unwrap().sgr, "\x1b[31m");
    }

    #[test]
    fn cursor_style_does_not_print_itself() {
        // DECSCUSR: ESC [ 2 SP q. The space is an intermediate byte, so a
        // params-only parser bails and `apply` prints the "2 q" tail into the
        // grid — the stray characters seen in mirrored agent prompts.
        let mut g = Grid::new();
        g.resize(10, 2);
        g.apply("\x1b[1;1H\x1b[2 qhi");
        assert_eq!(g.text_lines(), vec!["hi", ""]);
    }

    #[test]
    fn non_alphabetic_finals_are_consumed() {
        // @ and ~ are legal CSI finals; unparsed, they print their params too
        let mut g = Grid::new();
        g.resize(10, 2);
        g.apply("\x1b[1;1H\x1b[3~\x1b[2@ok");
        assert_eq!(g.text_lines(), vec!["ok", ""]);
    }

    #[test]
    fn csi_parsing_shape() {
        // params stop at the first intermediate; the final is reported
        assert_eq!(
            parse_csi(&"\x1b[2 q".chars().collect::<Vec<_>>()),
            Some(("2".into(), 'q', 5))
        );
        assert_eq!(
            parse_csi(&"\x1b[?25h".chars().collect::<Vec<_>>()),
            Some(("?25".into(), 'h', 6))
        );
        assert_eq!(
            parse_csi(&"\x1b[1;31m".chars().collect::<Vec<_>>()),
            Some(("1;31".into(), 'm', 7))
        );
        // C1 and other non-CSI bytes are still rejected
        assert_eq!(parse_csi(&"\x1b[1;\x07m".chars().collect::<Vec<_>>()), None);
    }

    #[test]
    fn clear_and_visibility() {
        let mut g = Grid::new();
        g.resize(4, 2);
        g.apply("\x1b[1;1Habcd\x1b[2;1Hwxyz");
        assert_eq!(g.content_bottom, 1);
        g.apply("\x1b[2J\x1b[?25l");
        assert_eq!(g.text_lines(), vec!["", ""]);
        assert!(!g.cursor_visible);
    }

    #[test]
    fn skips_osc_and_tabs() {
        let mut g = Grid::new();
        g.resize(8, 1);
        g.apply("\x1b]0;title\x07\x1b[1;1Ha\tb");
        assert_eq!(g.text_lines(), vec!["a b"]);
    }

    #[test]
    fn content_bottom_shrinks_when_delta_erases() {
        let mut g = Grid::new();
        g.resize(6, 8);
        g.apply("\x1b[1;1Htop\x1b[7;1Hbottom");
        assert_eq!(g.content_bottom, 6);
        // delta frame erases the bottom content with spaces
        g.apply("\x1b[7;1H      ");
        assert_eq!(g.content_bottom, 0);
    }

    #[test]
    fn wide_chars_do_not_drift() {
        let mut g = Grid::new();
        g.resize(10, 1);
        // the server skips the spacer cell after a wide char with a CUP jump
        g.apply("\x1b[1;1H한\x1b[1;3H글\x1b[1;5H!");
        assert_eq!((g.cursor_row, g.cursor_col), (0, 5));
        let mut r = Renderer::new();
        let out = r.paint(&g, 10, 1);
        // without width handling this painted "한 글 !" and drifted right
        assert!(out.contains("한글!"), "got: {out:?}");
    }

    #[test]
    fn wide_char_overwrites_stale_spacer() {
        let mut g = Grid::new();
        g.resize(10, 1);
        g.apply("\x1b[1;1Hab"); // narrow content first
        g.apply("\x1b[1;1H한"); // delta frame paints a wide char over it
        let mut r = Renderer::new();
        let out = r.paint(&g, 10, 1);
        assert!(!out.contains('b'), "stale spacer cell survived: {out:?}");
    }

    #[test]
    fn a_full_width_row_is_not_erased_by_its_own_el() {
        // EL after filling the last column erases the cell just written: the
        // cursor is still IN that column with the deferred-wrap flag set, so
        // "erase to end of line" starts there. Cost was one character on every
        // row long enough to wrap — a wrapped line lost the char at the break.
        let mut g = Grid::new();
        g.resize(5, 1);
        g.apply("\x1b[1;1Habcde");
        let mut r = Renderer::new();
        let out = r.paint(&g, 5, 1);
        assert!(out.contains("abcde"), "got: {out:?}");
        assert!(
            !out.contains("abcde\x1b[0m\x1b[K"),
            "EL would erase the 'e': {out:?}"
        );
    }

    #[test]
    fn a_short_row_still_clears_its_tail() {
        // the other half: when the remote is narrower than the local pane the
        // gutter must still be cleared, or stale content survives to the right
        let mut g = Grid::new();
        g.resize(3, 1);
        g.apply("\x1b[1;1Hab");
        let mut r = Renderer::new();
        let out = r.paint(&g, 10, 1);
        assert!(
            out.contains("\x1b[K"),
            "narrow grid must still emit EL: {out:?}"
        );
    }

    #[test]
    fn status_paints_on_empty_grid() {
        // before the first frame the grid is 0x0 — status must still render
        let g = Grid::new();
        let mut r = Renderer::new();
        r.status("reconnecting in 5s");
        let out = r.paint(&g, 80, 24);
        assert!(out.contains("reconnecting in 5s"));
    }

    #[test]
    fn renderer_bottom_anchors_and_status() {
        let mut g = Grid::new();
        g.resize(5, 10);
        g.apply("\x1b[10;1Hlast"); // content at the bottom row of a tall grid
        let mut r = Renderer::new();
        let out = r.paint(&g, 5, 3);
        // window shows rows 8..10 → "last" lands on the visible last row
        assert!(out.contains("last"));
        r.status("HELLO");
        let out2 = r.paint(&g, 5, 3);
        assert!(out2.contains("HELLO"));
        // unchanged rows are not repainted
        let out3 = r.paint(&g, 5, 3);
        assert!(!out3.contains("last"));
    }

    #[test]
    fn viewport_points_share_the_renderers_bottom_anchor() {
        let mut g = Grid::new();
        g.resize(5, 10);
        g.apply("\x1b[10;1Hlast");

        assert_eq!(g.viewport_row_offset(3), 7);
        assert_eq!(g.point_at_viewport(0, 0, 5, 3), Some(GridPoint::new(7, 0)));
        assert_eq!(g.point_at_viewport(3, 2, 5, 3), Some(GridPoint::new(9, 3)));
        assert_eq!(g.point_at_viewport(5, 2, 5, 3), None);
        assert_eq!(g.point_at_viewport(0, 3, 5, 3), None);
        assert_eq!(
            g.point_at_viewport_clamped(99, 99, 5, 3),
            Some(GridPoint::new(9, 4))
        );

        let mut renderer = Renderer::new();
        let selected = Selection::new(g.point_at_viewport(0, 2, 5, 3).unwrap());
        let out = renderer.paint_with_selection(&g, 5, 3, Some(&selected));
        assert!(
            out.contains("\x1b[7ml"),
            "the mapped bottom-row point must highlight the rendered bottom row: {out:?}"
        );
        assert_eq!(g.selected_text(&selected).as_deref(), Some("l"));
    }

    #[test]
    fn selection_orders_forward_and_reverse_ranges() {
        let forward = Selection::range(GridPoint::new(1, 3), GridPoint::new(3, 1));
        let reverse = Selection::range(GridPoint::new(3, 1), GridPoint::new(1, 3));
        assert_eq!(forward.ordered(), reverse.ordered());
        assert!(forward.contains(GridPoint::new(1, 3)));
        assert!(forward.contains(GridPoint::new(2, 0)));
        assert!(forward.contains(GridPoint::new(3, 1)));
        assert!(!forward.contains(GridPoint::new(1, 2)));
        assert!(!forward.contains(GridPoint::new(3, 2)));
    }

    #[test]
    fn selected_text_handles_forward_reverse_and_multiple_rows() {
        let mut g = Grid::new();
        g.resize(8, 3);
        g.apply("\x1b[1;1Halpha\x1b[2;1Hbravo\x1b[3;1Hcharlie");

        let forward = Selection::range(GridPoint::new(0, 2), GridPoint::new(2, 2));
        let reverse = Selection::range(GridPoint::new(2, 2), GridPoint::new(0, 2));
        assert_eq!(
            g.selected_text(&forward).as_deref(),
            Some("pha\nbravo\ncha")
        );
        assert_eq!(g.selected_text(&reverse), g.selected_text(&forward));

        let one_line = Selection::range(GridPoint::new(1, 1), GridPoint::new(1, 3));
        assert_eq!(g.selected_text(&one_line).as_deref(), Some("rav"));
    }

    #[test]
    fn wide_glyph_spacer_maps_and_copies_as_one_character() {
        let mut g = Grid::new();
        g.resize(6, 1);
        g.apply("\x1b[1;1HA한B");

        let wide_lead = GridPoint::new(0, 1);
        assert_eq!(g.point_at_viewport(1, 0, 6, 1), Some(wide_lead));
        assert_eq!(g.point_at_viewport(2, 0, 6, 1), Some(wide_lead));

        let from_spacer = Selection::range(GridPoint::new(0, 2), GridPoint::new(0, 3));
        assert_eq!(g.selected_text(&from_spacer).as_deref(), Some("한B"));
        let spacer_only = Selection::range(GridPoint::new(0, 2), GridPoint::new(0, 2));
        assert_eq!(g.selected_text(&spacer_only).as_deref(), Some("한"));
    }

    #[test]
    fn selected_text_rejects_stale_points_after_a_resize() {
        let mut g = Grid::new();
        g.resize(4, 2);
        g.apply("\x1b[1;1Htext");
        let stale = Selection::range(GridPoint::new(0, 0), GridPoint::new(1, 3));
        g.resize(2, 1);
        assert_eq!(g.selected_text(&stale), None);
    }

    #[test]
    fn selection_match_ignores_unrelated_repaints_but_detects_text_changes() {
        let mut source = Grid::new();
        source.resize(8, 2);
        source.apply("\x1b[1;1Halpha\x1b[2;1Hstatus");
        let selection = Selection::range(GridPoint::new(0, 0), GridPoint::new(0, 4));

        let mut current = source.clone();
        current.apply("\x1b[2;1H\x1b[31mSTATUS");
        assert!(current.selection_text_matches(&source, &selection));

        current.apply("\x1b[1;3HX");
        assert!(!current.selection_text_matches(&source, &selection));
    }

    #[test]
    fn renderer_highlights_only_the_selected_cells_and_restores_style() {
        let mut g = Grid::new();
        g.resize(4, 1);
        g.apply("\x1b[1;1H\x1b[31mabcd");
        let selection = Selection::range(GridPoint::new(0, 1), GridPoint::new(0, 2));
        let mut renderer = Renderer::new();

        let out = renderer.paint_with_selection(&g, 4, 1, Some(&selection));
        assert!(
            out.contains("a\x1b[31m\x1b[7mbc\x1b[27m\x1b[31md"),
            "selection must layer reverse video over, then restore, the cell SGR: {out:?}"
        );

        // The selection is part of the row cache key: an unchanged overlay is
        // not repainted, while moving it repaints the affected row.
        let unchanged = renderer.paint_with_selection(&g, 4, 1, Some(&selection));
        assert!(!unchanged.contains("abcd"));
        let moved = Selection::range(GridPoint::new(0, 2), GridPoint::new(0, 3));
        let moved_out = renderer.paint_with_selection(&g, 4, 1, Some(&moved));
        assert!(moved_out.contains("\x1b[7mcd"), "got: {moved_out:?}");

        let cleared = renderer.paint_with_selection(&g, 4, 1, None);
        assert!(
            cleared.contains("abcd"),
            "clearing selection must repaint: {cleared:?}"
        );
        assert!(!cleared.contains("\x1b[7m"));
    }

    #[test]
    fn renderer_highlights_a_wide_glyph_without_painting_its_spacer() {
        let mut g = Grid::new();
        g.resize(4, 1);
        g.apply("\x1b[1;1HA한B");
        let selection = Selection::range(GridPoint::new(0, 1), GridPoint::new(0, 1));
        let mut renderer = Renderer::new();
        let out = renderer.paint_with_selection(&g, 4, 1, Some(&selection));
        assert!(out.contains("\x1b[7m한"), "got: {out:?}");
        assert!(
            !out.contains("한 B"),
            "wide spacer was painted as text: {out:?}"
        );
    }

    fn scroll_info(offset_from_bottom: u64) -> ScrollInfo {
        ScrollInfo {
            offset_from_bottom,
            max_offset_from_bottom: 8,
            viewport_rows: 2,
        }
    }

    #[test]
    fn renderer_combines_selection_and_scrollbar() {
        let mut grid = Grid::new();
        grid.resize(4, 2);
        grid.apply("\x1b[1;1H\x1b[31mabcd");
        let selection = Selection::range(GridPoint::new(0, 1), GridPoint::new(0, 2));
        let mut renderer = Renderer::new();

        let out = renderer.paint_with_selection_and_scrollbar(
            &grid,
            5,
            2,
            Some(&selection),
            Some(scroll_info(8)),
        );

        assert!(out.contains("\x1b[7mbc"), "selection was lost: {out:?}");
        assert!(
            out.contains("\x1b[1;5H\x1b[0m▐\x1b[0m"),
            "scrollbar thumb was not painted in the spare column: {out:?}"
        );
    }

    #[test]
    fn scrollbar_spans_every_row_including_status() {
        let mut grid = Grid::new();
        grid.resize(4, 3);
        let mut renderer = Renderer::new();
        renderer.status("syncing");

        let out =
            renderer.paint_with_selection_and_scrollbar(&grid, 5, 3, None, Some(scroll_info(8)));

        assert!(out.contains("\x1b[1;5H\x1b[0m▐"), "top thumb: {out:?}");
        assert!(out.contains("\x1b[2;5H\x1b[0;2m▕"), "middle track: {out:?}");
        assert!(
            out.contains("\x1b[3;5H\x1b[0;2m▕"),
            "status-row track: {out:?}"
        );
        assert!(out.contains("syncing"), "status text was lost: {out:?}");
    }

    #[test]
    fn scrollbar_never_overwrites_full_width_source_content() {
        let mut grid = Grid::new();
        grid.resize(5, 1);
        grid.apply("\x1b[1;1Habcde");
        let mut renderer = Renderer::new();

        let out =
            renderer.paint_with_selection_and_scrollbar(&grid, 5, 1, None, Some(scroll_info(8)));

        assert!(out.contains("abcde"), "last source cell was lost: {out:?}");
        assert!(
            !out.contains('▐'),
            "thumb overwrote source content: {out:?}"
        );
        assert!(
            !out.contains('▕'),
            "track overwrote source content: {out:?}"
        );
    }

    #[test]
    fn scrollbar_removal_clears_every_stale_gutter_cell() {
        let mut grid = Grid::new();
        grid.resize(4, 2);
        let mut renderer = Renderer::new();
        renderer.paint_with_selection_and_scrollbar(&grid, 5, 2, None, Some(scroll_info(8)));

        let cleared = renderer.paint_with_selection_and_scrollbar(&grid, 5, 2, None, None);
        assert!(!cleared.contains('▐'), "stale thumb survived: {cleared:?}");
        assert!(!cleared.contains('▕'), "stale track survived: {cleared:?}");
        assert!(
            cleared.contains("\x1b[1;1H") && cleared.contains("\x1b[2;1H"),
            "rows containing the old bar were not repainted: {cleared:?}"
        );
        assert!(
            cleared.matches("\x1b[K").count() >= 2,
            "row clears did not erase the old gutter: {cleared:?}"
        );

        let unchanged = renderer.paint_with_selection_and_scrollbar(&grid, 5, 2, None, None);
        assert!(
            !unchanged.contains("\x1b[K") && !unchanged.contains('▐') && !unchanged.contains('▕'),
            "cleared rows were not cached: {unchanged:?}"
        );
    }

    #[test]
    fn visible_cursor_stays_out_of_scrollbar_gutter() {
        let mut grid = Grid::new();
        grid.resize(4, 2);
        // After painting the last source cell, the decoded cursor is one cell
        // beyond the grid. It must clamp to source column four, not the gutter.
        grid.apply("\x1b[1;4Hx\x1b[?25h");
        let mut renderer = Renderer::new();

        let out =
            renderer.paint_with_selection_and_scrollbar(&grid, 5, 2, None, Some(scroll_info(8)));

        assert!(
            out.ends_with("\x1b[1;4H\x1b[?25h\x1b[?2026l"),
            "cursor entered the fifth-column gutter: {out:?}"
        );

        let without_history = renderer.paint_with_selection_and_scrollbar(
            &grid,
            5,
            2,
            None,
            Some(ScrollInfo {
                max_offset_from_bottom: 0,
                ..scroll_info(0)
            }),
        );
        assert!(
            without_history.ends_with("\x1b[1;4H\x1b[?25h\x1b[?2026l"),
            "cursor entered the stable empty gutter: {without_history:?}"
        );
    }
}
