//! Streaming decoder for SGR mouse reports mixed with ordinary terminal input.
//!
//! PTY reads have arbitrary boundaries, so a report such as
//! `ESC [ < 64 ; 10 ; 5 M` may arrive in any number of chunks. This decoder
//! carries only a syntactically valid, incomplete report between calls. All
//! other bytes are returned unchanged, including malformed escape sequences.

/// Maximum number of bytes retained for one incomplete SGR mouse report.
///
/// Real reports are much smaller. The bound prevents a malformed digit run
/// from growing the carry buffer indefinitely while input continues to arrive.
pub const MAX_SGR_MOUSE_SEQUENCE_LEN: usize = 64;

/// One complete `CSI < button ; column ; row M/m` mouse report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SgrMouseEvent {
    /// Raw SGR button value, including any modifier and motion bits.
    pub button: u32,
    /// One-based terminal column from the wire protocol.
    pub column: u32,
    /// One-based terminal row from the wire protocol.
    pub row: u32,
    /// `true` for an `M` report and `false` for an `m` release report.
    pub press: bool,
    raw: Vec<u8>,
}

impl SgrMouseEvent {
    /// Recover the exact bytes consumed for this event without copying.
    #[cfg(test)]
    pub fn into_raw(self) -> Vec<u8> {
        self.raw
    }
}

/// A decoded item from the mixed terminal-input stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MouseInputItem {
    /// Bytes that are not a complete SGR mouse report.
    Bytes(Vec<u8>),
    /// One complete SGR mouse report.
    Mouse(SgrMouseEvent),
}

/// Incremental SGR mouse parser for arbitrarily chunked PTY input.
#[derive(Debug, Default)]
pub struct SgrMouseInputParser {
    pending: Vec<u8>,
}

impl SgrMouseInputParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed another arbitrary input chunk.
    ///
    /// Returned `Bytes` items and `Mouse` event raw bytes, in order, reproduce
    /// every input byte exactly. A syntactically valid but incomplete SGR mouse
    /// prefix is held until the next call or [`Self::finish`].
    pub fn push(&mut self, chunk: &[u8]) -> Vec<MouseInputItem> {
        let mut input = Vec::with_capacity(self.pending.len() + chunk.len());
        input.append(&mut self.pending);
        input.extend_from_slice(chunk);

        let mut items = Vec::new();
        let mut cursor = 0usize;
        while cursor < input.len() {
            let Some(relative_escape) = input[cursor..].iter().position(|byte| *byte == 0x1b)
            else {
                push_bytes(&mut items, &input[cursor..]);
                break;
            };
            let escape = cursor + relative_escape;
            push_bytes(&mut items, &input[cursor..escape]);

            match parse_sgr_mouse_prefix(&input[escape..]) {
                PrefixParse::Complete {
                    button,
                    column,
                    row,
                    press,
                    len,
                } => {
                    items.push(MouseInputItem::Mouse(SgrMouseEvent {
                        button,
                        column,
                        row,
                        press,
                        raw: input[escape..escape + len].to_vec(),
                    }));
                    cursor = escape + len;
                }
                PrefixParse::Incomplete => {
                    self.pending.extend_from_slice(&input[escape..]);
                    debug_assert!(self.pending.len() < MAX_SGR_MOUSE_SEQUENCE_LEN);
                    break;
                }
                PrefixParse::NotMouse => {
                    // Release only ESC here. Scanning the remainder again lets
                    // us recover if a malformed sequence contains the start of
                    // a later valid mouse report.
                    push_bytes(&mut items, &input[escape..escape + 1]);
                    cursor = escape + 1;
                }
            }
        }
        items
    }

    /// Flush an incomplete prefix as ordinary bytes.
    ///
    /// Besides end-of-input, callers can use this after their input-framing
    /// timeout so a standalone Escape key is not retained indefinitely.
    pub fn flush_pending(&mut self) -> Vec<MouseInputItem> {
        if self.pending.is_empty() {
            Vec::new()
        } else {
            vec![MouseInputItem::Bytes(std::mem::take(&mut self.pending))]
        }
    }

    /// Finish the stream, returning any incomplete prefix as ordinary bytes.
    #[cfg(test)]
    pub fn finish(&mut self) -> Vec<MouseInputItem> {
        self.flush_pending()
    }

    /// Number of incomplete-prefix bytes currently retained.
    pub fn buffered_len(&self) -> usize {
        self.pending.len()
    }

    /// Whether the retained bytes have passed the ambiguous Escape/CSI prefix
    /// and are definitely the beginning of an SGR mouse report.
    ///
    /// Callers may time out a bare Escape or `ESC [` so keyboard input remains
    /// responsive. Once `ESC [ <` has arrived, however, flushing the partial
    /// report as ordinary bytes would inject mouse-protocol garbage into the
    /// remote terminal. Keep that bounded prefix until its remaining bytes
    /// arrive instead.
    pub fn pending_is_mouse_report(&self) -> bool {
        self.pending.starts_with(b"\x1b[<")
    }
}

