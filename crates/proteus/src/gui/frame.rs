//! Building and drawing frames: theme, tab bar, pane chrome, bottom bar,
//! redraw scheduling; window title, notices and input-method placement.

use super::*;

impl App {
    pub(super) fn theme(&self) -> Theme {
        let pal = self.focused_pane().term.palette();
        let (fg, bg) = (pal.foreground(), pal.background());
        let ui = self.cfg.theme.ui;
        Theme {
            fg: ui.foreground.unwrap_or(fg),
            bg: ui.background.unwrap_or(bg),
            accent: ui.accent.unwrap_or_else(|| pal.indexed(4)),
            bar: ui.bar.unwrap_or_else(|| chrome::mix(bg, fg, 0.10)),
            dim: ui.dim.unwrap_or_else(|| chrome::mix(bg, fg, 0.35)),
            colors: ui,
            cursor_shape: self.cfg.cursor_shape,
        }
    }

    pub(super) fn tab_labels(&self, theme: &Theme) -> Vec<render::Label> {
        let (pw, _) = self.physical();
        let p = self.pad() as f32;
        let cw = self.cell().0 as f32;
        let titles: Vec<String> = self
            .w
            .tabs
            .iter()
            .map(|t| {
                let title = t.name.as_deref().unwrap_or_else(|| self.panes.get(&t.focus).map_or("", |p| p.title()));
                let n = t.panes().len();
                // A bell rang in one of its panes.
                let bell =
                    if t.panes().iter().any(|id| self.panes.get(id).is_some_and(|p| p.bell)) { "• " } else { "" };
                match (n, t.zoomed) {
                    (1, _) => format!("{bell}{title}"),
                    (n, true) => format!("{bell}{title} [{n}, zoom]"),
                    (n, false) => format!("{bell}{title} [{n}]"),
                }
            })
            .collect();
        let cols = ((pw as f32 - 2.0 * p) / cw).max(1.0) as usize;
        let cols = cols.saturating_sub(self.status_cols(cols)).max(1);
        let y = (self.title_bar_h() + self.pad_y() as i32) as f32;
        chrome::tab_bar(&titles, self.w.active, p, y, cols, cw, theme)
    }

    fn pane_part(&self, id: PaneId, r: Rect, fid: PaneId, floating: bool, theme: &Theme) -> PanePart {
        let focused = id == fid;
        let preedit = self.ime.as_ref().and_then(|i| i.preedit.clone());
        let pane = self.panes.get(&id);
        let rows = pane.map_or(rows::Rows(Vec::new()), |p| rows::Rows::of(&p.term));
        let skip = self.row_skip(id);
        let mut marks = self.search_marks(id);
        marks.extend(self.hint_marks(id));
        let focused = focused && self.w.surface_focus.is_none();
        PanePart {
            id,
            surface: None,
            x: r.x as f32,
            y: r.y as f32,
            focused: focused && self.w.focused,
            preedit: if focused && self.w.input.is_none() && self.w.hints.is_none() { preedit } else { None },
            copy_cursor: self.copy_cursor_cell(id),
            marks,
            skip,
            floating,
            backdrop: self.gutter_marks(id, r, &rows, skip, theme),
            labels: Vec::new(),
            rows,
            flash: pane.and_then(|p| p.flash_until).is_some_and(|t| Instant::now() < t),
        }
    }

    /// Title text of a pane's title row: name, and the mode on the focused one.
    pub(super) fn title_text(&self, id: PaneId, focused: bool) -> String {
        let p = self.panes.get(&id);
        let t = p.map_or("", |p| p.title());
        let t = if p.is_some_and(|p| p.bell) { format!("• {t}") } else { t.to_string() };
        let t = if p.is_some_and(|p| p.private) { format!("[private] {t}") } else { t };
        if focused && self.w.mode != Mode::Normal { format!(" {t}  [{}]", self.w.mode.name()) } else { format!(" {t}") }
    }

