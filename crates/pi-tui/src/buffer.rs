//! Cell grid: the renderer's backing store.
//!
//! A frame is a rectangular grid of [`Cell`]s. Writing text advances a cursor
//! by the grapheme's display width, so wide (CJK) and zero-width (combining)
//! graphemes occupy the right number of columns. The differential renderer
//! ([`crate::render`]) compares two buffers cell by cell.

use unicode_width::UnicodeWidthStr;

/// Style applied to a cell. `Default` means "no SGR sequence".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub dim: bool,
    pub reverse: bool,
}

/// A color in one of the terminal's color spaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// One of the 16 ANSI colors (0-7 normal, 8-15 bright).
    Ansi(u8),
    /// 256-color palette index.
    Indexed(u8),
    /// 24-bit truecolor.
    Rgb(u8, u8, u8),
}

/// One character cell. A wide grapheme is stored as the grapheme in the first
/// column and [`Cell::continuation`] in the columns it covers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cell {
    pub ch: Option<char>,
    pub style: Style,
    /// True when this column is the tail of a wide grapheme in the column to
    /// its left. Continuations are still rendered (as blanks) so the diff does
    /// not leave stale glyphs, but their `ch` is unused.
    pub continuation: bool,
}

impl Cell {
    /// A blank cell with the given style.
    pub fn blank(style: Style) -> Self {
        Self {
            ch: None,
            style,
            continuation: false,
        }
    }
}

/// A rectangular grid of cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Buffer {
    width: usize,
    height: usize,
    cells: Vec<Cell>,
}

impl Buffer {
    /// A grid of blank, default-styled cells.
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![Cell::default(); width * height],
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn cell(&self, x: usize, y: usize) -> Option<&Cell> {
        if x >= self.width || y >= self.height {
            return None;
        }
        self.cells.get(y * self.width + x)
    }

    fn cell_mut(&mut self, x: usize, y: usize) -> Option<&mut Cell> {
        if x >= self.width || y >= self.height {
            return None;
        }
        self.cells.get_mut(y * self.width + x)
    }

    /// Render `text` at `(x, y)`, advancing by display width and never wrapping.
    /// Returns the column after the last glyph written.
    pub fn put_str(&mut self, x: usize, y: usize, text: &str, style: Style) -> usize {
        let mut column = x;
        // Iterate grapheme clusters so combining marks and emoji stay together.
        for grapheme in unicode_graphemes(text) {
            let width = UnicodeWidthStr::width(grapheme);
            if width == 0 {
                // Zero-width grapheme: attach to the previous cell's character
                // is not representable in a flat grid, so skip it rather than
                // misplacing it. (The terminal renders the base char.)
                continue;
            }
            if column >= self.width {
                break;
            }
            if let Some(ch) = grapheme.chars().next() {
                if let Some(cell) = self.cell_mut(column, y) {
                    cell.ch = Some(ch);
                    cell.style = style;
                    cell.continuation = false;
                }
                // Cover the remaining columns of a wide grapheme with
                // continuation cells.
                for offset in 1..width {
                    if let Some(cell) = self.cell_mut(column + offset, y) {
                        cell.ch = Some(' ');
                        cell.style = style;
                        cell.continuation = true;
                    }
                }
            }
            column += width;
        }
        column
    }

    /// Fill a row with `style`'s background from `x` to the right edge.
    pub fn fill_row(&mut self, x: usize, y: usize, style: Style) {
        for column in x..self.width {
            if let Some(cell) = self.cell_mut(column, y) {
                *cell = Cell::blank(style);
            }
        }
    }
}

/// Minimal grapheme segmentation: split on chars (one char per cluster) except
/// that a combining mark joins the preceding cluster. This covers the width
/// cases the renderer needs without a full UAX #29 dependency.
fn unicode_graphemes(text: &str) -> Vec<&str> {
    let mut clusters: Vec<&str> = Vec::new();
    let mut start = 0usize;
    for (index, ch) in text.char_indices() {
        if index == 0 {
            continue;
        }
        if is_combining(ch) {
            continue;
        }
        clusters.push(&text[start..index]);
        start = index;
    }
    if start < text.len() {
        clusters.push(&text[start..]);
    }
    clusters
}

/// True for combining marks (Unicode categories Mn/Me plus the variation
/// selectors). These are zero-width and belong to the previous cluster.
fn is_combining(ch: char) -> bool {
    matches!(ch as u32,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x0610..=0x061A
        | 0x064B..=0x065F | 0x0670 | 0x06D6..=0x06DC | 0x0E31 | 0x0E34..=0x0E3A
        | 0x0E47..=0x0E4E | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF
        | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F
    )
}