fn push_bytes(items: &mut Vec<MouseInputItem>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if let Some(MouseInputItem::Bytes(existing)) = items.last_mut() {
        existing.extend_from_slice(bytes);
    } else {
        items.push(MouseInputItem::Bytes(bytes.to_vec()));
    }
}

enum PrefixParse {
    Complete {
        button: u32,
        column: u32,
        row: u32,
        press: bool,
        len: usize,
    },
    Incomplete,
    NotMouse,
}

/// Parse a buffer known to begin with ESC. `Incomplete` is returned only while
/// every byte seen so far is a valid SGR mouse prefix and remains under the
/// carry limit.
fn parse_sgr_mouse_prefix(bytes: &[u8]) -> PrefixParse {
    debug_assert_eq!(bytes.first(), Some(&0x1b));

    for (index, expected) in [(1usize, b'['), (2usize, b'<')] {
        let Some(actual) = bytes.get(index) else {
            return incomplete_or_oversized(bytes.len());
        };
        if *actual != expected {
            return PrefixParse::NotMouse;
        }
    }

    let mut values = [0u32; 3];
    let mut cursor = 3usize;
    for field in 0..3 {
        let digit_start = cursor;
        loop {
            let Some(byte) = bytes.get(cursor) else {
                return incomplete_or_oversized(bytes.len());
            };
            if !byte.is_ascii_digit() {
                break;
            }
            // Match the previous one-shot parser's overflow-safe behavior.
            values[field] = values[field]
                .saturating_mul(10)
                .saturating_add((byte - b'0') as u32);
            cursor += 1;
            if cursor >= MAX_SGR_MOUSE_SEQUENCE_LEN {
                return PrefixParse::NotMouse;
            }
        }
        if cursor == digit_start {
            return PrefixParse::NotMouse;
        }

        let byte = bytes[cursor];
        if field < 2 {
            if byte != b';' {
                return PrefixParse::NotMouse;
            }
            cursor += 1;
            if cursor >= MAX_SGR_MOUSE_SEQUENCE_LEN {
                return PrefixParse::NotMouse;
            }
            continue;
        }

        let press = match byte {
            b'M' => true,
            b'm' => false,
            _ => return PrefixParse::NotMouse,
        };
        return PrefixParse::Complete {
            button: values[0],
            column: values[1],
            row: values[2],
            press,
            len: cursor + 1,
        };
    }

    unreachable!("the three SGR mouse fields always return from the loop")
}

