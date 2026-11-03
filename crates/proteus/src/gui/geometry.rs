//! Window geometry: padding, cells, the pane area, sizes, hit testing.

use super::*;

impl App {
    pub(super) fn pad(&self) -> u32 {
        (self.cfg.padding * self.w.scale).round() as u32
    }

    pub(super) fn pad_y(&self) -> u32 {
        (self.cfg.padding_vertical * self.w.scale).round() as u32
    }

    fn pane_gap(&self) -> i32 {
        self.cfg.pane_gap.map_or(self.cell().0 as i32, |n| (n * self.w.scale).round() as i32)
    }

    pub(super) fn physical(&self) -> (u32, u32) {
        ((self.w.size.0 as f64 * self.w.scale).round() as u32, (self.w.size.1 as f64 * self.w.scale).round() as u32)
    }

    pub(super) fn cell(&self) -> (u32, u32) {
        let m = self.w.fonts.metrics();
        (m.cell_w, m.cell_h)
    }

    /// Layout sizes for the active tab.
    pub(super) fn layout_metrics(&self) -> layout::Metrics {
        match self.w.tabs.get(self.w.active) {
            Some(t) => self.tab_metrics(t),
            None => {
                let (w, h) = self.cell();
                layout::Metrics { cell_w: w as i32, cell_h: h as i32, gap: self.pane_gap(), titles: false }
            }
        }
    }

    pub(super) fn tab_metrics(&self, t: &Tab) -> layout::Metrics {
        let (w, h) = self.cell();
        let titles = match self.cfg.pane_titles {
            PaneTitles::Always => true,
            PaneTitles::Never => false,
            PaneTitles::Auto => matches!(t.root, layout::Node::Split { .. }),
        };
        layout::Metrics { cell_w: w as i32, cell_h: h as i32, gap: self.pane_gap(), titles }
    }

    pub(super) fn tab_bar_visible(&self) -> bool {
        match self.cfg.tab_bar {
            TabBar::Always => true,
            TabBar::Never => false,
            TabBar::Auto => {
                let (l, r) = self.status_all();
                self.w.tabs.len() > 1 || !l.is_empty() || !r.is_empty()
            }
        }
    }

    pub(super) fn hint_bar_visible(&self) -> bool {
        if self.w.input.is_some() || self.w.hints.is_some() || self.w.viewer.is_some() {
            return true;
        }
        match self.cfg.hint_bar {
            HintBar::Always => true,
            HintBar::Never => false,
            HintBar::InMode => self.w.mode != Mode::Normal,
        }
    }

    /// The part of the window given to panes, in physical pixels.
    pub(super) fn area(&self) -> Rect {
        let (pw, ph) = self.physical();
        let p = self.pad() as i32;
        let ch = self.cell().1 as i32;
        let py = self.pad_y() as i32;
        let mut top = py + self.title_bar_h();
        let mut bottom = ph as i32 - py;
        if self.tab_bar_visible() {
            top += ch + py;
        }
        if self.cfg.hint_bar == HintBar::Always {
            bottom -= ch + py;
        }
        // The block gutter of the leftmost panes lives in the padding (or
        // the gap after a sidebar).
        let (sl, sr) = self.sidebar_widths();
        let left = if sl > 0 { p + sl } else { p.max(self.gutter_px() + 1) };
        Rect::new(left, top, (pw as i32 - left - p - sr).max(1), (bottom - top).max(1))
    }

    /// Visible panes of the active tab.
    pub(super) fn view(&self) -> Layout {
        self.w.tabs[self.w.active].layout(self.area(), self.layout_metrics())
    }

    pub(super) fn focused_id(&self) -> PaneId {
        self.w.tabs[self.w.active].focus
    }

    pub(super) fn focused_pane(&self) -> &Pane {
        &self.panes[&self.focused_id()]
    }

    pub(super) fn focused_pane_mut(&mut self) -> &mut Pane {
        let id = self.focused_id();
        self.panes.get_mut(&id).expect("focused pane exists")
    }

    /// Recompute surface size and grids after a size, scale or font change.
    pub(super) fn apply_size(&mut self) {
        let (pw, ph) = self.physical();
        if let Some(vp) = &self.w.viewport {
            vp.set_destination(self.w.size.0 as i32, self.w.size.1 as i32);
        }
        self.w.renderer.resize(pw, ph);
        self.style_window();
        self.relayout();
    }

    /// Fit every pane of every tab to its rectangle.
    pub(super) fn relayout(&mut self) {
        let (area, cell) = (self.area(), self.cell());
        for (i, t) in self.w.tabs.iter().enumerate() {
            let m = self.tab_metrics(t);
            let l = t.layout(area, m);
            for (id, r) in l.panes.into_iter().chain(l.floats.iter().map(|f| (f.id, f.content))) {
                if let Some(p) = self.panes.get_mut(&id) {
                    let cols = (r.w / m.cell_w).max(2) as u16;
                    let rows = (r.h / m.cell_h).max(1) as u16;
                    // Reflowing scrollback is costly; hidden tabs catch up when shown.
                    if i != self.w.active && p.pty.is_some() {
                        p.defer_resize(cols, rows, cell);
                        self.w.resize_deferred |= p.has_deferred_resize();
                    } else {
                        p.resize(cols, rows, cell, !self.w.defer_pty_resize);
                    }
                }
            }
        }
        self.relayout_surfaces();
        self.w.dirty = true;
    }

