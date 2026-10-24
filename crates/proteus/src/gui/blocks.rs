//! Command blocks in the GUI (doc 01, D2): the gutter marks left of each
//! pane, hover info, folding, and the block actions.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use proteus_core::blocks::{Block, BlockId, BlockState};
use proteus_core::color::Rgb;
use proteus_core::event::ClipboardKind;
use proteus_core::selection::SelectionKind;
use proteus_core::{BlockPart, Line, Point, Terminal, input};

use super::render::Label;
use super::rows::{Row, Rows};
use super::scroll::CopyState;
use super::{App, Theme};
use crate::layout::{PaneId, Rect};
use crate::modes::{Mode, Part, Select};

/// Redraw interval while a running command's spinner is on screen.
pub const SPINNER: Duration = Duration::from_millis(200);

/// Lines of a block: (end of prompt and command, end of the block).
pub fn extent(t: &Terminal, b: &Block) -> (Line, Line) {
    let next = t.blocks().next_prompt(b.prompt_line).unwrap_or(Line::MAX);
    let live = t.primary_grid().abs_line(t.cursor().y) + 1;
    let min_end = b.prompt_line + 1;
    let prompt_end = b.output_start.unwrap_or(live.min(next)).max(min_end).min(next.max(min_end));
    let end = b.output_lines(live).map_or(prompt_end, |r| r.end).min(next).max(prompt_end);
    (prompt_end, end)
}

/// The block that `line` belongs to (its prompt, command or output).
pub fn block_at(t: &Terminal, line: Line) -> Option<&Block> {
    let b = t.blocks().at_line(line)?;
    (line < extent(t, b).1).then_some(b)
}

/// `1.2s`, `3m 05s`, `2h 01m`.
pub fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..10 => format!("{:.1}s", d.as_secs_f64()),
        10..60 => format!("{s}s"),
        60..3600 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, s / 60 % 60),
    }
}

