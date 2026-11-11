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

const CHAR_MASK: u32 = 0x1f_ffff;
const FLAGS_SHIFT: u32 = 24;

/// One grid cell: 16 bytes. The base character and the flags share one
/// word; the rare extra text (combining marks, bidi prefix) lives behind
/// one thin pointer.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Cell {
    /// Base character (low 21 bits) and [`CellFlags`] (high 8 bits).
    code: u32,
    pub style: StyleId,
    text: Option<Box<CellText>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
struct CellText {
    /// Zero-width bidi formatting at the start of a row, before this character.
    prefix: Box<str>,
    /// Combining marks / further code points of the grapheme cluster.
    extra: Box<str>,
}

impl std::fmt::Debug for Cell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cell")
            .field("ch", &self.ch())
            .field("style", &self.style)
            .field("flags", &self.flags())
            .field("extra", &self.extra())
            .field("prefix", &self.prefix())
            .finish()
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self::blank(StyleId::DEFAULT)
    }
}

impl Cell {
    pub const fn new(ch: char, style: StyleId, flags: CellFlags) -> Self {
        Self { code: ch as u32 | (flags.bits() as u32) << FLAGS_SHIFT, style, text: None }
    }

    pub const fn blank(style: StyleId) -> Self {
        Self::new(' ', style, CellFlags::empty())
    }

    /// Base character of the grapheme (' ' for blank).
    #[inline]
    pub fn ch(&self) -> char {
        char::from_u32(self.code & CHAR_MASK).unwrap_or(' ')
    }

    #[inline]
    pub fn set_ch(&mut self, ch: char) {
        self.code = (self.code & !CHAR_MASK) | ch as u32;
    }

    #[inline]
    pub fn flags(&self) -> CellFlags {
        CellFlags::from_bits_retain((self.code >> FLAGS_SHIFT) as u8)
    }

    #[inline]
    pub fn set_flags(&mut self, flags: CellFlags) {
        self.code = (self.code & CHAR_MASK) | (flags.bits() as u32) << FLAGS_SHIFT;
    }

    pub fn insert_flags(&mut self, flags: CellFlags) {
        self.set_flags(self.flags() | flags);
    }

    /// Combining marks / further code points of the grapheme cluster.
    pub fn extra(&self) -> Option<&str> {
        self.text.as_deref().map(|t| &*t.extra).filter(|s| !s.is_empty())
    }

    /// Zero-width bidi formatting at the start of a row, before this character.
    pub fn prefix(&self) -> Option<&str> {
        self.text.as_deref().map(|t| &*t.prefix).filter(|s| !s.is_empty())
    }

    /// Whether the cell carries text beyond its base character.
    #[inline]
    pub fn has_text(&self) -> bool {
        self.text.is_some()
    }

    pub fn take_prefix(&mut self) -> Option<Box<str>> {
        let text = self.text.as_mut()?;
        let prefix = std::mem::take(&mut text.prefix);
        if text.extra.is_empty() {
            self.text = None;
        }
        (!prefix.is_empty()).then_some(prefix)
    }

    pub fn set_prefix(&mut self, prefix: Option<Box<str>>) {
        match prefix.filter(|p| !p.is_empty()) {
            Some(p) => self.text.get_or_insert_default().prefix = p,
            None => {
                self.take_prefix();
            }
        }
    }

    /// Blank with default style: carries no information and can be trimmed.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.code == ' ' as u32 && self.style == StyleId::DEFAULT && self.text.is_none()
    }

    pub fn is_spacer(&self) -> bool {
        self.flags().intersects(CellFlags::WIDE_SPACER | CellFlags::LEADING_WIDE_SPACER)
    }

    /// Display width of this cell (0 for spacers).
    pub fn width(&self) -> usize {
        let flags = self.flags();
        if flags.contains(CellFlags::WIDE) {
            2
        } else if flags.intersects(CellFlags::WIDE_SPACER | CellFlags::LEADING_WIDE_SPACER) {
            0
        } else {
            1
        }
    }

    /// Append the cell's text (base char + extras) to `out`.
    pub fn push_text(&self, out: &mut String) {
        match self.text.as_deref() {
            None => out.push(self.ch()),
            Some(t) => {
                out.push_str(&t.prefix);
                out.push(self.ch());
                out.push_str(&t.extra);
            }
        }
    }

    pub fn push_extra(&mut self, c: char) {
        let text = self.text.get_or_insert_default();
        let mut s = String::from(std::mem::take(&mut text.extra));
        s.push(c);
        text.extra = s.into_boxed_str();
    }

    pub fn push_prefix(&mut self, c: char) {
        let text = self.text.get_or_insert_default();
        let mut s = String::from(std::mem::take(&mut text.prefix));
        s.push(c);
        text.prefix = s.into_boxed_str();
    }
}

/// A row of cells. `wrapped` means the logical line continues on the next row.
#[derive(Debug, Clone)]
pub struct Row {
    cells: Vec<Cell>,
    pub wrapped: bool,
    /// Every cell from this index on is empty ([`Cell::is_empty`]), so
    /// per-line work only touches the written part of the row. Cells change
    /// only through `Row` methods, which keep it up to date.
    blank_from: usize,
}

impl PartialEq for Row {
    fn eq(&self, other: &Self) -> bool {
        self.cells == other.cells && self.wrapped == other.wrapped
    }
}

impl Eq for Row {}

