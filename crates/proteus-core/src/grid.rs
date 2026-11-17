//! Screen grid with scrollback.
//!
//! All rows (history followed by the visible screen) live in one ring buffer.
//! Every row has a stable *absolute line number*: `dropped + index`, where
//! `dropped` counts rows that have fallen off the top of the scrollback. Marks
//! that must survive scrolling (blocks, selections) are stored as absolute lines.

use std::collections::VecDeque;

use crate::cell::{Cell, CellFlags, Row};
use crate::style::StyleId;

/// Absolute line number (stable while the row is in the grid).
pub type Line = u64;

#[derive(Debug, Clone)]
pub struct Grid {
    rows: VecDeque<Row>,
    cols: usize,
    lines: usize,
    max_scrollback: usize,
    dropped: Line,
    display_offset: usize,
}

/// Result of a reflowing resize.
#[derive(Debug)]
pub struct Reflow {
    pub cursor_x: usize,
    pub cursor_y: usize,
    pub pending_wrap: bool,
    pub line_map: LineMap,
}

/// Maps absolute line numbers from before a reflow to after it.
#[derive(Debug, Clone)]
pub struct LineMap {
    base: Line,
    map: Vec<usize>,
    valid: std::ops::Range<Line>,
}

impl LineMap {
    /// New absolute line of the row that contained the start of old line `abs`,
    /// or `None` if it was dropped.
    pub fn map(&self, abs: Line) -> Option<Line> {
        let idx = usize::try_from(abs.checked_sub(self.base)?).ok()?;
        let new = self.base + *self.map.get(idx)? as Line;
        self.valid.contains(&new).then_some(new)
    }
}

impl Grid {
    pub fn new(cols: usize, lines: usize, max_scrollback: usize) -> Self {
        let rows = (0..lines).map(|_| Row::new(cols, StyleId::DEFAULT)).collect();
        Self { rows, cols, lines, max_scrollback, dropped: 0, display_offset: 0 }
    }

    pub fn cols(&self) -> usize {
        self.cols
    }
    pub fn lines(&self) -> usize {
        self.lines
    }
    pub fn history_len(&self) -> usize {
        self.rows.len() - self.lines
    }
    pub fn max_scrollback(&self) -> usize {
        self.max_scrollback
    }
    pub fn set_max_scrollback(&mut self, n: usize) {
        self.max_scrollback = n;
        self.trim_history();
        self.display_offset = self.display_offset.min(self.history_len());
    }
    /// Absolute line number of the oldest row still stored.
    pub fn first_line(&self) -> Line {
        self.dropped
    }
    /// One past the absolute line number of the last screen row.
    pub fn end_line(&self) -> Line {
        self.dropped + self.rows.len() as Line
    }
    /// Absolute line number of screen row `y`.
    pub fn abs_line(&self, y: usize) -> Line {
        self.dropped + (self.history_len() + y) as Line
    }
    /// Screen row of an absolute line, if it is on screen.
    pub fn screen_y(&self, abs: Line) -> Option<usize> {
        let top = self.abs_line(0);
        (abs >= top && abs < top + self.lines as Line).then(|| (abs - top) as usize)
    }

    pub fn row(&self, y: usize) -> &Row {
        &self.rows[self.history_len() + y]
    }
    pub fn row_mut(&mut self, y: usize) -> &mut Row {
        let h = self.history_len();
        &mut self.rows[h + y]
    }
    pub fn abs_row(&self, abs: Line) -> Option<&Row> {
        let idx = usize::try_from(abs.checked_sub(self.dropped)?).ok()?;
        self.rows.get(idx)
    }
    /// All stored rows, oldest first.
    pub fn rows_mut(&mut self) -> impl Iterator<Item = &mut Row> {
        self.rows.iter_mut()
    }
    /// Renumber rows so the oldest has absolute line `first` (keeps numbering
    /// monotonic across a reset).
    pub fn set_first_line(&mut self, first: Line) {
        self.dropped = first;
    }
    pub fn cell_mut(&mut self, x: usize, y: usize) -> &mut Cell {
        self.row_mut(y).cell_mut(x)
    }

    // --- viewport (scrollback viewing) -----------------------------------

    /// Rows scrolled back from the bottom (0 = live screen).
    pub fn display_offset(&self) -> usize {
        self.display_offset
    }
    /// Scroll the viewport; positive = towards older lines. Returns true if it moved.
    pub fn scroll_display(&mut self, delta: isize) -> bool {
        let old = self.display_offset;
        self.display_offset = old.saturating_add_signed(delta).min(self.history_len());
        old != self.display_offset
    }
    pub fn reset_display(&mut self) -> bool {
        std::mem::take(&mut self.display_offset) != 0
    }
    /// Scroll the viewport so absolute line `abs` is at the top (clamped).
    pub fn scroll_to_line(&mut self, abs: Line) -> bool {
        let top = self.abs_line(0);
        let off = top.saturating_sub(abs).min(self.history_len() as Line) as usize;
        std::mem::replace(&mut self.display_offset, off) != off
    }
    /// Absolute line shown at viewport row `y`.
    pub fn viewport_line(&self, y: usize) -> Line {
        self.abs_line(y) - self.display_offset as Line
    }
    /// Row shown at viewport row `y` (accounts for the display offset).
    pub fn viewport_row(&self, y: usize) -> &Row {
        &self.rows[self.history_len() - self.display_offset + y]
    }

