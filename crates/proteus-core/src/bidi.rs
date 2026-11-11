//! Unicode bidi presentation over immutable logical terminal cells.
use crate::{Grid, cell::CellFlags, grid::Line};
use std::{collections::HashMap, sync::Arc};
use unicode_bidi::{BidiInfo, Level};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// Reorder the primary screen; alternate-screen applications keep their own layout.
    #[default]
    Auto,
    On,
    Off,
}
impl Mode {
    pub fn enabled(self, alternate: bool) -> bool {
        match self {
            Self::Auto => !alternate,
            Self::On => true,
            Self::Off => false,
        }
    }
}
pub fn is_control(c: char) -> bool {
    matches!(c,'\u{061c}'|'\u{200e}'|'\u{200f}'|'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')
}

#[derive(Debug, Default)]
pub struct RowLayout {
    visual: Vec<usize>,
    logical: Vec<usize>,
    levels: Vec<u8>,
}
impl RowLayout {
    pub fn visual(&self, logical: usize) -> usize {
        self.visual.get(logical).copied().unwrap_or(logical)
    }
    pub fn logical(&self, visual: usize) -> usize {
        self.logical.get(visual).copied().unwrap_or(visual)
    }
    pub fn rtl(&self, logical: usize) -> bool {
        self.levels.get(logical).copied().unwrap_or(0) % 2 == 1
    }
    pub fn has_rtl(&self) -> bool {
        !self.levels.is_empty()
    }
}

/// Characters that can give text a right-to-left level in a left-to-right
/// paragraph: strong R/AL, Arabic digits and RTL formatting characters.
/// Text without any of them always lays out in logical order.
fn may_be_rtl(c: char) -> bool {
    matches!(c,
        '\u{0590}'..='\u{08ff}'
        | '\u{fb1d}'..='\u{fdff}'
        | '\u{fe70}'..='\u{feff}'
        | '\u{10800}'..='\u{10fff}'
        | '\u{1e800}'..='\u{1efff}'
        | '\u{200f}' | '\u{202b}' | '\u{202e}' | '\u{2067}' | '\u{2068}')
}

fn row_may_be_rtl(row: &crate::cell::Row) -> bool {
    row.cells.iter().any(|c| {
        !c.ch().is_ascii() && may_be_rtl(c.ch())
            || c.has_text() && c.extra().into_iter().chain(c.prefix()).flat_map(str::chars).any(may_be_rtl)
    })
}

/// Soft-wrapped lines longer than this many rows are laid out in blocks
/// of this size, so a huge wrapped line costs a bounded amount per frame.
const CHUNK_ROWS: Line = 128;
/// Bound on cached row layouts before the cache is reset.
const CACHE_ROWS: usize = 4096;