/// One-line description: `✗ exit 2 · 1.3s · make test`.
pub fn summary(b: &Block) -> String {
    let status = match (b.state, b.exit_code) {
        (BlockState::Running, _) => "running".to_string(),
        (_, Some(0)) => "✓".to_string(),
        (_, Some(c)) => format!("✗ exit {c}"),
        (_, None) if b.output_start.is_some() => "done".to_string(),
        (_, None) => "prompt".to_string(),
    };
    let mut parts = vec![status];
    if let Some(d) = b.duration() {
        parts.push(human_duration(d));
    }
    if let Some(c) = b.command.as_deref().filter(|c| !c.is_empty()) {
        parts.push(c.lines().next().unwrap_or("").to_string());
    }
    parts.join(" · ")
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mark {
    None,
    /// Prompt and command rows.
    Prompt(Rgb, bool),
    /// Output rows (only drawn for failed or running commands).
    Output(Rgb),
    Fold,
}

impl App {
    /// Gutter width in physical pixels from the config.
    pub(super) fn gutter_px(&self) -> i32 {
        (self.cfg.gutter * self.cell().0 as f32).round() as i32
    }

    /// Width of the gutter drawn left of a grid at `r` (it lives in the
    /// window padding or the gap between panes).
    pub(super) fn gutter_width(&self, r: Rect) -> i32 {
        let g = self.gutter_px();
        if g <= 0 {
            return 0;
        }
        let thick = (self.w.scale.round() as i32).max(1);
        let room = if r.x <= self.area().x { r.x - 1 } else { self.cell().0 as i32 / 2 - thick - 1 };
        g.min(room).max(0)
    }

    /// Gutter marks of pane `id` whose grid is at `r`.
    pub(super) fn gutter_marks(
        &self,
        id: PaneId,
        r: Rect,
        rows: &Rows,
        skip: usize,
        theme: &Theme,
    ) -> Vec<([f32; 4], Rgb)> {
        let gw = self.gutter_width(r);
        let Some(p) = self.panes.get(&id) else {
            return Vec::new();
        };
        let t = &p.term;
        if gw <= 0 || t.is_alt_screen() || t.blocks().is_empty() {
            return Vec::new();
        }
        let red = t.palette().indexed(1);
        let mut cache: Option<(BlockId, (Line, Line))> = None;
        let mut kinds: Vec<Mark> = Vec::with_capacity(rows.0.len());
        for row in &rows.0 {
            let m = match *row {
                Row::Empty => Mark::None,
                Row::Fold { .. } => Mark::Fold,
                Row::Line(l) => match t.blocks().at_line(l) {
                    None => Mark::None,
                    Some(b) => {
                        let ext = match cache {
                            Some((bid, e)) if bid == b.id => e,
                            _ => {
                                let e = extent(t, b);
                                cache = Some((b.id, e));
                                e
                            }
                        };
                        let running = b.state == BlockState::Running;
                        let color = if running {
                            theme.accent
                        } else if b.failed() {
                            red
                        } else {
                            theme.dim
                        };
                        if l < ext.0 {
                            Mark::Prompt(color, running)
                        } else if l < ext.1 && (running || b.failed()) {
                            Mark::Output(color)
                        } else {
                            Mark::None
                        }
                    }
                },
            };
            kinds.push(m);
        }
        let ch = self.cell().1 as f32;
        let x = (r.x - gw - 1) as f32;
        let w = gw as f32;
        let thin = (w / 3.0).round().max(1.0);
        let mut out = Vec::new();
        let mut y = skip;
        while y < kinds.len() {
            let k = kinds[y];
            let mut e = y + 1;
            while e < kinds.len() && kinds[e] == k {
                e += 1;
            }
            let top = r.y as f32 + (y - skip) as f32 * ch;
            let h = (e - y) as f32 * ch;
            // A one-pixel break between neighboring blocks' marks.
            let h1 = (h - 1.0).max(1.0);
            match k {
                Mark::None => {}
                Mark::Prompt(c, running) => {
                    out.push(([x, top, w, h1], if running { theme.dim } else { c }));
                    if running {
                        // Spinner: a segment walking down the mark.
                        let phase = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_or(0, |d| d.as_millis() / SPINNER.as_millis())
                            % 4;
                        let seg = (h1 / 2.0).max(2.0);
                        let step = (h1 - seg) / 3.0;
                        out.push(([x, top + phase as f32 * step, w, seg], c));
                    }
                }
                Mark::Output(c) => out.push(([x, top, thin, h1], c)),
                Mark::Fold => out.push(([x, top, w, h1], theme.dim)),
            }
            y = e;
        }
        out
    }

    /// A command is running in a visible pane whose gutter is drawn.
    pub(super) fn spinner_visible(&self) -> bool {
        if self.gutter_px() <= 0 {
            return false;
        }
        let view = self.view();
        view.panes.iter().map(|p| p.0).chain(view.floats.iter().map(|f| f.id)).any(|id| {
            self.panes.get(&id).is_some_and(|p| {
                !p.term.is_alt_screen() && p.term.blocks().last().is_some_and(|b| b.state == BlockState::Running)
            })
        })
    }

    /// Pane grids of the active tab, topmost first: (pane, grid rect).
    fn grids_topmost_first(&self) -> Vec<(PaneId, Rect)> {
        let view = self.view();
        view.floats.iter().rev().map(|f| (f.id, f.content)).chain(view.panes.iter().copied()).collect()
    }

    /// Pane and display row under a logical position, with whether it is
    /// in the gutter (else in the grid).
    pub(super) fn row_at(&self, pos: (f64, f64)) -> Option<(PaneId, usize, bool)> {
        let (x, y) = self.phys(pos);
        let ch = self.cell().1 as i32;
        for (id, r) in self.grids_topmost_first() {
            let t = &self.panes.get(&id)?.term;
            let gw = self.gutter_width(r);
            let bottom = r.y + (t.lines() as i32) * ch;
            if y < r.y || y >= bottom || x >= r.right() || x < r.x - gw - 2 {
                continue;
            }
            let row = ((y - r.y) / ch) as usize + self.row_skip(id);
            return Some((id, row.min(t.lines() - 1), x < r.x));
        }
        None
    }

    /// Block whose gutter mark is under the pointer.
    pub(super) fn gutter_hit(&self, pos: (f64, f64)) -> Option<(PaneId, BlockId)> {
        let (id, row, gutter) = self.row_at(pos)?;
        if !gutter || self.gutter_width(self.view().rect(id)?) <= 0 {
            return None;
        }
        let t = &self.panes.get(&id)?.term;
        if t.is_alt_screen() {
            return None;
        }
        let rows = Rows::of(t);
        match rows.0.get(row)? {
            Row::Fold { block, .. } => Some((id, *block)),
            Row::Line(l) => block_at(t, *l).map(|b| (id, b.id)),
            Row::Empty => None,
        }
    }

    /// Pointer moved: update the hovered block. Returns true if over a gutter mark.
    pub(super) fn update_hover(&mut self, pos: Option<(f64, f64)>) -> bool {
        let hit = pos.and_then(|p| self.gutter_hit(p));
        if hit != self.w.hover {
            self.w.hover = hit;
            self.w.dirty = true;
        }
        hit.is_some()
    }

    /// Tooltip for the hovered block, above its prompt row.
    pub(super) fn hover_label(&self, theme: &Theme) -> Option<Label> {
        let (id, bid) = self.w.hover?;
        let t = &self.panes.get(&id)?.term;
        let b = t.blocks().get(bid)?;
        let r = self.view().rect(id)?;
        let rows = Rows::of(t);
        let skip = self.row_skip(id);
        let row = rows.row_of(b.prompt_line).filter(|&y| y >= skip).unwrap_or(skip);
        let ch = self.cell().1 as i32;
        let mut y = r.y + (row - skip) as i32 * ch - ch;
        if y < r.y {
            y += 2 * ch;
        }
        let text = format!(" {} ", summary(b));
        let cols = super::chrome::text_width(&text).min(t.cols());
        let bg = if b.failed() { t.palette().indexed(1) } else { theme.fg };
        Some(Label { x: r.x as f32, y: y as f32, cols, text, fg: theme.bg, bg: Some(bg), bold: false })
    }

    // --- actions ---------------------------------------------------------------

    /// Block an action applies to: the one at the Scroll cursor, else the
    /// last command that ran in the focused pane.
    pub(super) fn target_block(&self) -> Option<(PaneId, BlockId)> {
        if let Some(c) = &self.w.copy {
            let t = &self.panes.get(&c.pane)?.term;
            if t.is_alt_screen() {
                return None;
            }
            let b = t.blocks().at_line(c.cursor.line)?;
            return Some((c.pane, b.id));
        }
        let id = self.focused_id();
        let t = &self.panes.get(&id)?.term;
        let b = t.blocks().iter().rev().find(|b| b.output_start.is_some() || b.command.is_some())?;
        Some((id, b.id))
    }

    pub(super) fn block_copy(&mut self, part: Part) {
        let Some((id, bid)) = self.target_block() else {
            return self.notice(self.cfg.i18n.t("ui-no-command-block-here", &[]));
        };
        let t = &self.panes[&id].term;
        let p = match part {
            Part::Command => BlockPart::Command,
            Part::Output => BlockPart::Output,
            Part::All => BlockPart::All,
        };
        match t.block_text(bid, p).map(|s| s.trim_end_matches('\n').to_string()).filter(|s| !s.is_empty()) {
            Some(text) => {
                self.set_selection(ClipboardKind::Clipboard, text.clone());
                self.set_selection(ClipboardKind::Primary, text);
            }
            None => self.notice(format!("this block has no {}", part_name(part))),
        }
    }

    /// Select part of the block at the Scroll cursor; `y` copies it.
    pub(super) fn block_select(&mut self, part: Part) {
        let Some((id, bid)) = self.target_block() else {
            return;
        };
        if self.w.mode != Mode::Scroll {
            self.set_mode(Mode::Scroll);
        }
        let Some(p) = self.panes.get_mut(&id) else {
            return;
        };
        let t = &mut p.term;
        let Some(b) = t.blocks().get(bid).cloned() else {
            return;
        };
        let (prompt_end, end) = extent(t, &b);
        let last_col = t.cols() - 1;
        let (kind, start, stop) = match part {
            Part::Command => {
                let s = b.input_start.unwrap_or(Point::new(b.prompt_line, 0));
                (SelectionKind::Simple, s, Point::new(prompt_end - 1, last_col))
            }
            Part::Output => match b.output_start {
                Some(o) if o < end => (SelectionKind::Semantic, Point::new(o, 0), Point::new(o, 0)),
                _ => return self.notice(self.cfg.i18n.t("ui-this-block-has-no-output", &[])),
            },
            Part::All => (SelectionKind::Line, Point::new(b.prompt_line, 0), Point::new(end - 1, 0)),
        };
        t.set_block_folded(bid, false);
        t.start_selection(kind, start);
        t.update_selection(stop);
        let cursor = match part {
            Part::Output => Point::new(end - 1, 0),
            _ => stop,
        };
        t.reveal_line(cursor.line);
        // `y` copies the selection; moving the cursor extends it (a
        // semantic selection always covers the whole output).
        let select = if kind == SelectionKind::Line { Select::Line } else { Select::Char };
        self.w.copy = Some(CopyState { pane: id, cursor, select: Some(select) });
        self.w.dirty = true;
    }

    pub(super) fn block_fold(&mut self) {
        let Some((id, bid)) = self.target_block() else {
            return;
        };
        self.toggle_fold(id, bid);
    }

    pub(super) fn toggle_fold(&mut self, id: PaneId, bid: BlockId) {
        let Some(p) = self.panes.get_mut(&id) else {
            return;
        };
        let Some(b) = p.term.blocks().get(bid) else {
            return;
        };
        if b.state != BlockState::Finished || !b.output_lines(Line::MAX).is_some_and(|r| !r.is_empty()) {
            return;
        }
        let fold = !b.folded;
        p.term.set_block_folded(bid, fold);
        // The Scroll cursor moves to the marker (the fold's first line).
        if fold
            && let Some(c) = self.w.copy.as_mut().filter(|c| c.pane == id)
            && let Some((_, r)) = super::rows::folds(&p.term).into_iter().find(|f| f.0 == bid)
            && r.contains(&c.cursor.line)
        {
            c.cursor = Point::new(r.start, 0);
        }
        self.w.dirty = true;
    }

    pub(super) fn unfold_all(&mut self) {
        let id = self.w.copy.as_ref().map_or(self.focused_id(), |c| c.pane);
        let Some(p) = self.panes.get_mut(&id) else {
            return;
        };
        let ids: Vec<BlockId> = p.term.blocks().iter().filter(|b| b.folded).map(|b| b.id).collect();
        for b in ids {
            p.term.set_block_folded(b, false);
        }
        self.w.dirty = true;
    }

    /// Unfold the block hiding `line` of pane `id`, if any.
    pub(super) fn unfold_at(&mut self, id: PaneId, line: Line) {
        let Some(p) = self.panes.get_mut(&id) else {
            return;
        };
        if let Some((bid, _)) = super::rows::folds(&p.term).into_iter().find(|f| f.1.contains(&line)) {
            p.term.set_block_folded(bid, false);
            self.w.dirty = true;
        }
    }

    pub(super) fn block_rerun(&mut self) {
        let Some((id, bid)) = self.target_block() else {
            return;
        };
        let Some(cmd) =
            self.panes[&id].term.blocks().get(bid).and_then(|b| b.command.clone()).filter(|c| !c.trim().is_empty())
        else {
            return self.notice(self.cfg.i18n.t("ui-no-command-to-run-again", &[]));
        };
        if let Some(name) = self.panes[&id].busy(&[]) {
            return self.notice(format!("{name} is running in this pane"));
        }
        if self.w.mode == Mode::Scroll {
            self.set_mode(Mode::Normal);
        }
        let Some(p) = self.panes.get_mut(&id) else {
            return;
        };
        p.term.scroll_to_bottom();
        // Clear what is typed at the prompt, then the command as a paste
        // (no line runs early) and Enter.
        let mut bytes = vec![0x15];
        bytes.extend(input::encode_paste(&cmd, p.term.mode()));
        bytes.push(b'\r');
        p.write(&bytes);
    }

    pub(super) fn block_info(&mut self) {
        let Some((id, bid)) = self.target_block() else {
            return self.notice(self.cfg.i18n.t("ui-no-command-block-here", &[]));
        };
        let t = &self.panes[&id].term;
        let Some(b) = t.blocks().get(bid) else { return };
        let mut s = summary(b);
        if let Some(cwd) = &b.cwd {
            s.push_str(&format!(" · in {cwd}"));
        }
        let (prompt_end, end) = extent(t, b);
        let lines = end.saturating_sub(prompt_end);
        if b.output_start.is_some() {
            s.push_str(&format!(
                " · {} output line{}",
                super::render::group_digits(lines),
                if lines == 1 { "" } else { "s" }
            ));
        }
        self.notice(s);
    }
}

fn part_name(p: Part) -> &'static str {
    match p {
        Part::Command => "command",
        Part::Output => "output",
        Part::All => "text",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(human_duration(Duration::from_millis(1250)), "1.2s");
        assert_eq!(human_duration(Duration::from_secs(42)), "42s");
        assert_eq!(human_duration(Duration::from_secs(185)), "3m 05s");
        assert_eq!(human_duration(Duration::from_secs(7260)), "2h 01m");
    }

    #[test]
    fn extents_and_summary() {
        let mut t = Terminal::new(20, 10, proteus_core::Config::default());
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07false\r\n\x1b]133;C\x07err1\r\nerr2\r\n\x1b]133;D;1\x07");
        t.advance(b"\x1b]133;A\x07$ ");
        let b = t.blocks().iter().next().unwrap().clone();
        let l0 = b.prompt_line;
        assert_eq!(extent(&t, &b), (l0 + 1, l0 + 3));
        assert!(summary(&b).starts_with("✗ exit 1 · "), "{}", summary(&b));
        assert!(summary(&b).ends_with("· false"));
        assert_eq!(block_at(&t, l0 + 2).map(|b| b.id), Some(b.id));
        let last = t.blocks().last().unwrap().clone();
        // The live prompt spans its own line only.
        assert_eq!(extent(&t, &last), (l0 + 4, l0 + 4));
        assert_eq!(block_at(&t, l0 + 5).map(|b| b.id), None);
    }
}