    pub(super) fn frame_parts(&self) -> FrameParts {
        let view = self.view();
        let m = self.layout_metrics();
        let fid = self.focused_id();
        let theme = self.theme();
        let (pw, ph) = self.physical();
        let (cw, ch) = self.cell();
        let (cw, ch) = (cw as f32, ch as f32);
        let p = self.pad() as f32;
        let py = self.pad_y() as f32;
        let thick = (self.cfg.border_width * self.w.scale) as f32;
        let float_focus = self.w.tabs[self.w.active].focus_is_float();
        let mut panes: Vec<PanePart> =
            view.panes.iter().map(|&(id, r)| self.pane_part(id, r, fid, false, &theme)).collect();

        // Tiled chrome: dividers, the focused pane's accent, title rows.
        let mut tiled_rects = Vec::new();
        let mut tiled_labels = Vec::new();
        for d in &view.dividers {
            let g = &d.gap;
            let r = match d.axis {
                Axis::X => [(g.x as f32 + g.w as f32 / 2.0 - thick / 2.0).round(), g.y as f32, thick, g.h as f32],
                Axis::Y => [g.x as f32, (g.y as f32 + g.h as f32 / 2.0 - thick / 2.0).round(), g.w as f32, thick],
            };
            tiled_rects.push((r, theme.dim));
        }
        let tiled_focus = if float_focus { None } else { Some(fid) };
        if view.panes.len() > 1
            && let Some(r) = tiled_focus.and_then(|f| view.panes.iter().find(|p| p.0 == f).map(|p| p.1))
        {
            // Include the pane's title row in the outline.
            let r = match view.titles.iter().find(|t| Some(t.id) == tiled_focus && !t.collapsed) {
                Some(t) => Rect::new(r.x, t.rect.y, r.w, r.bottom() - t.rect.y),
                None => r,
            };
            let area = self.area();
            let half = m.gap as f32 / 2.0;
            let (x0, y0) = ((r.x as f32 - half).max(area.x as f32), (r.y as f32 - half).max(area.y as f32));
            let x1 = (r.right() as f32 + half).min(area.right() as f32);
            let y1 = (r.bottom() as f32 + half).min(area.bottom() as f32);
            let mid = |v: f32| (v - thick / 2.0).round();
            if r.x > area.x {
                tiled_rects.push(([mid(r.x as f32 - half), y0, thick, y1 - y0], theme.accent));
            }
            if r.right() < area.right() {
                tiled_rects.push(([mid(r.right() as f32 + half), y0, thick, y1 - y0], theme.accent));
            }
            if r.y > area.y {
                tiled_rects.push(([x0, mid(r.y as f32 - half), x1 - x0, thick], theme.accent));
            }
            if r.bottom() < area.bottom() {
                tiled_rects.push(([x0, mid(r.bottom() as f32 + half), x1 - x0, thick], theme.accent));
            }
        }
        for t in &view.titles {
            let on = Some(t.id) == tiled_focus;
            let (fg, bg) = match (on, t.collapsed) {
                (true, _) => (theme.bg, theme.accent),
                (false, true) => (theme.dim, theme.bar),
                (false, false) => (theme.fg, theme.bar),
            };
            let cols = (t.rect.w as f32 / cw).max(1.0) as usize;
            tiled_labels.push(render::Label {
                x: t.rect.x as f32,
                y: t.rect.y as f32,
                cols,
                text: self.title_text(t.id, on),
                fg,
                bg: Some(bg),
                bold: on,
            });
        }

        // Plugin sidebars (with the tiled panes) and floats (over everything).
        let (surface_tiled, surface_floats, surface_rects, surface_labels) = self.surface_parts(&theme);
        panes.extend(surface_tiled);
        tiled_rects.extend(surface_rects);
        tiled_labels.extend(surface_labels);

        // Floating panes, each with its frame.
        for f in &view.floats {
            let on = f.id == fid;
            let mut part = self.pane_part(f.id, f.content, fid, true, &theme);
            let bg = self.panes.get(&f.id).map_or(theme.bg, |p| p.term.palette().background());
            let line = if on { theme.accent } else { theme.dim };
            let o = f.outer;
            let (x, y, w, h) = (o.x as f32, o.y as f32, o.w as f32, o.h as f32);
            let mut frame = vec![
                ([x, y, w, h], bg),
                ([x, f.title.y as f32, w, ch], if on { theme.accent } else { theme.bar }),
                ([x, y, thick, h], line),
                ([x + w - thick, y, thick, h], line),
                ([x, y + h - thick, w, thick], line),
            ];
            frame.append(&mut part.backdrop);
            part.backdrop = frame;
            let cols = (f.title.w as f32 / cw).max(1.0) as usize;
            part.labels.push(render::Label {
                x: f.title.x as f32,
                y: f.title.y as f32,
                cols,
                text: self.title_text(f.id, on),
                fg: if on { theme.bg } else { theme.fg },
                bg: None,
                bold: on,
            });
            panes.push(part);
        }

        panes.extend(surface_floats);
        let mut rects = Vec::new();
        let mut labels = Vec::new();
        if self.tab_bar_visible() {
            rects.push(([0.0, self.title_bar_h() as f32, pw as f32, 1.5 * py + ch], theme.bar));
            let tabs = self.tab_labels(&theme);
            labels.extend(self.status_labels(&tabs, &theme));
            labels.extend(tabs);
        }
        if self.hint_bar_visible() {
            let y = ph as f32 - py - ch;
            rects.push(([0.0, y - py / 2.0, pw as f32, ph as f32 - y + py / 2.0], theme.bar));
            let cols = ((pw as f32 - 2.0 * p) / cw).max(1.0) as usize;
            if let Some(i) = &self.w.input {
                let caret_byte = i.before_cursor().len();
                let (l, x) = chrome::input_bar(
                    i.purpose.badge(),
                    &i.text,
                    caret_byte,
                    &i.status,
                    p,
                    y,
                    cols,
                    cw,
                    &theme,
                    &self.cfg.i18n,
                );
                labels.extend(l);
                rects.push(([x, y, (cw / 8.0).round().max(1.0), ch], theme.fg));
            } else if let Some(v) = &self.w.viewer {
                let hints = [
                    ("wheel +-", "ui-zoom"),
                    ("0", "ui-fit"),
                    ("1", "1:1"),
                    ("drag ←→", "ui-pan"),
                    ("c", "action-copy"),
                    ("s", "ui-save"),
                    ("esc", "ui-close"),
                ]
                .map(|(k, a)| (k.to_string(), a))
                .to_vec();
                labels.extend(chrome::hint_bar(
                    &v.title(self.viewer_area(), &self.cfg.i18n),
                    &hints,
                    p,
                    y,
                    cols,
                    cw,
                    &theme,
                    &self.cfg.i18n,
                ));
            } else if let Some(h) = &self.w.hints {
                let hints = vec![
                    (
                        "a-z".to_string(),
                        match h.action {
                            crate::modes::HintAction::Copy => "action-copy",
                            crate::modes::HintAction::Open => "ui-open",
                            crate::modes::HintAction::Insert => "ui-insert",
                        },
                    ),
                    ("shift".to_string(), "action-copy"),
                    ("ctrl".to_string(), "ui-insert"),
                    ("bksp".to_string(), "ui-undo"),
                    ("esc".to_string(), "ui-cancel"),
                ];
                labels.extend(chrome::hint_bar(
                    &self.cfg.i18n.t("ui-hints-count", &[("count", &h.items.len().to_string())]),
                    &hints,
                    p,
                    y,
                    cols,
                    cw,
                    &theme,
                    &self.cfg.i18n,
                ));
            } else {
                let mut name = self.cfg.i18n.t(&format!("badge-{}", self.w.mode.key()), &[]);
                if self.w.tabs[self.w.active].zoomed {
                    name.push_str(&self.cfg.i18n.t("ui-zoom-suffix", &[]));
                }
                if let Some(s) = &self.w.search {
                    name = format!("{name} {}", s.status(&self.cfg.i18n));
                }
                labels.extend(chrome::hint_bar(
                    &name,
                    &self.keymap.hints(self.w.mode),
                    p,
                    y,
                    cols,
                    cw,
                    &theme,
                    &self.cfg.i18n,
                ));
            }
        }

        labels.extend(self.hint_labels(&theme));
        labels.extend(self.hover_label(&theme));
        let (r, l) = self.toast_labels(&theme);
        rects.extend(r);
        labels.extend(l);

        labels.extend(self.completion_labels(&theme));

        let (r, l) = self.csd_parts(&theme);
        rects.extend(r);
        labels.extend(l);

        FrameParts {
            panes,
            info: FrameInfo {
                panes: Vec::new(),
                pad: p,
                pad_y: py,
                theme,
                cursor_blink_off: self.blink_off,
                ligatures: self.cfg.ligatures,
                tiled_rects,
                tiled_labels,
                rects,
                labels,
                overlay: self.w.prompts.front().map(|p| p.overlay(&self.cfg.i18n)),
                viewer: None,
            },
        }
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.w.dirty = true;
    }

