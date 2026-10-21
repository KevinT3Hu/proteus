//! Selections over the grid: normal, word, line, rectangular and semantic (block output).

use crate::blocks::{Blocks, Point};
use crate::cell::CellFlags;
use crate::grid::{Grid, Line};
use crate::text;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    Simple,
    Word,
    Line,
    /// Rectangular (Alt-drag, `Ctrl+v`).
    Block,
    /// The output of the command block under the anchor.
    Semantic,
}

/// A selection between an anchor and the moving end, in absolute grid points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub kind: SelectionKind,
    pub anchor: Point,
    pub cursor: Point,
}

/// Resolved selection; both ends inclusive. For `rect`, columns are
/// `start.col..=end.col` on every line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionRange {
    pub start: Point,
    pub end: Point,
    pub rect: bool,
}

impl SelectionRange {
    pub fn contains(&self, p: Point) -> bool {
        if p.line < self.start.line || p.line > self.end.line {
            return false;
        }
        if self.rect {
            return p.col >= self.start.col && p.col <= self.end.col;
        }
        (p.line > self.start.line || p.col >= self.start.col) && (p.line < self.end.line || p.col <= self.end.col)
    }
}


impl Selection {
    pub fn new(kind: SelectionKind, at: Point) -> Self {
        Self { kind, anchor: at, cursor: at }
    }

    pub fn update(&mut self, to: Point) {
        self.cursor = to;
    }

    /// Resolve against the grid. `blocks` is needed for semantic selection;
    /// `live_end` is the line after the cursor (end of a running block).
    pub fn range(&self, grid: &Grid, blocks: Option<&Blocks>, separators: &str, live_end: Line) -> Option<SelectionRange> {
        let (a, b) = if self.anchor <= self.cursor { (self.anchor, self.cursor) } else { (self.cursor, self.anchor) };
        let last_col = grid.cols() - 1;
        let r = match self.kind {
            SelectionKind::Simple => SelectionRange { start: a, end: b, rect: false },
            SelectionKind::Block => SelectionRange {
                start: Point::new(a.line, a.col.min(b.col)),
                end: Point::new(b.line, a.col.max(b.col)),
                rect: true,
            },
            SelectionKind::Word => SelectionRange {
                start: word_bounds(grid, a, separators).0,
                end: word_bounds(grid, b, separators).1,
                rect: false,
            },
            SelectionKind::Line => SelectionRange {
                start: Point::new(text::logical_start(grid, a.line), 0),
                end: Point::new(logical_end(grid, b.line), last_col),
                rect: false,
            },
            SelectionKind::Semantic => {
                let out = blocks?.at_line(self.anchor.line)?.output_lines(live_end)?;
                if out.is_empty() {
                    return None;
                }
                SelectionRange { start: Point::new(out.start, 0), end: Point::new(out.end - 1, last_col), rect: false }
            }
        };
        // Clamp to what is still stored.
        let first = grid.first_line();
        let last = grid.end_line().checked_sub(1)?;
        if r.end.line < first || r.start.line > last {
            return None;
        }
        let start = if r.start.line < first { Point::new(first, if r.rect { r.start.col } else { 0 }) } else { r.start };
        let end = if r.end.line > last { Point::new(last, if r.rect { r.end.col } else { last_col }) } else { r.end };
        Some(SelectionRange { start, end, rect: r.rect })
    }
}


/// Last row of the logical line containing `line`.
fn logical_end(grid: &Grid, mut line: Line) -> Line {
    while grid.abs_row(line).is_some_and(|r| r.wrapped) && grid.abs_row(line + 1).is_some() {
        line += 1;
    }
    line
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Class {
    Space,
    Separator,
    Word,
}

fn class(grid: &Grid, p: Point, separators: &str) -> Option<Class> {
    let row = grid.abs_row(p.line)?;
    let mut cell = row.cells.get(p.col)?;
    if cell.flags.contains(CellFlags::WIDE_SPACER) && p.col > 0 {
        cell = &row.cells[p.col - 1];
    }
    if cell.flags.contains(CellFlags::LEADING_WIDE_SPACER) {
        return Some(Class::Word);
    }
    Some(match cell.ch {
        ' ' | '\t' => Class::Space,
        c if separators.contains(c) => Class::Separator,
        _ => Class::Word,
    })
}

fn prev_point(grid: &Grid, p: Point) -> Option<Point> {
    if p.col > 0 {
        Some(Point::new(p.line, p.col - 1))
    } else if p.line > grid.first_line() && grid.abs_row(p.line - 1)?.wrapped {
        Some(Point::new(p.line - 1, grid.cols() - 1))
    } else {
        None
    }
}

fn next_point(grid: &Grid, p: Point) -> Option<Point> {
    if p.col + 1 < grid.cols() {
        Some(Point::new(p.line, p.col + 1))
    } else if grid.abs_row(p.line)?.wrapped && grid.abs_row(p.line + 1).is_some() {
        Some(Point::new(p.line + 1, 0))
    } else {
        None
    }
}

/// Extent of the word (or whitespace run) at `p`, following soft wraps. A
/// separator character is a word of its own.
pub fn word_bounds(grid: &Grid, p: Point, separators: &str) -> (Point, Point) {
    let Some(cls) = class(grid, p, separators) else { return (p, p) };
    if cls == Class::Separator {
        return (p, p);
    }
    let (mut start, mut end) = (p, p);
    while let Some(q) = prev_point(grid, start).filter(|&q| class(grid, q, separators) == Some(cls)) {
        start = q;
    }
    while let Some(q) = next_point(grid, end).filter(|&q| class(grid, q, separators) == Some(cls)) {
        end = q;
    }
    (start, end)
}
