//! Cells and rows.

use bitflags::bitflags;

use crate::style::StyleId;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct CellFlags: u8 {
        /// First half of a double-width character.
        const WIDE = 1 << 0;
        /// Second half of a double-width character (no content of its own).
        const WIDE_SPACER = 1 << 1;
        /// Padding in the last column left by a wide char that wrapped to the next row.
        const LEADING_WIDE_SPACER = 1 << 2;
    }
}

/// One grid cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// Base character of the grapheme (' ' for blank).
    pub ch: char,
    pub style: StyleId,
    pub flags: CellFlags,
    /// Combining marks / further code points of the grapheme cluster.
    pub extra: Option<Box<str>>,
}

impl Default for Cell {
    fn default() -> Self {
        Self::blank(StyleId::DEFAULT)
    }
}

impl Cell {
    pub const fn blank(style: StyleId) -> Self {
        Self { ch: ' ', style, flags: CellFlags::empty(), extra: None }
    }

    /// Blank with default style: carries no information and can be trimmed.
    pub fn is_empty(&self) -> bool {
        self.ch == ' ' && self.style == StyleId::DEFAULT && self.flags.is_empty() && self.extra.is_none()
    }

    pub fn is_spacer(&self) -> bool {
        self.flags.intersects(CellFlags::WIDE_SPACER | CellFlags::LEADING_WIDE_SPACER)
    }

    /// Display width of this cell (0 for spacers).
    pub fn width(&self) -> usize {
        if self.flags.contains(CellFlags::WIDE) {
            2
        } else if self.is_spacer() {
            0
        } else {
            1
        }
    }

    /// Append the cell's text (base char + extras) to `out`.
    pub fn push_text(&self, out: &mut String) {
        out.push(self.ch);
        if let Some(extra) = &self.extra {
            out.push_str(extra);
        }
    }

    pub fn push_extra(&mut self, c: char) {
        let mut s = self.extra.take().map(String::from).unwrap_or_default();
        s.push(c);
        self.extra = Some(s.into_boxed_str());
    }
}

/// A row of cells. `wrapped` means the logical line continues on the next row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
}

impl Row {
    pub fn new(cols: usize, style: StyleId) -> Self {
        Self { cells: vec![Cell::blank(style); cols], wrapped: false }
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Reset every cell to blank with the given style.
    pub fn clear(&mut self, style: StyleId) {
        for c in &mut self.cells {
            *c = Cell::blank(style);
        }
        self.wrapped = false;
    }

    /// Number of cells up to and including the last non-empty one.
    pub fn content_len(&self) -> usize {
        self.cells.iter().rposition(|c| !c.is_empty()).map_or(0, |i| i + 1)
    }

    pub fn is_blank(&self) -> bool {
        self.content_len() == 0
    }

    /// Text of `cols` range, skipping spacers. Trailing blanks are kept; callers trim.
    pub fn text(&self, range: std::ops::Range<usize>) -> String {
        let mut s = String::new();
        let end = range.end.min(self.cells.len());
        for c in &self.cells[range.start.min(end)..end] {
            if !c.is_spacer() {
                c.push_text(&mut s);
            }
        }
        s
    }

    /// Resize without reflow: truncate or pad, never leaving half a wide char.
    pub fn resize(&mut self, cols: usize, style: StyleId) {
        if cols < self.cells.len() {
            self.cells.truncate(cols);
            if let Some(last) = self.cells.last_mut()
                && last.flags.contains(CellFlags::WIDE)
            {
                *last = Cell::blank(last.style);
            }
        } else if cols > self.cells.len() {
            // A leading spacer is only meaningful in the last column.
            if let Some(last) = self.cells.last_mut()
                && last.flags.contains(CellFlags::LEADING_WIDE_SPACER)
            {
                *last = Cell::blank(last.style);
            }
            self.cells.resize(cols, Cell::blank(style));
        }
    }
}
