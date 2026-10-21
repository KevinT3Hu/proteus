//! What each row of a pane shows: a grid line, or the one-line marker of a
//! folded command output. Without folds this is the plain viewport.
//!
//! Folding only compresses: the viewport's last line stays at the bottom
//! and more history shows above, so everything the unfolded viewport shows
//! stays visible (lines inside a fold are "on" its marker row). When the
//! history runs out, rows start at the oldest line instead.

use std::ops::Range;

use proteus_core::blocks::{BlockId, BlockState};
use proteus_core::{Line, Terminal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Line(Line),
    /// Folded output of `block`: lines `start..end`.
    Fold {
        block: BlockId,
        start: Line,
        end: Line,
    },
    /// Past the last line (short history with folds).
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rows(pub Vec<Row>);

/// Output ranges of folded, finished blocks (primary screen), in order.
/// A range never reaches the next prompt.
pub fn folds(t: &Terminal) -> Vec<(BlockId, Range<Line>)> {
    if t.is_alt_screen() {
        return Vec::new();
    }
    let blocks: Vec<_> = t.blocks().iter().collect();
    let live_end = t.grid().end_line();
    let mut out = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        if !b.folded || b.state != BlockState::Finished {
            continue;
        }
        let Some(mut r) = b.output_lines(live_end) else {
            continue;
        };
        if let Some(next) = blocks.get(i + 1) {
            r.end = r.end.min(next.prompt_line);
        }
        if !r.is_empty() {
            out.push((b.id, r));
        }
    }
    out
}

impl Rows {
    pub fn of(t: &Terminal) -> Rows {
        let g = t.grid();
        let n = t.lines();
        let folds = folds(t);
        if folds.is_empty() {
            return Rows((0..n).map(|y| Row::Line(g.viewport_line(y))).collect());
        }
        let fold_at = |l: Line| folds.iter().find(|f| f.1.contains(&l));
        let first = g.first_line();
        let mut rows = Vec::with_capacity(n);
        let mut cur = Some(g.viewport_line(n - 1));
        while rows.len() < n {
            let Some(l) = cur else { break };
            let start = match fold_at(l) {
                Some((block, r)) => {
                    rows.push(Row::Fold {
                        block: *block,
                        start: r.start,
                        end: r.end,
                    });
                    r.start
                }
                None => {
                    rows.push(Row::Line(l));
                    l
                }
            };
            cur = start.checked_sub(1).filter(|&p| p >= first);
        }
        if rows.len() == n {
            rows.reverse();
            return Rows(rows);
        }
        // Not enough history to fill the pane: start at the oldest line.
        rows.clear();
        let end = g.end_line();
        let mut l = first;
        while rows.len() < n && l < end {
            match fold_at(l) {
                Some((block, r)) => {
                    rows.push(Row::Fold {
                        block: *block,
                        start: r.start,
                        end: r.end,
                    });
                    l = r.end;
                }
                None => {
                    rows.push(Row::Line(l));
                    l += 1;
                }
            }
        }
        rows.resize(n, Row::Empty);
        Rows(rows)
    }

    /// Grid line of row `y` (a fold's first line), if any.
    pub fn line(&self, y: usize) -> Option<Line> {
        match self.0.get(y)? {
            Row::Line(l) => Some(*l),
            Row::Fold { start, .. } => Some(*start),
            Row::Empty => None,
        }
    }

    /// Row showing `line` (the fold marker for folded lines).
    pub fn row_of(&self, line: Line) -> Option<usize> {
        self.0.iter().position(|r| match *r {
            Row::Line(l) => l == line,
            Row::Fold { start, end, .. } => (start..end).contains(&line),
            Row::Empty => false,
        })
    }

    /// Last row showing a grid line.
    pub fn last_line(&self) -> Option<Line> {
        (0..self.0.len()).rev().find_map(|y| self.line(y))
    }

    /// Lines shown as themselves (not folded), oldest first.
    pub fn visible_lines(&self) -> impl Iterator<Item = Line> + '_ {
        self.0.iter().filter_map(|r| match r {
            Row::Line(l) => Some(*l),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A terminal with `blocks` commands each printing `out` lines.
    fn term(rows: usize, blocks: usize, out: usize) -> Terminal {
        let mut t = Terminal::new(20, rows, proteus_core::Config::default());
        for b in 0..blocks {
            let mut s = format!("\x1b]133;A\x07$ \x1b]133;B\x07cmd{b}\r\n\x1b]133;C\x07");
            for i in 0..out {
                s.push_str(&format!("o{b}.{i}\r\n"));
            }
            s.push_str("\x1b]133;D;0\x07");
            t.advance(s.as_bytes());
        }
        t.advance(b"\x1b]133;A\x07$ ");
        t
    }

    #[test]
    fn identity_without_folds() {
        let t = term(5, 3, 3);
        let r = Rows::of(&t);
        let g = t.grid();
        assert_eq!(
            r.0,
            (0..5)
                .map(|y| Row::Line(g.viewport_line(y)))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn fold_compresses_from_the_bottom() {
        let mut t = term(5, 3, 3);
        let last_out = t.blocks().iter().nth(2).unwrap().clone();
        assert!(t.set_block_folded(last_out.id, true));
        let r = Rows::of(&t);
        let bottom = t.grid().viewport_line(4);
        // Bottom: the prompt; above it the fold of 3 lines; above that more history.
        assert_eq!(r.0[4], Row::Line(bottom));
        let out = last_out.output_start.unwrap();
        assert_eq!(
            r.0[3],
            Row::Fold {
                block: last_out.id,
                start: out,
                end: out + 3
            }
        );
        assert_eq!(r.0[2], Row::Line(out - 1));
        assert_eq!(r.row_of(out + 1), Some(3));
        assert_eq!(r.line(3), Some(out));
        assert_eq!(r.last_line(), Some(bottom));
        assert!(!r.visible_lines().any(|l| (out..out + 3).contains(&l)));
    }

    #[test]
    fn short_history_starts_at_the_top() {
        let mut t = term(12, 1, 3);
        let b = t.blocks().iter().next().unwrap().clone();
        t.set_block_folded(b.id, true);
        let r = Rows::of(&t);
        let first = t.grid().first_line();
        assert_eq!(r.0[0], Row::Line(first));
        assert!(matches!(r.0[1], Row::Fold { .. }));
        assert_eq!(r.0[2], Row::Line(first + 4));
        assert_eq!(*r.0.last().unwrap(), Row::Empty);
    }
}