    pub(super) fn update_title(&mut self) {
        let t = self.focused_pane().title();
        let t = if t.is_empty() { "proteus" } else { t }.to_string();
        if t != self.w.window_title {
            self.w.window.set_title(t.clone());
            self.w.window_title = t;
        }
    }

    pub(super) fn notice(&mut self, msg: String) {
        self.w.prompts.push_back(Prompt::Notice(msg));
        self.w.dirty = true;
    }

    // --- input method ----------------------------------------------------------

    /// Text committed by the input method is sent as if typed.
    pub(crate) fn commit_text(&mut self, text: &str) {
        self.restart_blink();
        if self.w.input.is_some() {
            return self.input_text(text);
        }
        if let Some(s) = self.key_surface() {
            return self.surface_ui(s, proteus_wasm::types::UiEvent::Text(text.to_string()));
        }
        let p = self.focused_pane_mut();
        p.term.scroll_to_bottom();
        p.write(text.as_bytes());
    }

    pub(crate) fn scroll_focused_to_bottom(&mut self) {
        self.focused_pane_mut().term.scroll_to_bottom();
    }

    /// Tell the input method where the cursor (or the preedit cursor) is.
    pub(crate) fn update_ime_rect(&mut self) {
        let (cw, ch) = self.cell();
        let (r, cols, x, y) = match self.surface_ime_spot() {
            Some((r, cols, (x, y))) => (r, cols, x, y),
            None => {
                let Some(r) = self.view().rect(self.focused_id()) else { return };
                let t = &self.focused_pane().term;
                let skip = self.row_skip(self.focused_id());
                let y = rows::Rows::of(t)
                    .row_of(t.grid().abs_line(t.cursor().y))
                    .unwrap_or(t.cursor().y)
                    .saturating_sub(skip);
                let x = t.visual_row(t.grid().abs_line(t.cursor().y)).visual(t.cursor().x);
                (r, t.cols(), x, y)
            }
        };
        let s = self.w.scale;
        let Some(ime) = self.ime.as_mut() else { return };
        let col = match &ime.preedit {
            Some(p) => render::preedit_start(x, p.width(), cols) + p.cursor_col().unwrap_or(0),
            None => x,
        }
        .min(cols.saturating_sub(1));
        let lx = ((r.x as f64 + col as f64 * cw as f64) / s).round() as i32;
        let ly = ((r.y as f64 + y as f64 * ch as f64) / s).round() as i32;
        let w = (cw as f64 / s).ceil() as i32;
        let h = (ch as f64 / s).ceil() as i32;
        ime.set_cursor_rect((lx, ly, w.max(1), h.max(1)));
    }

