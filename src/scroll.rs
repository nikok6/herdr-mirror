//! Pure parsing and geometry for Herdr terminal scroll metrics.

use serde_json::Value;

/// Scroll position reported by a `terminal.state` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollInfo {
    pub offset_from_bottom: u64,
    pub max_offset_from_bottom: u64,
    pub viewport_rows: u64,
}

impl ScrollInfo {
    /// Parse the `scroll` value from a terminal-state event.
    ///
    /// Unknown object fields are deliberately ignored so a newer Herdr can add
    /// metrics without coordinating a mirror release. Missing, null, signed,
    /// fractional, or otherwise malformed required fields make this update
    /// unavailable instead of retaining or guessing scroll state.
    pub fn from_value(value: &Value) -> Option<Self> {
        Some(Self {
            offset_from_bottom: value.get("offset_from_bottom")?.as_u64()?,
            max_offset_from_bottom: value.get("max_offset_from_bottom")?.as_u64()?,
            viewport_rows: value.get("viewport_rows")?.as_u64()?,
        })
    }
}

/// Zero-based thumb geometry within a scrollbar track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarThumb {
    pub top: usize,
    pub len: usize,
}

/// Match Herdr's native proportional scrollbar geometry without overflowing.
///
/// Herdr places the thumb at the top when the viewport is at maximum
/// scrollback and at the bottom when `offset_from_bottom` is zero. Integer
/// half-up rounding matches the positive-number behavior of Herdr's `f32`
/// calculation while `u128` intermediates keep valid `u64` metrics safe.
pub fn scrollbar_thumb(metrics: ScrollInfo, track_rows: usize) -> Option<ScrollbarThumb> {
    if metrics.max_offset_from_bottom == 0 || track_rows == 0 {
        return None;
    }

    let total_rows = u128::from(metrics.max_offset_from_bottom) + u128::from(metrics.viewport_rows);
    if total_rows == 0 {
        return None;
    }

    let thumb_len = rounded_ratio(u128::from(metrics.viewport_rows), track_rows, total_rows)
        .max(1)
        .min(track_rows);
    let max_thumb_top = track_rows.saturating_sub(thumb_len);
    let scrolled_from_top = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    let thumb_top = if max_thumb_top == 0 {
        0
    } else {
        rounded_ratio(
            u128::from(scrolled_from_top),
            max_thumb_top,
            u128::from(metrics.max_offset_from_bottom),
        )
        .min(max_thumb_top)
    };

    Some(ScrollbarThumb {
        top: thumb_top,
        len: thumb_len,
    })
}

fn rounded_ratio(value: u128, scale: usize, denominator: u128) -> usize {
    if denominator == 0 {
        return 0;
    }

    // `value` originates as u64 and `scale` is usize, so their product and
    // the half-denominator rounding term fit in u128 on supported targets.
    let numerator = value * scale as u128;
    ((numerator + denominator / 2) / denominator).min(usize::MAX as u128) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typed_scroll_state_and_ignores_new_fields() {
        let value = serde_json::json!({
            "offset_from_bottom": 12,
            "max_offset_from_bottom": 240,
            "viewport_rows": 30,
            "future_metric": { "anything": true },
        });

        assert_eq!(
            ScrollInfo::from_value(&value),
            Some(ScrollInfo {
                offset_from_bottom: 12,
                max_offset_from_bottom: 240,
                viewport_rows: 30,
            })
        );
    }

    #[test]
    fn malformed_or_partial_scroll_state_is_unavailable() {
        for value in [
            serde_json::json!(null),
            serde_json::json!({}),
            serde_json::json!({
                "offset_from_bottom": 12,
                "max_offset_from_bottom": 240,
            }),
            serde_json::json!({
                "offset_from_bottom": -1,
                "max_offset_from_bottom": 240,
                "viewport_rows": 30,
            }),
            serde_json::json!({
                "offset_from_bottom": "12",
                "max_offset_from_bottom": 240,
                "viewport_rows": 30,
            }),
            serde_json::json!({
                "offset_from_bottom": 12,
                "max_offset_from_bottom": 240.5,
                "viewport_rows": 30,
            }),
        ] {
            assert_eq!(ScrollInfo::from_value(&value), None, "value: {value}");
        }
    }

    #[test]
    fn thumb_matches_herdr_at_top_middle_and_bottom() {
        let at_top = ScrollInfo {
            offset_from_bottom: 90,
            max_offset_from_bottom: 90,
            viewport_rows: 10,
        };
        let halfway = ScrollInfo {
            offset_from_bottom: 45,
            ..at_top
        };
        let at_bottom = ScrollInfo {
            offset_from_bottom: 0,
            ..at_top
        };

        assert_eq!(
            scrollbar_thumb(at_top, 10),
            Some(ScrollbarThumb { top: 0, len: 1 })
        );
        assert_eq!(
            scrollbar_thumb(halfway, 10),
            Some(ScrollbarThumb { top: 5, len: 1 })
        );
        assert_eq!(
            scrollbar_thumb(at_bottom, 10),
            Some(ScrollbarThumb { top: 9, len: 1 })
        );
    }

    #[test]
    fn thumb_is_proportional_and_hides_without_a_track_or_history() {
        let metrics = ScrollInfo {
            offset_from_bottom: 0,
            max_offset_from_bottom: 80,
            viewport_rows: 20,
        };
        assert_eq!(
            scrollbar_thumb(metrics, 10),
            Some(ScrollbarThumb { top: 8, len: 2 })
        );
        assert_eq!(scrollbar_thumb(metrics, 0), None);
        assert_eq!(
            scrollbar_thumb(
                ScrollInfo {
                    max_offset_from_bottom: 0,
                    ..metrics
                },
                10,
            ),
            None
        );
    }

    #[test]
    fn thumb_handles_maximum_wire_values_without_overflow() {
        let metrics = ScrollInfo {
            offset_from_bottom: 0,
            max_offset_from_bottom: u64::MAX,
            viewport_rows: u64::MAX,
        };
        assert_eq!(
            scrollbar_thumb(metrics, usize::MAX),
            Some(ScrollbarThumb {
                top: usize::MAX / 2,
                len: usize::MAX / 2 + 1,
            })
        );
    }
}