    // --- scrolling -------------------------------------------------------

    /// Scroll region `top..=bottom` up by `n`. If the region is the full screen
    /// and `to_history` is set, rows scrolled off the top go into scrollback.
    /// Returns the number of rows pushed into history.
    pub fn scroll_up(&mut self, top: usize, bottom: usize, n: usize, style: StyleId, to_history: bool) -> usize {
        let n = n.min(bottom + 1 - top);
        if n == 0 {
            return 0;
        }
        if to_history && top == 0 && bottom + 1 == self.lines && self.max_scrollback > 0 {
            for _ in 0..n {
                self.push_screen_row(style);
            }
            if self.display_offset > 0 {
                self.display_offset += n;
            }
            self.trim_history();
            self.display_offset = self.display_offset.min(self.history_len());
            return n;
        }
        let base = self.history_len();
        let (top, bottom) = (base + top, base + bottom);
        for i in top..bottom + 1 - n {
            self.rows.swap(i, i + n);
        }
        for i in bottom + 1 - n..=bottom {
            self.rows[i].reset(self.cols, style);
        }
        0
    }

    /// The top screen row moves into history and a blank row of `style`
    /// appears at the bottom. With history full, the oldest row's memory is
    /// reused for one of them.
    fn push_screen_row(&mut self, style: StyleId) {
        let mut spare = None;
        if self.rows.len() >= self.lines + self.max_scrollback {
            spare = self.rows.pop_front();
            self.dropped += 1;
        }
        let top = self.history_len();
        let mut row = match self.rows[top].archive(&mut spare) {
            Some(row) => row,
            // The dropped row keeps track of its blank tail, so reset only clears what it wrote.
            None => spare.unwrap_or_else(|| Row::from_cells(Vec::new(), false)),
        };
        row.reset(self.cols, style);
        self.rows.push_back(row);
    }

    /// Scroll region `top..=bottom` down by `n` (blank rows appear at `top`).
    pub fn scroll_down(&mut self, top: usize, bottom: usize, n: usize, style: StyleId) {
        let n = n.min(bottom + 1 - top);
        if n == 0 {
            return;
        }
        let base = self.history_len();
        let (top, bottom) = (base + top, base + bottom);
        for i in (top + n..=bottom).rev() {
            self.rows.swap(i, i - n);
        }
        for i in top..top + n {
            self.rows[i].reset(self.cols, style);
        }
    }

    fn trim_history(&mut self) {
        while self.rows.len() > self.lines + self.max_scrollback {
            self.rows.pop_front();
            self.dropped += 1;
        }
    }

    pub fn clear_history(&mut self) {
        let h = self.history_len();
        self.rows.drain(..h);
        self.dropped += h as Line;
        self.display_offset = 0;
    }

    pub fn clear_screen(&mut self, style: StyleId) {
        for y in 0..self.lines {
            self.row_mut(y).clear(style);
        }
    }

    // --- resizing --------------------------------------------------------

    /// Resize without reflow (alternate screen). Keeps the cursor row on screen.
    pub fn resize_simple(&mut self, cols: usize, lines: usize, cursor_y: usize) -> usize {
        let mut cursor_y = cursor_y;
        if lines < self.lines {
            let excess = self.lines - lines;
            let from_top = (cursor_y + 1).saturating_sub(lines).min(excess);
            let h = self.history_len();
            self.rows.drain(h..h + from_top);
            for _ in 0..excess - from_top {
                self.rows.pop_back();
            }
            cursor_y -= from_top;
        } else {
            for _ in self.lines..lines {
                self.rows.push_back(Row::new(cols, StyleId::DEFAULT));
            }
        }
        for row in &mut self.rows {
            row.resize(cols, StyleId::DEFAULT);
            row.wrapped = false;
        }
        self.cols = cols;
        self.lines = lines;
        self.display_offset = 0;
        cursor_y
    }