    /// Resize the shown panes whose resize waited while their tab was hidden.
    pub(super) fn apply_deferred_resizes(&mut self) {
        let view = self.view();
        let notify = !self.w.defer_pty_resize;
        for id in view.panes.iter().map(|p| p.0).chain(view.floats.iter().map(|f| f.id)) {
            if let Some(p) = self.panes.get_mut(&id) {
                p.apply_deferred_resize(notify);
            }
        }
        let panes = &self.panes;
        self.w.resize_deferred =
            self.w.tabs.iter().flat_map(|t| t.panes()).any(|id| panes.get(&id).is_some_and(Pane::has_deferred_resize));
    }

    pub(super) fn set_scale(&mut self, scale: f64) {
        if (scale - self.w.scale).abs() < 1e-6 || scale <= 0.0 {
            return;
        }
        self.w.scale = scale;
        if self.w.fonts.set_size(self.w.font_size, scale) {
            self.w.renderer.clear_glyphs();
        }
        self.apply_size();
    }

    pub(super) fn set_font_size(&mut self, size: f32) {
        self.w.font_size = size.clamp(4.0, 96.0);
        if self.w.fonts.set_size(self.w.font_size, self.w.scale) {
            self.w.renderer.clear_glyphs();
            self.apply_size();
        }
    }

    /// Physical position of a surface-local logical position.
    pub(super) fn phys(&self, pos: (f64, f64)) -> (i32, i32) {
        ((pos.0 * self.w.scale) as i32, (pos.1 * self.w.scale) as i32)
    }

    pub(super) fn pane_at(&self, pos: (f64, f64)) -> Option<PaneId> {
        self.hit_at(pos).map(|h| h.0)
    }

    pub(super) fn hit_at(&self, pos: (f64, f64)) -> Option<(PaneId, layout::Part)> {
        let (x, y) = self.phys(pos);
        self.view().hit(x, y, self.layout_metrics())
    }

    /// Cell (col, viewport row) of pane `id` under a logical position, and
    /// the pixel offset inside the grid; clamped to the grid.
    pub(super) fn cell_in(&self, id: PaneId, pos: (f64, f64)) -> Option<(usize, usize, (u32, u32))> {
        let r = self.view().rect(id)?;
        let t = &self.panes.get(&id)?.term;
        let (cw, ch) = self.cell();
        let px = (pos.0 * self.w.scale - r.x as f64).max(0.0);
        let py = (pos.1 * self.w.scale - r.y as f64).max(0.0);
        let col = ((px / cw as f64) as usize).min(t.cols() - 1);
        let row = ((py / ch as f64) as usize + self.row_skip(id)).min(t.lines() - 1);
        Some((col, row, (px as u32, py as u32)))
    }

    /// Top of the bottom bar when it is drawn over the panes (not reserved).
    pub(super) fn overlay_top(&self) -> Option<i32> {
        if !self.hint_bar_visible() || self.cfg.hint_bar == HintBar::Always {
            return None;
        }
        let (_, ph) = self.physical();
        let (p, ch) = (self.pad_y() as f32, self.cell().1 as f32);
        Some((ph as f32 - p - ch - p / 2.0).floor() as i32)
    }

    /// Rows to draw pane `id` shifted up by (see [`render::PaneFrame::skip`]):
    /// one while the Scroll-mode cursor, or the search match being typed
    /// for, is on a last row that the bottom bar covers.
    pub(super) fn row_skip(&self, id: PaneId) -> usize {
        let Some(bar) = self.overlay_top() else { return 0 };
        let (Some(r), Some(p)) = (self.view().rect(id), self.panes.get(&id)) else { return 0 };
        let t = &p.term;
        let ch = self.cell().1 as i32;
        if t.lines() < 2 || r.y + t.lines() as i32 * ch <= bar {
            return 0;
        }
        let rows = rows::Rows::of(t);
        let last = t.lines() - 1;
        let at_last = |l: u64| rows.row_of(l) == Some(last);
        let cursor = self.w.copy.as_ref().filter(|c| c.pane == id).is_some_and(|c| at_last(c.cursor.line));
        let current = self
            .w
            .search
            .as_ref()
            .and_then(|s| s.matches.get(s.current?))
            .filter(|(p, _)| *p == id)
            .is_some_and(|(_, m)| at_last(m.start.line) || at_last(m.end.line));
        usize::from(cursor || (current && self.w.input.is_some()))
    }

    pub(super) fn point_in(&self, id: PaneId, pos: (f64, f64)) -> Option<Point> {
        let (col, row, _) = self.cell_in(id, pos)?;
        let rows = rows::Rows::of(&self.panes.get(&id)?.term);
        let line = rows.line(row).or_else(|| rows.last_line())?;
        let col = self.panes.get(&id)?.term.visual_row(line).logical(col);
        Some(Point::new(line, col))
    }
}
