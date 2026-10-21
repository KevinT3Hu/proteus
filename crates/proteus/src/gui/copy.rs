//! Scroll-mode cursor movement over the grid (vi-style motions).

use proteus_core::blocks::Blocks;
use proteus_core::cell::CellFlags;
use proteus_core::{Grid, Line, Point};

use crate::modes::Motion;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(grid: &Grid, p: Point) -> Class {
    let Some(row) = grid.abs_row(p.line) else { return Class::Space };
    let Some(mut cell) = row.cells.get(p.col) else { return Class::Space };
    if cell.flags.contains(CellFlags::WIDE_SPACER) && p.col > 0 {
        cell = &row.cells[p.col - 1];
    }
    match cell.ch {
        ' ' | '\t' | '\0' => Class::Space,
        c if c.is_alphanumeric() || c == '_' => Class::Word,
        _ => Class::Punct,
    }
}

/// Next cell in reading order (rows joined; a row end acts as a space).
fn next(grid: &Grid, p: Point) -> Option<Point> {
    if p.col + 1 < grid.cols() {
        Some(Point::new(p.line, p.col + 1))
    } else if p.line + 1 < grid.end_line() {
        Some(Point::new(p.line + 1, 0))
    } else {
        None
    }
}

fn prev(grid: &Grid, p: Point) -> Option<Point> {
    if p.col > 0 {
        Some(Point::new(p.line, p.col - 1))
    } else if p.line > grid.first_line() {
        Some(Point::new(p.line - 1, grid.cols() - 1))
    } else {
        None
    }
}


fn word_next(grid: &Grid, p: Point) -> Point {
    let start = class(grid, p);
    let mut q = p;
    // Leave the current word (a row change ends it).
    if start != Class::Space {
        while let Some(n) = next(grid, q) {
            if n.line != q.line || class(grid, n) != start {
                q = n;
                break;
            }
            q = n;
        }
    }
    while class(grid, q) == Class::Space {
        match next(grid, q) {
            Some(n) => q = n,
            None => return q,
        }
    }
    q
}

fn word_prev(grid: &Grid, p: Point) -> Point {
    let mut q = p;
    loop {
        match prev(grid, q) {
            Some(n) => q = n,
            None => return q,
        }
        if class(grid, q) != Class::Space {
            break;
        }
    }
    let c = class(grid, q);
    while let Some(n) = prev(grid, q).filter(|n| n.line == q.line && class(grid, *n) == c) {
        q = n;
    }
    q
}

fn word_end(grid: &Grid, p: Point) -> Point {
    let mut q = match next(grid, p) {
        Some(n) => n,
        None => return p,
    };
    while class(grid, q) == Class::Space {
        match next(grid, q) {
            Some(n) => q = n,
            None => return q,
        }
    }
    let c = class(grid, q);
    while let Some(n) = next(grid, q).filter(|n| n.line == q.line && class(grid, *n) == c) {
        q = n;
    }
    q
}

/// Last non-blank column of a row.
fn line_end(grid: &Grid, line: Line) -> usize {
    let Some(row) = grid.abs_row(line) else { return 0 };
    row.cells.iter().rposition(|c| !matches!(c.ch, ' ' | '\0') && !c.flags.contains(CellFlags::WIDE_SPACER)).unwrap_or(0)
}

/// Clamp a point to stored lines and columns.
pub fn clamp(grid: &Grid, p: Point) -> Point {
    let line = p.line.clamp(grid.first_line(), grid.end_line().saturating_sub(1));
    Point::new(line, p.col.min(grid.cols() - 1))
}

/// Where `m` moves the cursor from `p`. `top` is the first visible line.
/// Page motions return the new cursor; the caller scrolls to reveal it.
pub fn apply(grid: &Grid, blocks: Option<&Blocks>, p: Point, m: Motion, top: Line, lines: usize) -> Point {
    let p = clamp(grid, p);
    let shift = |n: isize| {
        let line = (p.line as i64 + n as i64).max(0) as Line;
        clamp(grid, Point::new(line, p.col))
    };
    let half = (lines / 2).max(1) as isize;
    let page = (lines.saturating_sub(1)).max(1) as isize;
    let last_visible = top + lines as Line - 1;
    let q = match m {
        Motion::Left => Point::new(p.line, p.col.saturating_sub(1)),
        Motion::Right => Point::new(p.line, p.col + 1),
        Motion::Up => shift(-1),
        Motion::Down => shift(1),
        Motion::WordNext => word_next(grid, p),
        Motion::WordPrev => word_prev(grid, p),
        Motion::WordEnd => word_end(grid, p),
        Motion::LineStart => Point::new(p.line, 0),
        Motion::LineEnd => Point::new(p.line, line_end(grid, p.line)),
        Motion::Top => Point::new(grid.first_line(), 0),
        Motion::Bottom => Point::new(grid.end_line().saturating_sub(1), 0),
        Motion::HalfPageUp => shift(-half),
        Motion::HalfPageDown => shift(half),
        Motion::PageUp => shift(-page),
        Motion::PageDown => shift(page),
        Motion::ScreenTop => Point::new(top, p.col),
        Motion::ScreenMiddle => Point::new(top + (lines / 2) as Line, p.col),
        Motion::ScreenBottom => Point::new(last_visible, p.col),
        Motion::PrevPrompt => match blocks.and_then(|b| b.prev_prompt(p.line)) {
            Some(l) => Point::new(l, 0),
            None => p,
        },
        Motion::NextPrompt => match blocks.and_then(|b| b.next_prompt(p.line)) {
            Some(l) => Point::new(l, 0),
            None => p,
        },
    };
    clamp(grid, q)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proteus_core::Terminal;

    fn term(text: &str) -> Terminal {
        let mut t = Terminal::new(20, 4, proteus_core::Config::default());
        t.advance(text.replace('\n', "\r\n").as_bytes());
        t
    }

    #[test]
    fn words_and_lines() {
        let t = term("foo bar.baz\n  qux");
        let g = t.grid();
        let l0 = g.abs_line(0);
        let at = |c| Point::new(l0, c);
        let mv = |p, m| apply(g, None, p, m, l0, 4);
        assert_eq!(mv(at(0), Motion::WordNext), at(4));
        assert_eq!(mv(at(4), Motion::WordNext), at(7)); // `.`
        assert_eq!(mv(at(7), Motion::WordNext), at(8));
        assert_eq!(mv(at(8), Motion::WordNext), Point::new(l0 + 1, 2)); // next row
        assert_eq!(mv(Point::new(l0 + 1, 2), Motion::WordPrev), at(8));
        assert_eq!(mv(at(9), Motion::WordPrev), at(8));
        assert_eq!(mv(at(0), Motion::WordEnd), at(2));
        assert_eq!(mv(at(2), Motion::WordEnd), at(6));
        assert_eq!(mv(at(3), Motion::LineEnd), at(10));
        assert_eq!(mv(at(3), Motion::LineStart), at(0));
        assert_eq!(mv(at(0), Motion::Left), at(0));
        assert_eq!(mv(at(19), Motion::Right), at(19));
        assert_eq!(mv(at(5), Motion::Up), at(5));
        assert_eq!(mv(at(5), Motion::Bottom), Point::new(l0 + 3, 0));
        assert_eq!(mv(at(5), Motion::ScreenMiddle), Point::new(l0 + 2, 5));
        assert_eq!(mv(Point::new(l0 + 9, 30), Motion::Left), Point::new(l0 + 3, 18));
    }
}