fn incomplete_or_oversized(len: usize) -> PrefixParse {
    if len < MAX_SGR_MOUSE_SEQUENCE_LEN {
        PrefixParse::Incomplete
    } else {
        PrefixParse::NotMouse
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn append_raw(out: &mut Vec<u8>, items: Vec<MouseInputItem>) {
        for item in items {
            match item {
                MouseInputItem::Bytes(bytes) => out.extend(bytes),
                MouseInputItem::Mouse(event) => out.extend(event.into_raw()),
            }
        }
    }

    fn parse_chunks<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> Vec<MouseInputItem> {
        let mut parser = SgrMouseInputParser::new();
        let mut items = Vec::new();
        for chunk in chunks {
            items.extend(parser.push(chunk));
        }
        items.extend(parser.finish());
        items
    }

    fn mouse_events(items: &[MouseInputItem]) -> Vec<&SgrMouseEvent> {
        items
            .iter()
            .filter_map(|item| match item {
                MouseInputItem::Mouse(event) => Some(event),
                MouseInputItem::Bytes(_) => None,
            })
            .collect()
    }

    fn assert_lossless(expected: &[u8], items: Vec<MouseInputItem>) {
        let mut actual = Vec::new();
        append_raw(&mut actual, items);
        assert_eq!(actual, expected);
    }

    #[test]
    fn parses_fields_release_and_exact_raw_bytes() {
        let input = b"\x1b[<32;140;51m";
        let items = parse_chunks([input.as_slice()]);
        let events = mouse_events(&items);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].button, 32);
        assert_eq!(events[0].column, 140);
        assert_eq!(events[0].row, 51);
        assert!(!events[0].press);
        assert_eq!(events[0].raw, input);
        assert_lossless(input, items);
    }

    #[test]
    fn wheel_reports_survive_every_two_chunk_boundary() {
        for (input, expected_button) in [
            (b"\x1b[<64;10;5M".as_slice(), 64),
            (b"\x1b[<65;10;5M".as_slice(), 65),
        ] {
            for split in 0..=input.len() {
                let items = parse_chunks([&input[..split], &input[split..]]);
                let events = mouse_events(&items);
                assert_eq!(events.len(), 1, "input={input:?}, split={split}");
                assert_eq!(events[0].button, expected_button);
                assert!(events[0].press);
                assert_lossless(input, items);
            }
        }
    }

    #[test]
    fn wheel_report_survives_every_three_chunk_boundary_pair() {
        let input = b"\x1b[<64;123;45M";
        for first in 0..=input.len() {
            for second in first..=input.len() {
                let items =
                    parse_chunks([&input[..first], &input[first..second], &input[second..]]);
                let events = mouse_events(&items);
                assert_eq!(events.len(), 1, "first={first}, second={second}");
                assert_eq!(events[0].button, 64);
                assert_eq!((events[0].column, events[0].row), (123, 45));
                assert_lossless(input, items);
            }
        }
    }

    #[test]
    fn wheel_report_survives_byte_at_a_time() {
        let input = b"\x1b[<64;10;5M";
        let chunks: Vec<&[u8]> = input.iter().map(std::slice::from_ref).collect();
        let items = parse_chunks(chunks);
        assert_eq!(mouse_events(&items).len(), 1);
        assert_lossless(input, items);
    }

    #[test]
    fn mixed_bytes_and_multiple_mouse_reports_are_lossless() {
        let input = b"abc\x1b[<0;2;3Mdef\x1b[<35;9;8Mghi\x1b[<0;2;3m";
        let items = parse_chunks([&input[..5], &input[5..17], &input[17..31], &input[31..]]);
        let events = mouse_events(&items);
        assert_eq!(events.len(), 3);
        assert_eq!(
            events.iter().map(|event| event.button).collect::<Vec<_>>(),
            [0, 35, 0]
        );
        assert!(!events[2].press);
        assert_lossless(input, items);
    }

    #[test]
    fn malformed_sequences_are_ordinary_and_parser_resynchronizes() {
        let input = b"a\x1b[<64;12;xbroken\x1b[<65;7;8Mtail";
        for split in 0..=input.len() {
            let items = parse_chunks([&input[..split], &input[split..]]);
            let events = mouse_events(&items);
            assert_eq!(events.len(), 1, "split={split}");
            assert_eq!(events[0].button, 65);
            assert_lossless(input, items);
        }
    }

    #[test]
    fn finish_releases_each_incomplete_prefix_losslessly() {
        for input in [
            b"\x1b".as_slice(),
            b"\x1b[".as_slice(),
            b"\x1b[<".as_slice(),
            b"\x1b[<64".as_slice(),
            b"\x1b[<64;10;".as_slice(),
        ] {
            let items = parse_chunks(input.iter().map(std::slice::from_ref));
            assert!(mouse_events(&items).is_empty());
            assert_lossless(input, items);
        }
    }

    #[test]
    fn oversized_incomplete_report_never_grows_carry_past_bound() {
        let mut input = b"prefix\x1b[<".to_vec();
        input.extend(std::iter::repeat_n(b'9', MAX_SGR_MOUSE_SEQUENCE_LEN * 3));

        let mut parser = SgrMouseInputParser::new();
        let mut raw = Vec::new();
        for byte in &input {
            append_raw(&mut raw, parser.push(std::slice::from_ref(byte)));
            assert!(parser.buffered_len() < MAX_SGR_MOUSE_SEQUENCE_LEN);
        }
        append_raw(&mut raw, parser.finish());
        assert_eq!(raw, input);
        assert_eq!(parser.buffered_len(), 0);
    }

    #[test]
    fn numeric_overflow_saturates_without_losing_bytes() {
        let input = b"\x1b[<42949672960;99999999999;99999999999M";
        let items = parse_chunks([input.as_slice()]);
        let events = mouse_events(&items);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].button, u32::MAX);
        assert_eq!(events[0].column, u32::MAX);
        assert_eq!(events[0].row, u32::MAX);
        assert_lossless(input, items);
    }

    #[test]
    fn malformed_escape_can_be_followed_immediately_by_valid_mouse() {
        let input = b"\x1b\x1b[<64;1;1M";
        let items = parse_chunks(input.iter().map(std::slice::from_ref));
        let events = mouse_events(&items);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].button, 64);
        assert_lossless(input, items);
    }

    #[test]
    fn exposes_when_a_partial_prefix_is_definitely_mouse_input() {
        let mut parser = SgrMouseInputParser::new();
        assert!(parser.push(b"\x1b").is_empty());
        assert!(!parser.pending_is_mouse_report());

        assert!(parser.push(b"[").is_empty());
        assert!(!parser.pending_is_mouse_report());

        assert!(parser.push(b"<64;").is_empty());
        assert!(parser.pending_is_mouse_report());

        let items = parser.push(b"10;5M");
        assert_eq!(mouse_events(&items).len(), 1);
        assert!(!parser.pending_is_mouse_report());
        assert_eq!(parser.buffered_len(), 0);
    }
}
