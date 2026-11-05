//! Command blocks built from OSC 133 shell-integration marks.
//!
//! A block spans from its prompt (`A`) through the command input (`B`), the
//! command output (`C`) up to the end mark (`D`). Positions are absolute grid
//! lines so they stay valid while output scrolls into history.

use std::collections::VecDeque;
use std::ops::Range;
use std::time::{Duration, Instant};

use crate::grid::{Line, LineMap};

/// A position in the grid: absolute line + column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Point {
    pub line: Line,
    pub col: usize,
}

impl Point {
    pub const fn new(line: Line, col: usize) -> Self {
        Self { line, col }
    }
}

pub type BlockId = u64;

/// Most blocks kept per screen; the oldest are forgotten first.
pub const MAX_BLOCKS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockState {
    /// Prompt is being drawn (after `A`).
    Prompt,
    /// User is typing the command (after `B`).
    Input,
    /// Command is executing (after `C`).
    Running,
    /// Done (after `D`, or superseded by the next prompt).
    Finished,
}

#[derive(Debug, Clone)]
pub struct Block {
    pub id: BlockId,
    pub state: BlockState,
    pub prompt_line: Line,
    pub input_start: Option<Point>,
    pub output_start: Option<Line>,
    /// Cursor position at `D`: output ends before this point.
    pub end: Option<Point>,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub exit_code: Option<i32>,
    pub started_at: Option<Instant>,
    pub finished_at: Option<Instant>,
    pub folded: bool,
}

impl Block {
    fn new(id: BlockId, prompt_line: Line) -> Self {
        Self {
            id,
            state: BlockState::Prompt,
            prompt_line,
            input_start: None,
            output_start: None,
            end: None,
            command: None,
            cwd: None,
            exit_code: None,
            started_at: None,
            finished_at: None,
            folded: false,
        }
    }

    /// Execution time; for a running block, the time so far.
    pub fn duration(&self) -> Option<Duration> {
        let start = self.started_at?;
        Some(self.finished_at.unwrap_or_else(Instant::now).duration_since(start))
    }

    pub fn failed(&self) -> bool {
        matches!(self.exit_code, Some(c) if c != 0)
    }

    /// Lines holding the command output. `live_end` is used for running blocks
    /// (normally the cursor line + 1).
    pub fn output_lines(&self, live_end: Line) -> Option<Range<Line>> {
        let start = self.output_start?;
        let end = match self.end {
            Some(p) if p.col == 0 => p.line,
            Some(p) => p.line + 1,
            None => live_end,
        };
        Some(start..end.max(start))
    }
}

#[derive(Debug, Default)]
pub struct Blocks {
    list: VecDeque<Block>,
    next_id: BlockId,
}

impl Blocks {
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Block> {
        self.list.iter()
    }
    pub fn len(&self) -> usize {
        self.list.len()
    }
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
    pub fn get(&self, id: BlockId) -> Option<&Block> {
        self.list.iter().find(|b| b.id == id)
    }
    pub fn get_mut(&mut self, id: BlockId) -> Option<&mut Block> {
        self.list.iter_mut().find(|b| b.id == id)
    }
    pub fn last(&self) -> Option<&Block> {
        self.list.back()
    }
    pub(crate) fn last_mut(&mut self) -> Option<&mut Block> {
        self.list.back_mut()
    }

    /// Start a new block at `line`. A previous block still at its prompt is
    /// finished (or replaced when the prompt was redrawn on the same line).
    pub(crate) fn start(&mut self, line: Line) -> &mut Block {
        if let Some(last) = self.list.back_mut()
            && matches!(last.state, BlockState::Prompt | BlockState::Input)
        {
            if last.prompt_line >= line {
                self.list.pop_back();
            } else {
                last.state = BlockState::Finished;
            }
        }
        // Drop blocks whose prompt is at or below the new one (screen was cleared/redrawn).
        while self.list.back().is_some_and(|b| b.prompt_line >= line) {
            self.list.pop_back();
        }
        let id = self.next_id;
        self.next_id += 1;
        self.list.push_back(Block::new(id, line));
        if self.list.len() > MAX_BLOCKS {
            self.list.pop_front();
        }
        self.list.back_mut().expect("just pushed")
    }

    /// The block containing `line` (from its prompt up to the next prompt).
    pub fn at_line(&self, line: Line) -> Option<&Block> {
        let idx = self.list.partition_point(|b| b.prompt_line <= line);
        idx.checked_sub(1).map(|i| &self.list[i])
    }

    /// Prompt line of the closest block starting before `line`.
    pub fn prev_prompt(&self, line: Line) -> Option<Line> {
        self.list.iter().rev().map(|b| b.prompt_line).find(|&l| l < line)
    }

    /// Prompt line of the closest block starting after `line`.
    pub fn next_prompt(&self, line: Line) -> Option<Line> {
        self.list.iter().map(|b| b.prompt_line).find(|&l| l > line)
    }

    /// Forget blocks entirely before `first_line` (dropped from scrollback);
    /// clamp the ones that are partially gone.
    pub(crate) fn prune(&mut self, first_line: Line) {
        while let Some(b) = self.list.front() {
            // A block finished without `D` lasts until the next prompt.
            let next = self.list.get(1).map_or(Line::MAX, |n| n.prompt_line);
            let end = b.end.map_or(next, |p| p.line + 1);
            if end <= first_line && b.state == BlockState::Finished {
                self.list.pop_front();
            } else {
                break;
            }
        }
        for b in &mut self.list {
            if b.prompt_line >= first_line {
                break;
            }
            b.prompt_line = first_line;
            b.input_start = b.input_start.map(|p| p.max(Point::new(first_line, 0)));
            b.output_start = b.output_start.map(|l| l.max(first_line));
        }
    }

    /// Rewrite positions after a reflow; blocks whose prompt vanished are dropped.
    pub(crate) fn remap(&mut self, map: &LineMap) {
        self.list.retain_mut(|b| {
            let Some(p) = map.map(b.prompt_line) else { return false };
            b.prompt_line = p;
            b.input_start = b.input_start.and_then(|pt| map.map(pt.line).map(|l| Point::new(l, pt.col)));
            b.output_start = b.output_start.and_then(|l| map.map(l));
            // Columns cannot be mapped exactly; snap the end to a line boundary.
            b.end = b.end.and_then(|pt| {
                let l = map.map(pt.line)?;
                Some(if pt.col == 0 { Point::new(l, 0) } else { Point::new(l + 1, 0) })
            });
            true
        });
    }

    pub fn set_folded(&mut self, id: BlockId, folded: bool) -> bool {
        match self.get_mut(id) {
            Some(b) if b.output_start.is_some() => {
                b.folded = folded;
                true
            }
            _ => false,
        }
    }
}
