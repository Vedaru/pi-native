//! Editor hot paths: grapheme-aware wrapping, cursor movement, and line layout.
//!
//! These are the operations the editor runs on every keystroke, so they avoid
//! allocating a full grapheme vector per call where a streaming pass suffices.

use unicode_width::UnicodeWidthStr;

/// One display row produced by wrapping a logical line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Byte range of the logical line this row covers.
    pub start: usize,
    pub end: usize,
    /// Display width of `text[start..end]`.
    pub width: usize,
}

/// Wrap one logical line to `width` columns, breaking between graphemes and
/// preferring whitespace. A word longer than `width` is hard-split. Returns at
/// least one row (an empty line yields one empty row).
pub fn wrap_line(text: &str, width: usize) -> Vec<Row> {
    if width == 0 {
        return vec![Row {
            start: 0,
            end: text.len(),
            width: 0,
        }];
    }
    let mut rows = Vec::new();
    let mut row_start = 0usize;
    let mut row_width = 0usize;
    // Byte offset of the last whitespace within the current row, usable as a
    // break point (the space itself stays with the row it was on).
    let mut last_space: Option<(usize, usize)> = None;

    for (offset, grapheme) in clusters(text) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        // A whitespace grapheme that would overflow ends the row and is dropped
        // (it is not carried onto the next row).
        let is_space = grapheme.chars().all(char::is_whitespace);
        if row_width + grapheme_width > width && offset > row_start {
            let (end, next_start) = if is_space {
                (offset, offset + grapheme.len())
            } else {
                match last_space {
                    Some((space_offset, space_len)) if space_offset > row_start => {
                        (space_offset, space_offset + space_len)
                    }
                    _ => (offset, offset),
                }
            };
            rows.push(Row {
                start: row_start,
                end,
                width: UnicodeWidthStr::width(&text[row_start..end]),
            });
            row_start = next_start;
            row_width = if row_start <= offset {
                UnicodeWidthStr::width(&text[row_start..offset])
            } else {
                0
            };
            last_space = None;
            if is_space {
                continue;
            }
        }
        if is_space {
            last_space = Some((offset, grapheme.len()));
        }
        row_width += grapheme_width;
    }
    rows.push(Row {
        start: row_start,
        end: text.len(),
        width: UnicodeWidthStr::width(&text[row_start..]),
    });
    rows
}

/// Wrap many logical lines, returning the rows in order.
pub fn wrap_lines(lines: &[String], width: usize) -> Vec<Row> {
    lines
        .iter()
        .flat_map(|line| wrap_line(line, width))
        .collect()
}

/// Move a cursor `delta` display columns within `text`, snapping to grapheme
/// boundaries. Returns the new byte offset. Positive moves right, negative left.
pub fn move_cursor(text: &str, cursor: usize, delta: isize) -> usize {
    let cursor = cursor.min(text.len());
    if delta >= 0 {
        let mut offset = cursor;
        let mut remaining = delta;
        for (start, grapheme) in clusters_from(text, cursor) {
            if remaining <= 0 {
                break;
            }
            let width = UnicodeWidthStr::width(grapheme) as isize;
            remaining -= width.max(1);
            offset = start + grapheme.len();
        }
        offset
    } else {
        let mut offset = cursor;
        let mut remaining = -delta;
        for (start, grapheme) in clusters_rev(text, cursor) {
            if remaining <= 0 {
                break;
            }
            let width = UnicodeWidthStr::width(grapheme) as isize;
            remaining -= width.max(1);
            offset = start;
        }
        offset
    }
}

/// The display column of `cursor` within `text` (for horizontal scrolling).
pub fn cursor_column(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    UnicodeWidthStr::width(&text[..cursor])
}

/// Split `text` into graphemes with their byte offsets.
fn clusters(text: &str) -> Vec<(usize, &str)> {
    clusters_from(text, 0)
}

/// Graphemes at or after byte `start`.
fn clusters_from(text: &str, start: usize) -> Vec<(usize, &str)> {
    let mut result = Vec::new();
    let base = start.min(text.len());
    let mut cluster_start = base;
    let mut first = true;
    for (index, ch) in text[base..].char_indices() {
        // `index` is relative to `base`, and we iterate the slice only once, so
        // the absolute byte offset is always `base + index`.
        let absolute = base + index;
        if first {
            first = false;
            continue;
        }
        if is_combining(ch) {
            continue;
        }
        result.push((cluster_start, &text[cluster_start..absolute]));
        cluster_start = absolute;
    }
    if cluster_start < text.len() {
        result.push((cluster_start, &text[cluster_start..]));
    }
    result
}

/// Graphemes that end at or before byte `end`, in reverse order.
fn clusters_rev(text: &str, end: usize) -> Vec<(usize, &str)> {
    let mut forward = clusters_from(text, 0);
    forward.retain(|(start, _)| *start < end.min(text.len()));
    forward.reverse();
    forward
}

fn is_combining(ch: char) -> bool {
    matches!(ch as u32,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x0610..=0x061A
        | 0x064B..=0x065F | 0x0670 | 0x06D6..=0x06DC | 0x0E31 | 0x0E34..=0x0E3A
        | 0x0E47..=0x0E4E | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF
        | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F
    )
}