    /// Resize with reflow of soft-wrapped lines (primary screen).
    pub fn reflow(
        &mut self,
        cols: usize,
        lines: usize,
        cursor_x: usize,
        cursor_y: usize,
        pending_wrap: bool,
    ) -> Reflow {
        let cols = cols.max(2);
        let lines = lines.max(1);
        let base = self.dropped;
        let cursor_idx = self.history_len() + cursor_y;
        let cursor_col = cursor_x + usize::from(pending_wrap);
        let mut rows: Vec<Row> = std::mem::take(&mut self.rows).into();

        // Blank rows below the cursor carry nothing; dropping them keeps a
        // shrinking window from pushing real content into history.
        while rows.len() > cursor_idx + 1 {
            let n = rows.len();
            if rows[n - 1].is_blank() && !rows[n - 2].wrapped {
                rows.pop();
            } else {
                break;
            }
        }

        let mut out: Vec<Row> = Vec::with_capacity(rows.len());
        let mut map = vec![0usize; rows.len()];
        let mut new_cursor: Option<(usize, usize, bool)> = None;

        let mut i = 0;
        while i < rows.len() {
            let start = i;
            let mut end = i;
            while rows[end].wrapped && end + 1 < rows.len() {
                end += 1;
            }
            let last_wrapped = rows[end].wrapped;

            // Gather the logical line.
            let mut cells: Vec<Cell> = Vec::new();
            let mut row_starts = Vec::with_capacity(end - start + 1);
            let mut cursor_off = None;
            for (k, row) in rows.iter_mut().enumerate().take(end + 1).skip(start) {
                row_starts.push(cells.len());
                let is_last = k == end;
                let mut len = if is_last { row.content_len() } else { row.len() };
                if !is_last && len > 0 && row.cells()[len - 1].flags().contains(CellFlags::LEADING_WIDE_SPACER) {
                    len -= 1;
                }
                if k == cursor_idx {
                    cursor_off = Some(cells.len() + cursor_col);
                }
                // Free each old row as it is consumed to bound the peak.
                cells.extend(row.take_cells().into_iter().take(len));
            }

            // Rewrap into `cols`. Unwrapped rows are padded at the end, once
            // it is known whether they land in history.
            let first_new = out.len();
            let mut pos: Vec<(usize, usize)> = Vec::with_capacity(cells.len() + 1);
            let mut cur: Vec<Cell> = Vec::with_capacity(cols);
            let push_row = |cur: &mut Vec<Cell>, wrapped: bool, out: &mut Vec<Row>| {
                let mut c = std::mem::replace(cur, Vec::with_capacity(cols));
                if wrapped {
                    c.resize(cols, Cell::default());
                }
                out.push(Row::from_cells(c, wrapped));
            };
            let cell_count = cells.len();
            for cell in cells {
                let w = if cell.flags().contains(CellFlags::WIDE) { 2 } else { 1 };
                if cur.len() + w > cols {
                    if w == 2 && cur.len() + 1 == cols {
                        cur.push(Cell::new(' ', StyleId::DEFAULT, CellFlags::LEADING_WIDE_SPACER));
                    }
                    push_row(&mut cur, true, &mut out);
                }
                pos.push((out.len() - first_new, cur.len()));
                cur.push(cell);
            }
            let end_pos = (out.len() - first_new, cur.len());
            push_row(&mut cur, last_wrapped, &mut out);

            if let Some(off) = cursor_off {
                let (r, c, p) = if off < cell_count {
                    let (r, c) = pos[off];
                    (r, c, false)
                } else {
                    let vc = end_pos.1 + (off - cell_count);
                    if vc < cols { (end_pos.0, vc, false) } else { (end_pos.0, cols - 1, true) }
                };
                new_cursor = Some((first_new + r, c, p));
            }
            let line_rows = out.len() - first_new;
            for (n, k) in (start..=end).enumerate() {
                let s = row_starts[n];
                map[k] = first_new + if s < cell_count { pos[s].0 } else { line_rows - 1 };
            }
            i = end + 1;
        }

        let (mut cidx, cx, cpending) = new_cursor.unwrap_or((out.len().saturating_sub(1), 0, false));
        if out.is_empty() {
            out.push(Row::new(cols, StyleId::DEFAULT));
            cidx = 0;
        }
        // Keep the cursor on screen: drop content below it if necessary.
        if out.len() > cidx + lines {
            out.truncate(cidx + lines);
        }
        while out.len() < lines {
            out.push(Row::new(cols, StyleId::DEFAULT));
        }
        let excess = out.len().saturating_sub(lines + self.max_scrollback);
        out.drain(..excess);
        let history = out.len() - lines;
        for (n, row) in out.iter_mut().enumerate() {
            if n >= history || row.wrapped {
                row.pad_to(cols);
            } else {
                row.compact();
            }
        }

        let valid_end = base + (out.len() + excess) as Line;
        self.dropped = base + excess as Line;
        self.rows = out.into();
        self.cols = cols;
        self.lines = lines;
        self.display_offset = 0;

        Reflow {
            cursor_x: cx,
            cursor_y: cidx - excess - self.history_len(),
            pending_wrap: cpending,
            line_map: LineMap { base, map, valid: self.dropped..valid_end },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(grid: &mut Grid, y: usize, s: &str) {
        for (x, ch) in s.chars().enumerate() {
            grid.cell_mut(x, y).set_ch(ch);
        }
    }

    fn text(grid: &Grid, y: usize) -> String {
        grid.row(y).text(0..grid.cols()).trim_end().to_string()
    }

    #[test]
    fn scroll_into_history_and_trim() {
        let mut g = Grid::new(4, 2, 3);
        for i in 0..6 {
            put(&mut g, 1, &i.to_string());
            g.scroll_up(0, 1, 1, StyleId::DEFAULT, true);
        }
        assert_eq!(g.history_len(), 3);
        assert_eq!(g.first_line(), 3);
        assert_eq!(g.abs_row(4).unwrap().text(0..4).trim_end(), "3");
    }

    #[test]
    fn shrinking_history_clamps_viewport() {
        let mut g = Grid::new(5, 2, 100);
        for _ in 0..20 {
            g.scroll_up(0, 1, 1, StyleId::DEFAULT, true);
        }
        g.scroll_display(20);
        g.set_max_scrollback(1);
        assert_eq!(g.display_offset(), 1);
        assert!(g.viewport_row(0).is_blank());
        g.set_max_scrollback(0);
        assert_eq!(g.display_offset(), 0);
        assert_eq!(g.viewport_row(0).len(), 5);
    }

    #[test]
    fn reflow_shrink_and_grow() {
        let mut g = Grid::new(10, 3, 100);
        put(&mut g, 0, "abcdefgh");
        let r = g.reflow(4, 3, 8, 0, false);
        assert_eq!(text(&g, 0), "abcd");
        assert_eq!(text(&g, 1), "efgh");
        assert!(g.row(0).wrapped);
        // Cursor was just past "h": end of row 1 → pending wrap.
        assert_eq!((r.cursor_x, r.cursor_y, r.pending_wrap), (3, 1, true));
        let r = g.reflow(10, 3, r.cursor_x, r.cursor_y, r.pending_wrap);
        assert_eq!(text(&g, 0), "abcdefgh");
        assert!(!g.row(0).wrapped);
        assert_eq!((r.cursor_x, r.cursor_y), (8, 0));
    }

    #[test]
    fn reflow_wide_char_boundary() {
        let mut g = Grid::new(4, 2, 10);
        put(&mut g, 0, "abc");
        let c = g.cell_mut(3, 0);
        c.set_flags(CellFlags::LEADING_WIDE_SPACER);
        g.row_mut(0).wrapped = true;
        let c = g.cell_mut(0, 1);
        c.set_ch('中');
        c.set_flags(CellFlags::WIDE);
        g.cell_mut(1, 1).set_flags(CellFlags::WIDE_SPACER);
        g.reflow(6, 2, 2, 1, false);
        assert_eq!(text(&g, 0), "abc中");
        assert!(g.row(0).cells()[3].flags().contains(CellFlags::WIDE));
    }

    #[test]
    fn history_rows_drop_trailing_blanks_and_region_scroll_rotates() {
        let mut g = Grid::new(40, 3, 10);
        put(&mut g, 0, "short");
        g.scroll_up(0, 2, 1, StyleId::DEFAULT, true);
        let h = g.abs_row(0).unwrap();
        assert_eq!(h.len(), 5);
        assert_eq!(h.text(0..40), "short");
        assert_eq!(g.row(2).len(), 40);
        let mut g = Grid::new(4, 5, 0);
        for y in 0..5 {
            put(&mut g, y, &y.to_string());
        }
        g.scroll_up(1, 3, 2, StyleId::DEFAULT, false);
        assert_eq!((0..5).map(|y| text(&g, y)).collect::<Vec<_>>(), ["0", "3", "", "", "4"]);
        g.scroll_down(0, 4, 1, StyleId::DEFAULT);
        assert_eq!((0..5).map(|y| text(&g, y)).collect::<Vec<_>>(), ["", "0", "3", "", ""]);
        g.scroll_up(0, 4, 5, StyleId::DEFAULT, false);
        assert!((0..5).all(|y| text(&g, y).is_empty() && g.row(y).len() == 4));
    }

    #[test]
    fn line_map_follows_rows() {
        let mut g = Grid::new(4, 4, 10);
        put(&mut g, 0, "aaaa");
        g.row_mut(0).wrapped = true;
        put(&mut g, 1, "bb");
        put(&mut g, 2, "cc");
        let r = g.reflow(8, 4, 0, 3, false);
        assert_eq!(r.line_map.map(0), Some(0));
        assert_eq!(r.line_map.map(1), Some(0));
        assert_eq!(r.line_map.map(2), Some(1));
    }
}