struct Atom {
    col: usize,
    width: usize,
    byte: usize,
}
struct Row {
    line: Line,
    start: usize,
    end: usize,
    atoms: Vec<Atom>,
}
#[derive(Default)]
pub(crate) struct Cache {
    rows: HashMap<Line, Arc<RowLayout>>,
    identity: Arc<RowLayout>,
}
impl Cache {
    pub fn clear(&mut self) {
        self.rows.clear();
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
    /// The shared layout of rows shown in logical order.
    pub fn identity(&self) -> Arc<RowLayout> {
        Arc::clone(&self.identity)
    }
    /// First line of the layout block containing `line`.
    pub fn block_start(grid: &Grid, line: Line) -> Line {
        let mut first = line;
        while first > grid.first_line() && grid.abs_row(first - 1).is_some_and(|r| r.wrapped) {
            first -= 1;
        }
        first + (line - first) / CHUNK_ROWS * CHUNK_ROWS
    }
    /// Forget layouts of `from` and later lines (their content may change).
    pub fn invalidate_from(&mut self, from: Line) {
        self.rows.retain(|&l, _| l < from);
    }
    pub fn row(&mut self, grid: &Grid, line: Line) -> Arc<RowLayout> {
        if let Some(row) = self.rows.get(&line) {
            return row.clone();
        }
        if grid.abs_row(line).is_none() {
            return self.identity();
        }
        let first = Self::block_start(grid, line);
        let mut last = first;
        while last + 1 < first + CHUNK_ROWS
            && grid.abs_row(last).is_some_and(|r| r.wrapped)
            && grid.abs_row(last + 1).is_some()
        {
            last += 1;
        }
        if self.rows.len() + (last - first + 1) as usize > CACHE_ROWS {
            self.rows.clear();
        }
        // Most output never needs bidi, including ordinary full-screen applications.
        if !(first..=last).any(|l| grid.abs_row(l).is_some_and(row_may_be_rtl)) {
            for l in first..=last {
                self.rows.insert(l, self.identity());
            }
            return self.identity();
        }
        let mut text = String::new();
        let mut rows = Vec::new();
        for line in first..=last {
            let row = grid.abs_row(line).unwrap();
            let start = text.len();
            let mut atoms = Vec::new();
            for (col, cell) in row.cells.iter().enumerate() {
                if cell.flags().contains(CellFlags::WIDE_SPACER) {
                    continue;
                }
                if let Some(prefix) = cell.prefix() {
                    text.push_str(prefix);
                }
                let byte = text.len();
                text.push(cell.ch());
                if let Some(extra) = cell.extra() {
                    text.push_str(extra);
                }
                atoms.push(Atom { col, width: cell.width().max(1), byte });
            }
            rows.push(Row { line, start, end: text.len(), atoms });
        }
        let info = BidiInfo::new(&text, Some(Level::ltr()));
        if !info.has_rtl() {
            for row in rows {
                self.rows.insert(row.line, self.identity());
            }
            return self.identity();
        }
        let mut maps = Vec::new();
        for row in rows {
            let mut levels = info.levels.clone();
            // Paragraph separators can be embedded in cells, so intersect each UBA paragraph.
            for paragraph in &info.paragraphs {
                let range = row.start.max(paragraph.range.start)..row.end.min(paragraph.range.end);
                if range.start < range.end {
                    let adjusted = info.reordered_levels(paragraph, range.clone());
                    levels[range.clone()].copy_from_slice(&adjusted[range]);
                }
            }
            let atom_levels: Vec<_> =
                row.atoms.iter().map(|a| levels.get(a.byte).copied().unwrap_or(Level::ltr())).collect();
            let mut map = RowLayout::default();
            if atom_levels.iter().any(|l| l.is_rtl()) {
                map.visual = (0..grid.cols()).collect();
                map.logical = (0..grid.cols()).collect();
                map.levels = vec![0; grid.cols()];
                let mut visual = 0;
                for index in BidiInfo::reorder_visual(&atom_levels) {
                    let atom = &row.atoms[index];
                    for offset in 0..atom.width.min(grid.cols() - atom.col) {
                        map.visual[atom.col + offset] = visual + offset;
                        map.logical[visual + offset] = atom.col;
                        map.levels[atom.col + offset] = atom_levels[index].number();
                    }
                    visual += atom.width;
                }
            }
            maps.push((row.line, Arc::new(map)));
        }
        self.rows.extend(maps);
        self.rows.get(&line).cloned().unwrap_or_else(|| self.identity())
    }
}

/// A grapheme in a standalone UI string, retaining its logical byte range.
#[derive(Debug)]
pub struct TextCell {
    pub text: String,
    pub start: usize,
    pub end: usize,
    pub logical: usize,
    pub visual: usize,
    pub width: usize,
    pub rtl: bool,
}
pub struct TextLayout {
    pub cells: Vec<TextCell>,
    pub width: usize,
}
impl TextLayout {
    pub fn new(text: &str, limit: usize) -> Self {
        use unicode_segmentation::UnicodeSegmentation;
        use unicode_width::UnicodeWidthStr;
        let mut shown = String::new();
        let mut width = 0;
        let total: usize =
            text.graphemes(true).map(|g| if g.chars().all(is_control) { 0 } else { g.width().clamp(1, 2) }).sum();
        let max = if total > limit { limit.saturating_sub(1) } else { limit };
        for g in text.graphemes(true) {
            let w = if g.chars().all(is_control) { 0 } else { g.width().clamp(1, 2) };
            if width + w > max {
                break;
            }
            shown.push_str(g);
            width += w;
        }
        if total > limit && limit > 0 {
            shown.push('…');
            width += 1;
        }
        let mut cells = Vec::new();
        let mut col = 0;
        for (start, g) in shown.grapheme_indices(true) {
            if g.chars().all(is_control) {
                continue;
            }
            let w = g.width().clamp(1, 2);
            cells.push(TextCell {
                text: g.into(),
                start,
                end: start + g.len(),
                logical: col,
                visual: col,
                width: w,
                rtl: false,
            });
            col += w;
        }
        let info = BidiInfo::new(&shown, Some(Level::ltr()));
        if info.has_rtl() {
            let mut levels = info.levels.clone();
            for para in &info.paragraphs {
                let adjusted = info.reordered_levels(para, para.range.clone());
                levels[para.range.clone()].copy_from_slice(&adjusted[para.range.clone()]);
            }
            let levels: Vec<_> = cells.iter().map(|c| levels[c.start]).collect();
            let mut col = 0;
            for i in BidiInfo::reorder_visual(&levels) {
                cells[i].visual = col;
                cells[i].rtl = levels[i].is_rtl();
                col += cells[i].width;
            }
        }
        Self { cells, width }
    }
    pub fn caret(&self, byte: usize) -> usize {
        if let Some(c) = self.cells.iter().find(|c| c.end > byte) {
            if c.rtl { c.visual + c.width } else { c.visual }
        } else if let Some(c) = self.cells.last() {
            if c.rtl { c.visual } else { c.visual + c.width }
        } else {
            0
        }
    }
}
