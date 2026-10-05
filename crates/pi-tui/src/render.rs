//! Differential renderer.
//!
//! Compares two [`Buffer`]s and emits the minimal ANSI byte sequence that turns
//! `previous` into `next`: it seeks to each run of changed cells, sets the style
//! only when it differs from the active one, and skips unchanged runs entirely.
//! This is what keeps a large transcript from re-emitting the whole screen every
//! frame.

use crate::buffer::{Buffer, Color, Style};

/// Turn a style back into the SGR sequences needed to activate it, given the
/// style currently active on the terminal. Emits `\x1b[0m` to reset when moving
/// from a styled cell to a plain one.
fn style_sequence(next: Style, active: Style) -> String {
    let mut out = String::new();
    if next == active {
        return out;
    }
    // A reset is the simplest correct way to leave an arbitrary prior state.
    out.push_str("\x1b[0m");
    if next.bold {
        out.push_str("\x1b[1m");
    }
    if next.dim {
        out.push_str("\x1b[2m");
    }
    if next.italic {
        out.push_str("\x1b[3m");
    }
    if next.underline {
        out.push_str("\x1b[4m");
    }
    if next.reverse {
        out.push_str("\x1b[7m");
    }
    if let Some(color) = next.fg {
        out.push_str(&color_sequence(color, false));
    }
    if let Some(color) = next.bg {
        out.push_str(&color_sequence(color, true));
    }
    out
}

fn color_sequence(color: Color, background: bool) -> String {
    match color {
        Color::Ansi(index) => {
            let base = if background { 40 } else { 30 };
            if index < 8 {
                format!("\x1b[{}m", base + index as u16)
            } else {
                format!("\x1b[{}m", base + 60 + (index as u16 - 8))
            }
        }
        Color::Indexed(index) => {
            let base = if background { 48 } else { 38 };
            format!("\x1b[{base};5;{index}m")
        }
        Color::Rgb(r, g, b) => {
            let base = if background { 48 } else { 38 };
            format!("\x1b[{base};2;{r};{g};{b}m")
        }
    }
}

/// Emit the escape that seeks the cursor to `(row, column)` (both zero-based).
fn cursor_sequence(x: usize, y: usize) -> String {
    // ANSI cursor positions are 1-based: CSI row;column H.
    format!("\x1b[{};{}H", y + 1, x + 1)
}

/// A cell differs if its character, style, or continuation flag differs.
fn changed(a: &crate::buffer::Cell, b: &crate::buffer::Cell) -> bool {
    a.ch != b.ch || a.style != b.style || a.continuation != b.continuation
}

/// Render the delta from `previous` to `next`.
///
/// `previous` is `None` for the first frame, which forces every cell to be
/// written. Buffers of different dimensions are fully redrawn (the caller is
/// expected to clear/resize first).
pub fn diff(previous: Option<&Buffer>, next: &Buffer) -> String {
    let mut out = String::new();
    let full = previous.is_none() || previous.map(|p| p.width() != next.width()) == Some(true);
    let mut active = Style::default();
    let mut cursor: Option<(usize, usize)> = None;

    // A comparable previous frame only when the dimensions match.
    let previous =
        previous.filter(|prev| prev.width() == next.width() && prev.height() == next.height());

    for y in 0..next.height() {
        // Skip an unchanged row entirely (the common case for a scrolling log).
        if !full {
            if let Some(prev) = previous {
                if (0..next.width())
                    .all(|x| !changed(prev.cell(x, y).unwrap(), next.cell(x, y).unwrap()))
                {
                    continue;
                }
            }
        }
        let mut x = 0usize;
        while x < next.width() {
            let cell = next.cell(x, y).expect("in bounds");
            if !full {
                if let Some(prev) = previous {
                    if !changed(prev.cell(x, y).unwrap(), cell) {
                        x += 1;
                        continue;
                    }
                }
            }
            // Seek to the run start only when the cursor is not already there.
            if cursor != Some((x, y)) {
                out.push_str(&cursor_sequence(x, y));
            }
            while x < next.width() {
                let cell = next.cell(x, y).expect("in bounds");
                if !full {
                    if let Some(prev) = previous {
                        if !changed(prev.cell(x, y).unwrap(), cell) {
                            break;
                        }
                    }
                }
                if cell.style != active {
                    out.push_str(&style_sequence(cell.style, active));
                    active = cell.style;
                }
                match cell.ch {
                    Some(ch) => out.push(ch),
                    None => out.push(' '),
                }
                x += 1;
            }
            cursor = Some((x, y));
        }
    }
    if active != Style::default() {
        out.push_str("\x1b[0m");
    }
    out
}

/// A convenience wrapper that keeps the previous frame and returns the delta.
#[derive(Debug, Default)]
pub struct Renderer {
    previous: Option<Buffer>,
    /// Set when the physical terminal was cleared, forcing a full redraw.
    force_full: bool,
}

impl Renderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the terminal as cleared so the next frame is drawn in full.
    pub fn invalidate(&mut self) {
        self.previous = None;
        self.force_full = true;
    }

    /// Render `next`, remembering it as the new baseline.
    pub fn render(&mut self, next: Buffer) -> String {
        let previous = if self.force_full {
            None
        } else {
            self.previous.as_ref()
        };
        let output = diff(previous, &next);
        self.previous = Some(next);
        self.force_full = false;
        output
    }
}
