// Cell grid + renderer for the pane wrapper.
//
// Frame wire format (verified against preview 2026-06-30): the frame ANSI is
// strictly per-cell — ESC[r;cH ESC[..m <char> — with a trailing cursor CUP and
// ?25h/l visibility. No scroll regions, no relative moves. So a cell grid plus
// this small parser is a complete decoder; no VT emulator needed.

use std::fmt::Write as _;
use std::rc::Rc;
use unicode_width::UnicodeWidthChar;

/// Terminal display width of a char (CJK/Hangul are 2 columns).
fn cw(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(1).max(1)
}

#[derive(Clone, PartialEq)]
pub struct Cell {
    /// Rc: runs of cells share one SGR allocation
    pub sgr: Rc<str>,
    /// OSC 8 target URI, when this cell sits inside a hyperlink. Carried
    /// per-cell for the same reason as `sgr`: the renderer repaints arbitrary
    /// rows in isolation, so it cannot rely on stream order to know the state.
    pub link: Option<Rc<str>>,
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

impl Grid {
    pub fn new() -> Grid {
        Grid { cursor_visible: true, ..Default::default() }
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
        let mut link: Option<Rc<str>> = None;
        let mut i = 0usize;
        while i < chars.len() {
            if chars[i] == '\x1b' {
                if let Some((params, final_ch, len)) = parse_csi(&chars[i..]) {
                    match final_ch {
                        'H' => {
                            let mut it = params.split(';').map(|n| n.parse::<usize>().unwrap_or(1).max(1));
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
                    // OSC is presentation-free apart from OSC 8, which carries
                    // the hyperlink a click needs; everything else still drops.
                    if let Some(uri) = parse_osc8(&chars[i..i + len]) {
                        link = uri;
                    }
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
                    self.rows[row][col] = Some(Cell { sgr: sgr.clone(), link: link.clone(), ch });
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
            .rposition(|cells| cells.iter().any(|c| c.as_ref().is_some_and(|c| c.ch != ' ')))
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

/// OSC 8 hyperlink: ESC ] 8 ; params ; URI (BEL | ESC \). `params` (the
/// optional `id=` and friends) is ignored — it only affects hover grouping.
///
/// Returns None when `seq` is some other OSC, `Some(None)` for the closing
/// form (empty URI), and `Some(Some(uri))` to open a link.
///
/// Distrusts the stream like the rest of this file. Past the `8;` prefix the
/// sequence IS a hyperlink, so anything malformed after it closes rather than
/// being ignored: ignoring would leave the previous link open and silently
/// attach it to every cell painted afterwards, which points a click at a URL
/// the text never mentioned. Control characters are stripped from the URI for
/// the same reason herdr strips them before sending (`sanitized_hyperlink_uri`)
/// — it is re-emitted into the local terminal, where a control byte inside an
/// OSC ends the string early and the tail executes. Deliberately no length cap:
/// herdr has none, and truncating a URL yields a *different* valid one, which
/// is the wrong-target failure this whole path exists to avoid.
fn parse_osc8(seq: &[char]) -> Option<Option<Rc<str>>> {
    let body: String = seq.iter().collect();
    let body = body.strip_prefix("\x1b]")?;
    let body = body.strip_suffix('\x07').or_else(|| body.strip_suffix("\x1b\\"))?;
    let rest = body.strip_prefix("8;")?;
    let uri = rest.split_once(';').map_or("", |(_params, uri)| uri);
    let uri: String = uri.chars().filter(|ch| !ch.is_control()).collect();
    Some((!uri.is_empty()).then(|| Rc::from(uri.as_str())))
}

// ---------------------------------------------------------------------------
// renderer: paints a window of the grid onto the local terminal

#[derive(Default)]
pub struct Renderer {
    last_rows: Vec<Option<String>>,
    status_text: String,
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

    /// Build the ANSI to paint the grid into an out_cols × out_rows terminal.
    /// Bottom-anchored window: agent TUIs live at the bottom of the screen.
    pub fn paint(&mut self, grid: &Grid, out_cols: usize, out_rows: usize) -> String {
        let bottom = grid.content_bottom.max(grid.cursor_row);
        let offset_r = (bottom + 1).saturating_sub(out_rows);
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
            let mut prev_sgr: Option<&str> = None;
            // No link is open at the start of a row: every row is repainted from
            // its own CUP, so hyperlink state never carries in from the row above.
            let mut prev_link: Option<&str> = None;
            let limit = out_cols.min(grid.width);
            let mut c = 0usize;
            while c < limit {
                let cell = cells.get(c).and_then(|c| c.as_ref());
                let sgr = cell.map(|c| &*c.sgr).unwrap_or("\x1b[0m");
                if prev_sgr != Some(sgr) {
                    line.push_str(if sgr.is_empty() { "\x1b[0m" } else { sgr });
                    prev_sgr = Some(sgr);
                }
                let link = cell.and_then(|c| c.link.as_deref());
                if prev_link != link {
                    match link {
                        Some(uri) => {
                            let _ = write!(line, "\x1b]8;;{uri}\x1b\\");
                        }
                        None => line.push_str("\x1b]8;;\x1b\\"),
                    }
                    prev_link = link;
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
            // A link left open would swallow everything painted after this row,
            // including the next row's CUP-anchored content and the status bar.
            if prev_link.is_some() {
                line.push_str("\x1b]8;;\x1b\\");
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
            if self.last_rows.get(r).map(|p| p.as_deref()) != Some(Some(painted.as_str())) {
                let _ = write!(out, "\x1b[{};1H", r + 1);
                out.push_str(&painted);
                self.last_rows[r] = Some(painted);
            }
        }
        let cr = grid.cursor_row as isize - offset_r as isize;
        if grid.cursor_visible && cr >= 0 && (cr as usize) < out_rows && self.status_text.is_empty() {
            let _ = write!(out, "\x1b[{};{}H\x1b[?25h", cr + 1, grid.cursor_col.min(out_cols.saturating_sub(1)) + 1);
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
        assert_eq!(parse_csi(&"\x1b[2 q".chars().collect::<Vec<_>>()), Some(("2".into(), 'q', 5)));
        assert_eq!(parse_csi(&"\x1b[?25h".chars().collect::<Vec<_>>()), Some(("?25".into(), 'h', 6)));
        assert_eq!(parse_csi(&"\x1b[1;31m".chars().collect::<Vec<_>>()), Some(("1;31".into(), 'm', 7)));
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
        assert!(!out.contains("abcde\x1b[0m\x1b[K"), "EL would erase the 'e': {out:?}");
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
        assert!(out.contains("\x1b[K"), "narrow grid must still emit EL: {out:?}");
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

    const LINK: &str = "\x1b]8;;https://example.com/x\x1b\\";
    const UNLINK: &str = "\x1b]8;;\x1b\\";

    #[test]
    fn osc8_is_captured_on_cells_and_repainted() {
        let mut g = Grid::new();
        g.resize(8, 2);
        g.apply(&format!("\x1b[1;1H{LINK}PR\x1b[1;3H{UNLINK}ok"));
        // the link rides the cells it covers, and only those
        assert_eq!(g.rows[0][0].as_ref().unwrap().link.as_deref(), Some("https://example.com/x"));
        assert_eq!(g.rows[0][1].as_ref().unwrap().link.as_deref(), Some("https://example.com/x"));
        assert_eq!(g.rows[0][3].as_ref().unwrap().link, None);
        // and survives the repaint, which is the whole point
        let out = Renderer::new().paint(&g, 8, 2);
        assert!(out.contains(LINK), "open sequence missing: {out:?}");
        assert!(out.contains(UNLINK), "close sequence missing: {out:?}");
    }

    #[test]
    fn osc8_params_are_ignored_and_bel_terminates() {
        // the `id=` form with a BEL terminator is equally valid
        let mut g = Grid::new();
        g.resize(4, 1);
        g.apply("\x1b[1;1H\x1b]8;id=7;https://example.com/y\x07hi");
        assert_eq!(g.rows[0][0].as_ref().unwrap().link.as_deref(), Some("https://example.com/y"));
    }

    #[test]
    fn non_hyperlink_osc_still_skipped() {
        // OSC 0 (window title) must neither print nor be mistaken for a link
        let mut g = Grid::new();
        g.resize(4, 1);
        g.apply("\x1b[1;1H\x1b]0;some title\x07hi");
        assert_eq!(g.text_lines(), vec!["hi"]);
        assert_eq!(g.rows[0][0].as_ref().unwrap().link, None);
    }

    #[test]
    fn open_link_cannot_leak_past_the_row() {
        // a link still open at the right edge would swallow the next row's
        // content, which the terminal paints from a fresh CUP
        let mut g = Grid::new();
        g.resize(2, 2);
        g.apply(&format!("\x1b[1;1H{LINK}ab\x1b[2;1H{UNLINK}cd"));
        let out = Renderer::new().paint(&g, 2, 2);
        let row2 = out.split("\x1b[2;1H").nth(1).expect("row 2 painted");
        assert!(!row2.contains("\x1b]8;;https"), "link leaked into row 2: {out:?}");
        assert_eq!(out.matches(UNLINK).count(), 1, "expected exactly one close: {out:?}");
    }

    #[test]
    fn malformed_osc8_closes_instead_of_bleeding() {
        // `8;` with no second `;` is not a link we can resolve. Treating it as
        // "not an OSC 8" would leave the open link riding every later cell, so
        // a click on unrelated text would open a URL the text never showed.
        let mut g = Grid::new();
        g.resize(4, 1);
        g.apply(&format!("\x1b[1;1H{LINK}ab\x1b]8;\x1b\\cd"));
        assert_eq!(g.rows[0][0].as_ref().unwrap().link.as_deref(), Some("https://example.com/x"));
        assert_eq!(g.rows[0][2].as_ref().unwrap().link, None, "stale link bled past the malformed close");
        assert_eq!(g.rows[0][3].as_ref().unwrap().link, None);
    }

    #[test]
    fn osc8_uri_control_bytes_are_stripped() {
        // the URI is re-emitted into the local terminal: a control byte inside
        // an OSC ends the string there and the tail is executed, so a hostile
        // remote could drive the terminal. herdr strips these too.
        let mut g = Grid::new();
        g.resize(4, 1);
        g.apply("\x1b[1;1H\x1b]8;;https://e.com/\r\n\x0ex\x1b\\hi");
        assert_eq!(g.rows[0][0].as_ref().unwrap().link.as_deref(), Some("https://e.com/x"));
        let out = Renderer::new().paint(&g, 4, 1);
        assert!(!out.contains('\r') && !out.contains('\x0e'), "control byte re-emitted: {out:?}");

        // nothing left after filtering is a close, not a link to ""
        let mut g = Grid::new();
        g.resize(4, 1);
        g.apply("\x1b[1;1H\x1b]8;;\r\n\x1b\\hi");
        assert_eq!(g.rows[0][0].as_ref().unwrap().link, None);
    }

    #[test]
    fn wrapped_link_reopens_on_the_continuation_row() {
        // rows are painted from their own CUP, so a link spanning a wrap must
        // be re-opened on the next row or only its first half stays clickable
        let mut g = Grid::new();
        g.resize(2, 2);
        g.apply(&format!("\x1b[1;1H{LINK}ab\x1b[2;1Hcd{UNLINK}"));
        let out = Renderer::new().paint(&g, 2, 2);
        let row2 = out.split("\x1b[2;1H").nth(1).expect("row 2 painted");
        assert!(row2.contains(LINK), "continuation row lost the link: {out:?}");
        assert_eq!(out.matches(LINK).count(), 2, "expected one open per row: {out:?}");
        assert_eq!(out.matches(UNLINK).count(), 2, "expected one close per row: {out:?}");
    }
}