    /// Redraw the window after `after`, unless a redraw is already due
    /// sooner. Only the earliest timer counts; superseded ones do nothing.
    pub(super) fn schedule(&mut self, after: Duration) {
        let at = Instant::now() + after;
        if self.w.wake_at.is_some_and(|w| w <= at) {
            return;
        }
        self.w.wake_at = Some(at);
        let id = self.w.id;
        let _ = self.handle.insert_source(Timer::from_deadline(at), move |_, _, app| {
            if app.activate(id) && app.w.wake_at == Some(at) {
                app.w.wake_at = None;
                app.w.dirty = true;
            }
            TimeoutAction::Drop
        });
    }

    pub(super) fn draw(&mut self) {
        // Configures are applied once per event loop turn, not once per event.
        if std::mem::take(&mut self.w.size_pending) {
            self.apply_size();
        }
        if self.w.resize_deferred {
            self.apply_deferred_resizes();
        }
        if !self.w.configured || self.w.suspended || self.w.frame_pending || self.exit {
            return;
        }
        let view = self.view();
        let visible: Vec<PaneId> = view.panes.iter().map(|p| p.0).chain(view.floats.iter().map(|f| f.id)).collect();
        let mine: Vec<PaneId> = self.w.tabs.iter().flat_map(|t| t.panes()).collect();
        // Mode 2026: hold the frame while an application is mid-update.
        if visible.iter().any(|id| self.panes.get(id).is_some_and(|p| p.term.is_synchronized())) {
            let since = *self.w.sync_since.get_or_insert_with(Instant::now);
            let held = since.elapsed();
            if held < SYNC_TIMEOUT {
                self.schedule(SYNC_TIMEOUT - held);
                return;
            }
        } else {
            self.w.sync_since = None;
        }
        let mut changed = std::mem::take(&mut self.w.dirty);
        changed |= self.expire_toasts();
        // Animated images advance only while their pane is on screen.
        let now = Instant::now();
        let mut next_frame: Option<Instant> = None;
        for id in &visible {
            if let Some(p) = self.panes.get_mut(id)
                && let Some(t) = p.term.tick_images(now)
            {
                next_frame = Some(next_frame.map_or(t, |n| n.min(t)));
            }
        }
        for id in &mine {
            // Hidden panes' damage is dropped: they redraw fully when shown.
            if let Some(p) = self.panes.get_mut(id)
                && p.term.clear_damage()
                && visible.contains(id)
            {
                changed = true;
            }
        }
        if let Some(t) = next_frame {
            self.schedule(t.saturating_duration_since(now));
        }
        if !changed {
            return;
        }
        let surface = self.w.window.wl_surface().clone();
        surface.frame(&self.qh, FrameCallbackData(surface.clone()));
        let area = self.viewer_area();
        let mut info = self.frame_parts().resolve(&self.panes, &self.pl.surfaces);
        info.viewer = self.w.viewer.as_ref().map(|v| v.frame(area));
        let ok = self.w.renderer.draw(&mut self.w.fonts, &info);
        drop(info);
        if ok {
            self.w.frame_pending = true;
        } else {
            // No buffer available (resize race): try again shortly.
            self.w.dirty = true;
            self.schedule(Duration::from_millis(16));
        }
        if std::mem::take(&mut self.w.renderer.redraw_needed) {
            self.w.dirty = true;
        }
        // Animations: the spinner of running commands, the visual bell.
        let flash_end = visible.iter().filter_map(|id| self.panes.get(id)?.flash_until).max();
        match flash_end {
            Some(t) if t > Instant::now() => self.schedule(t - Instant::now()),
            _ if self.spinner_visible() => self.schedule(blocks::SPINNER),
            _ => {}
        }
        self.update_ime_rect();
    }
}
