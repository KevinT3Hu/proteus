//! Incremental scrollback search over logical (unwrapped) lines.

use std::ops::Range;

use regex::{Regex, RegexBuilder};

use crate::blocks::Point;
use crate::grid::{Grid, Line};
use crate::text;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaseMode {
    /// Case-insensitive unless the query contains an uppercase letter.
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub regex: bool,
    pub case: CaseMode,
}

/// A match; both ends inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    pub start: Point,
    pub end: Point,
}

#[derive(Debug, Clone)]
pub struct Searcher {
    re: Regex,
}

impl Searcher {
    pub fn new(query: &str, opts: &SearchOptions) -> Result<Self, regex::Error> {
        let pattern = if opts.regex { query.to_string() } else { regex::escape(query) };
        let insensitive = match opts.case {
            CaseMode::Smart => !query.chars().any(char::is_uppercase),
            CaseMode::Sensitive => false,
            CaseMode::Insensitive => true,
        };
        let re = RegexBuilder::new(&pattern).case_insensitive(insensitive).size_limit(1 << 22).build()?;
        Ok(Self { re })
    }


    /// Matches in logical lines that intersect `lines`, in order.
    pub fn find(&self, grid: &Grid, lines: Range<Line>) -> Vec<Match> {
        let mut out = Vec::new();
        let end = lines.end.min(grid.end_line());
        let mut l = text::logical_start(grid, lines.start.max(grid.first_line()));
        while l < end {
            let ll = text::logical_line(grid, l);
            for m in self.re.find_iter(&ll.text) {
                if m.is_empty() {
                    continue;
                }
                if let (Some(start), Some(end)) = (ll.point_at(m.start()), ll.point_before(m.end())) {
                    out.push(Match { start, end });
                }
            }
            l = ll.end.max(l + 1);
        }
        out
    }

    pub fn find_all(&self, grid: &Grid) -> Vec<Match> {
        self.find(grid, grid.first_line()..grid.end_line())
    }

    /// Matches restricted to the given line ranges (e.g. block outputs).
    pub fn find_in_ranges(&self, grid: &Grid, ranges: &[Range<Line>]) -> Vec<Match> {
        let mut out: Vec<Match> = ranges
            .iter()
            .flat_map(|r| self.find(grid, r.clone()).into_iter().filter(move |m| r.contains(&m.start.line)))
            .collect();
        out.sort_by_key(|m| m.start);
        out.dedup();
        out
    }
}

/// Index of the next (`forward`) or previous match relative to `from`, wrapping around.
pub fn nearest(matches: &[Match], from: Point, forward: bool) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    if forward {
        Some(matches.iter().position(|m| m.start > from).unwrap_or(0))
    } else {
        Some(matches.iter().rposition(|m| m.start < from).unwrap_or(matches.len() - 1))
    }
}