impl Row {
    pub fn new(cols: usize, style: StyleId) -> Self {
        let blank_from = if style == StyleId::DEFAULT { 0 } else { cols };
        Self { cells: vec![Cell::blank(style); cols], wrapped: false, blank_from }
    }

    pub(crate) fn from_cells(cells: Vec<Cell>, wrapped: bool) -> Self {
        Self { blank_from: cells.len(), cells, wrapped }
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// All cells for writing.
    pub(crate) fn cells_mut(&mut self) -> &mut [Cell] {
        self.cells_to(self.cells.len())
    }

    pub(crate) fn cell_mut(&mut self, x: usize) -> &mut Cell {
        &mut self.cells_to(x + 1)[x]
    }

    pub(crate) fn into_cells(self) -> Vec<Cell> {
        self.cells
    }

    /// Move the cells out, leaving the row without any.
    pub(crate) fn take_cells(&mut self) -> Vec<Cell> {
        self.blank_from = 0;
        std::mem::take(&mut self.cells)
    }

    /// Pad with empty cells, or cut, to `cols` cells.
    pub(crate) fn pad_to(&mut self, cols: usize) {
        self.cells.resize(cols, Cell::default());
        self.blank_from = self.blank_from.min(cols);
    }

    /// Give back memory when a quarter or more of the allocation is unused.
    pub(crate) fn compact(&mut self) {
        if self.cells.len() * 4 <= self.cells.capacity() * 3 {
            self.cells.shrink_to_fit();
        }
    }

    /// The cells before `end` for writing; the cells from `end` on stay as they are.
    pub(crate) fn cells_to(&mut self, end: usize) -> &mut [Cell] {
        let end = end.min(self.cells.len());
        if end > self.blank_from {
            self.blank_from = end;
        }
        &mut self.cells[..end]
    }

    /// End of the part of the row that may hold content.
    fn written(&self) -> usize {
        let end = self.blank_from.min(self.cells.len());
        debug_assert!(self.cells[end..].iter().all(Cell::is_empty), "cells past blank_from must be empty");
        end
    }

    /// Reset every cell to blank with the given style.
    pub fn clear(&mut self, style: StyleId) {
        let end = if style == StyleId::DEFAULT { self.written() } else { self.cells.len() };
        self.cells[..end].fill(Cell::blank(style));
        self.blank_from = if style == StyleId::DEFAULT { 0 } else { self.cells.len() };
        self.wrapped = false;
    }

    /// Blank `range` with `style`.
    pub(crate) fn erase(&mut self, range: std::ops::Range<usize>, style: StyleId) {
        let range = range.start.min(self.cells.len())..range.end.min(self.cells.len());
        self.cells[range.clone()].fill(Cell::blank(style));
        if style != StyleId::DEFAULT {
            self.blank_from = self.blank_from.max(range.end);
        } else if range.end >= self.blank_from {
            self.blank_from = self.blank_from.min(range.start);
        }
    }

    /// Make this a blank row of `cols` cells, reusing its allocation.
    pub fn reset(&mut self, cols: usize, style: StyleId) {
        if style == StyleId::DEFAULT {
            let end = self.written().min(cols);
            self.cells[..end].fill(Cell::default());
            self.cells.resize(cols, Cell::default());
            self.blank_from = 0;
        } else {
            self.cells.clear();
            self.cells.resize(cols, Cell::blank(style));
            self.blank_from = cols;
        }
        self.wrapped = false;
    }

    /// The row moves into history: when that saves memory, it keeps only
    /// its content, in `spare`'s allocation if one is given. Returns the
    /// full-width row it gave up, now blank, for reuse. Soft-wrapped rows
    /// keep their width: their trailing blanks are text.
    pub(crate) fn archive(&mut self, spare: &mut Option<Vec<Cell>>) -> Option<Row> {
        if self.wrapped {
            return None;
        }
        let len = self.content_len();
        if len * 4 > self.cells.len() * 3 {
            return None;
        }
        let mut kept = match spare.take() {
            Some(mut v) => {
                v.clear();
                // Rows in history should not hold much more memory than they use.
                if v.capacity() > len + len / 2 + 8 {
                    v.shrink_to(len);
                }
                v.reserve_exact(len);
                v
            }
            None => Vec::with_capacity(len),
        };
        kept.extend(self.cells[..len].iter_mut().map(std::mem::take));
        let full = std::mem::replace(&mut self.cells, kept);
        self.blank_from = len;
        Some(Row { cells: full, wrapped: false, blank_from: 0 })
    }

    /// Number of cells up to and including the last non-empty one.
    pub fn content_len(&self) -> usize {
        self.cells[..self.written()].iter().rposition(|c| !c.is_empty()).map_or(0, |i| i + 1)
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
            self.blank_from = self.blank_from.min(cols);
            if let Some(last) = self.cells.last_mut()
                && last.flags().contains(CellFlags::WIDE)
            {
                *last = Cell::blank(last.style);
            }
        } else if cols > self.cells.len() {
            // A leading spacer is only meaningful in the last column.
            if let Some(last) = self.cells.last_mut()
                && last.flags().contains(CellFlags::LEADING_WIDE_SPACER)
            {
                *last = Cell::blank(last.style);
            }
            self.blank_from = self.written();
            self.cells.resize(cols, Cell::blank(style));
            if style != StyleId::DEFAULT {
                self.blank_from = cols;
            }
        }
    }
}
