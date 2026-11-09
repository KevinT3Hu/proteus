//! Opening and closing panes, tabs and windows; moving tabs between windows.

use super::*;

impl App {
    /// Start a shell in a new pane (not yet placed in a tab).
    pub(super) fn spawn_pane(
        &mut self,
        cols: u16,
        rows: u16,
        cwd: Option<PathBuf>,
        command: Option<Vec<OsString>>,
    ) -> io::Result<PaneId> {
        let id = self.next_id;
        self.next_id += 1;
        let (cw, ch) = self.cell();
        let opts = SpawnOptions {
            ipc_socket: self.ipc_socket.clone(),
            term: self.cfg.term.clone(),
            command: command.or_else(|| self.cfg.command.clone()),
            integration: self.cfg.shell_integration,
            cwd,
            cols: cols.max(2),
            rows: rows.max(1),
            cell: Some((cw as u16, ch as u16)),
        };
        let mut core = self.cfg.core();
        core.graphics.window_budget = Some(self.image_budget.clone());
        let tx = std::sync::Mutex::new(self.sender.clone());
        core.graphics.waker = Some(proteus_core::graphics::DecodeWaker(std::sync::Arc::new(move || {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(Msg::ImagesDecoded(id));
            }
        })));
        let mut pane = Pane::spawn(id, &opts, core, (cw, ch), self.sender.clone())?;
        pane.term.set_palette(self.cfg.theme.palette());
        self.panes.insert(id, pane);
        self.emit(ScriptEvent::new("pane_opened").pane(id));
        Ok(id)
    }

    /// Split the focused pane (`None`: along its longer side). With a
    /// floating pane focused, `None` opens another floating pane.
    pub(super) fn split(&mut self, axis: Option<Axis>) {
        if let Err(e) = self.split_with(axis, None, None) {
            // Too small to split: nothing happens, as before.
            if e.starts_with("cannot") {
                self.notice_error(e);
            }
        }
    }

    /// Split, running `command` (else the shell) in `cwd` (else the
    /// focused pane's directory). Returns the new pane.
    pub(super) fn split_with(
        &mut self,
        axis: Option<Axis>,
        cwd: Option<PathBuf>,
        command: Option<Vec<OsString>>,
    ) -> Result<PaneId, String> {
        let tab = &self.w.tabs[self.w.active];
        if axis.is_none() && tab.focus_is_float() && command.is_none() {
            return self.new_floating_with(cwd, None);
        }
        let cwd = cwd.or_else(|| self.focused_pane().cwd(&self.hostname));
        self.split_new(axis, move |app, c, r| {
            app.spawn_pane(c, r, cwd, command).map_err(|e| format!("cannot start shell: {e}"))
        })
    }

    /// Split the last tiled pane of the tab with a pane `make` creates for
    /// a size in cells.
    pub(super) fn split_new(
        &mut self,
        axis: Option<Axis>,
        make: impl FnOnce(&mut App, u16, u16) -> Result<PaneId, String>,
    ) -> Result<PaneId, String> {
        let (area, m) = (self.area(), self.layout_metrics());
        let tab = &self.w.tabs[self.w.active];
        let id = tab.last_tiled;
        let Some(r) = tab.full_layout(area, m).rect(id) else { return Err("no pane to split".into()) };
        let (cols, rows) = (r.w / m.cell_w, r.h / m.cell_h);
        // Cells are about twice as tall as wide.
        let axis = axis.unwrap_or(if cols >= 2 * rows { Axis::X } else { Axis::Y });
        if !Tab::can_split(r, axis, m) {
            return Err("the pane is too small to split".into());
        }
        let (c, rw) = match axis {
            Axis::X => (cols / 2, rows),
            Axis::Y => (cols, rows / 2),
        };
        let old = self.focused_id();
        let new = make(self, c as u16, rw as u16)?;
        self.w.tabs[self.w.active].split(new, axis);
        self.relayout();
        self.after_focus(old);
        Ok(new)
    }

    pub(super) fn new_floating(&mut self) {
        if let Err(e) = self.new_floating_with(None, None) {
            self.notice_error(e);
        }
    }

    pub(super) fn new_floating_with(
        &mut self,
        cwd: Option<PathBuf>,
        command: Option<Vec<OsString>>,
    ) -> Result<PaneId, String> {
        let (area, m) = (self.area(), self.layout_metrics());
        let cwd = cwd.or_else(|| self.focused_pane().cwd(&self.hostname));
        let id = self.next_id;
        let f = self.w.tabs[self.w.active].default_float(id, area, m);
        let old = self.focused_id();
        match self.spawn_pane(f.cols as u16, f.rows as u16, cwd, command) {
            Ok(new) => {
                self.w.tabs[self.w.active].add_float(layout::Float { id: new, ..f });
                self.relayout();
                self.after_focus(old);
                Ok(new)
            }
            Err(e) => Err(format!("cannot start shell: {e}")),
        }
    }

    pub(super) fn new_tab(&mut self) {
        if let Err(e) = self.new_tab_with(None, None) {
            self.notice_error(e);
        }
    }

    pub(super) fn new_tab_with(
        &mut self,
        cwd: Option<PathBuf>,
        command: Option<Vec<OsString>>,
    ) -> Result<PaneId, String> {
        let cwd = cwd.or_else(|| self.focused_pane().cwd(&self.hostname));
        self.new_tab_new(move |app, c, r| {
            app.spawn_pane(c, r, cwd, command).map_err(|e| format!("cannot start shell: {e}"))
        })
    }

    /// A new tab after the current one with a pane `make` creates.
    pub(super) fn new_tab_new(
        &mut self,
        make: impl FnOnce(&mut App, u16, u16) -> Result<PaneId, String>,
    ) -> Result<PaneId, String> {
        let (cols, rows) = {
            let t = &self.focused_pane().term;
            (t.cols() as u16, t.lines() as u16)
        };
        let old = self.focused_id();
        let id = make(self, cols, rows)?;
        self.w.tabs.insert(self.w.active + 1, Tab::new(id));
        self.w.active += 1;
        self.relayout();
        self.after_focus(old);
        Ok(id)
    }

    /// Next (or previous) pane across the tabs of the window, in each tab's
    /// focus order (tiled, then visible floats); wraps past the last tab.
    pub(super) fn focus_global(&mut self, forward: bool) {
        let area = self.area();
        let order: Vec<(usize, PaneId)> = (0..self.w.tabs.len())
            .flat_map(|ti| {
                let t = &self.w.tabs[ti];
                let l = t.layout(area, self.tab_metrics(t));
                let ids: Vec<PaneId> = l.panes.iter().map(|p| p.0).chain(l.floats.iter().map(|f| f.id)).collect();
                ids.into_iter().map(move |id| (ti, id))
            })
            .collect();
        let here = (self.w.active, self.focused_id());
        let Some(i) = order.iter().position(|&p| p == here) else { return };
        let n = order.len();
        let (ti, id) = order[if forward { (i + 1) % n } else { (i + n - 1) % n }];
        self.w.active = ti;
        self.w.tabs[ti].set_focus(id);
        self.relayout();
    }

    /// Next visible pane (tiled in order, then floats), wrapping.
    pub(super) fn focus_next(&mut self) {
        let view = self.view();
        let order: Vec<PaneId> = view.panes.iter().map(|p| p.0).chain(view.floats.iter().map(|f| f.id)).collect();
        let fid = self.focused_id();
        if let Some(i) = order.iter().position(|&p| p == fid) {
            let next = order[(i + 1) % order.len()];
            self.w.tabs[self.w.active].set_focus(next);
        }
    }

    // --- windows and moving tabs between them ----------------------------------

    /// A new window with a shell in the directory of the focused pane.
    pub(super) fn open_window(&mut self) {
        let cwd = self.focused_pane().cwd(&self.hostname);
        let from = self.w.id;
        if let Err(e) = self.new_window() {
            return self.notice_error(format!("cannot open a window: {e}"));
        }
        let (cols, rows) = {
            let m = self.layout_metrics_default();
            let a = self.area();
            ((a.w / m.0).max(2) as u16, (a.h / m.1).max(1) as u16)
        };
        match self.spawn_pane(cols, rows, cwd, None) {
            Ok(id) => {
                self.w.tabs.push(Tab::new(id));
                self.w.active = 0;
                self.relayout();
                self.update_title();
            }
            Err(e) => {
                self.destroy_window();
                self.activate(from);
                self.notice_error(format!("cannot start shell: {e}"));
            }
        }
    }

    fn layout_metrics_default(&self) -> (i32, i32) {
        let (w, h) = self.cell();
        (w as i32, h as i32)
    }

    /// Move the tab holding `pane` from window `from` to window `to` (a new
    /// window if `None`), at index `at` (default: the end). A window left
    /// without tabs closes.
    pub(super) fn move_tab(&mut self, from: WinId, pane: PaneId, to: Option<WinId>, at: Option<usize>) {
        if !self.activate(from) {
            return;
        }
        let Some(i) = self.w.tabs.iter().position(|t| t.contains(pane)) else { return };
        if to == Some(from) {
            // Reorder within the window.
            let j = at.unwrap_or(self.w.tabs.len() - 1).min(self.w.tabs.len() - 1);
            let t = self.w.tabs.remove(i);
            self.w.tabs.insert(j, t);
            self.w.active = j;
            self.w.dirty = true;
            return;
        }
        if to.is_none() && self.w.tabs.len() == 1 {
            return; // already alone in its window
        }
        if self.w.copy.as_ref().is_some_and(|c| self.w.tabs[i].contains(c.pane)) || self.w.mode == Mode::Scroll {
            self.set_mode(Mode::Normal);
        }
        let old = self.focused_id();
        let tab = self.w.tabs.remove(i);
        if self.w.active > i || self.w.active >= self.w.tabs.len() {
            self.w.active = self.w.active.saturating_sub(1);
        }
        let src_empty = self.w.tabs.is_empty();
        if !src_empty {
            self.relayout();
            self.after_focus(old);
        }
        let target = match to {
            Some(t) if self.activate(t) => t,
            _ => match self.new_window() {
                Ok(t) => t,
                Err(e) => {
                    // Put it back.
                    self.activate(from);
                    self.w.tabs.insert(i.min(self.w.tabs.len()), tab);
                    return self.notice_error(format!("cannot open a window: {e}"));
                }
            },
        };
        let j = at.unwrap_or(self.w.tabs.len()).min(self.w.tabs.len());
        self.w.tabs.insert(j, tab);
        self.w.active = j;
        self.relayout();
        self.update_title();
        if src_empty {
            self.activate(from);
            self.destroy_window();
            self.activate(target);
        }
    }

    /// Take the focused pane out of its tab into a tab of a new window.
    pub(super) fn pane_to_new_window(&mut self) {
        let id = self.focused_id();
        let from = self.w.id;
        if self.w.tabs[self.w.active].panes().len() == 1 {
            return self.move_tab(from, id, None, None);
        }
        if !self.w.tabs[self.w.active].remove(id) {
            return;
        }
        self.relayout();
        self.after_focus(id);
        match self.new_window() {
            Ok(_) => {
                self.w.tabs.push(Tab::new(id));
                self.relayout();
                self.update_title();
            }
            Err(e) => {
                self.activate(from);
                self.w.tabs.insert(self.w.active + 1, Tab::new(id));
                self.notice_error(format!("cannot open a window: {e}"));
            }
        }
    }

    /// Ask before closing a window's panes if something runs in them.
    pub(super) fn request_close_window(&mut self) {
        let ids: Vec<PaneId> = self.w.tabs.iter().flat_map(|t| t.panes()).collect();
        if self.parked.is_empty() {
            return self.request_quit();
        }
        self.request_close(ids, "ui-this-window");
    }

    /// Names of programs that closing these panes would kill.
    fn busy(&self, ids: &[PaneId]) -> Vec<String> {
        if !self.cfg.confirm_close {
            return Vec::new();
        }
        ids.iter().filter_map(|id| self.panes.get(id)?.busy(&self.cfg.confirm_close_ignore)).collect()
    }

    pub(super) fn request_close(&mut self, ids: Vec<PaneId>, what: &str) {
        let busy = self.busy(&ids);
        if busy.is_empty() {
            for id in ids {
                self.close_pane(id);
            }
        } else {
            self.w.prompts.push_back(Prompt::Close(
                ids,
                self.cfg
                    .i18n
                    .t("prompt-close-busy", &[("what", &self.cfg.i18n.t(what, &[])), ("programs", &busy.join(", "))]),
            ));
            self.w.dirty = true;
        }
    }

    pub(super) fn request_quit(&mut self) {
        let ids: Vec<PaneId> = self.panes.keys().copied().collect();
        let busy = self.busy(&ids);
        if busy.is_empty() {
            self.quit_requested = true;
            self.exit = true;
        } else if !self.w.prompts.iter().any(|p| matches!(p, Prompt::Quit(_))) {
            self.w
                .prompts
                .push_back(Prompt::Quit(self.cfg.i18n.t("prompt-close-window", &[("programs", &busy.join(", "))])));
            self.w.dirty = true;
        }
    }

    /// Remove a pane (its process exited or the user closed it). The last
    /// pane in a tab closes the tab, the last tab its window (or starts a
    /// new shell, with `panes.keep_last_shell`); the last window ends the
    /// program.
    pub(super) fn close_pane(&mut self, id: PaneId) {
        if let Some(p) = self.panes.get_mut(&id) {
            match p.exit_status() {
                Some(status) => tracing::info!(pane = id, status, "pane exited"),
                None => tracing::info!(pane = id, running = p.pty.is_some(), "pane closed"),
            }
        }
        if !self.activate_pane(id) {
            if let Some(p) = self.panes.remove(&id) {
                self.surface_pane_gone(p.surface);
                self.emit(ScriptEvent::new("pane_closed").pane(id));
            }
            return;
        }
        let Some(ti) = self.w.tabs.iter().position(|t| t.contains(id)) else { return };
        let last_tab = self.w.tabs.len() == 1 && self.w.tabs[0].panes().len() == 1;
        if last_tab && self.parked.is_empty() && !self.cfg.keep_last_shell {
            if let Some(p) = self.panes.get_mut(&id) {
                self.exit_code = p.finish();
            }
            self.exit = true;
            return;
        }
        let old = self.focused_id();
        if !self.w.tabs[ti].remove(id) {
            self.w.tabs.remove(ti);
            if self.w.active > ti {
                self.w.active -= 1;
            }
            self.w.active = self.w.active.min(self.w.tabs.len().saturating_sub(1));
        }
        if let Some(p) = self.panes.remove(&id) {
            self.surface_pane_gone(p.surface);
        }
        self.emit(ScriptEvent::new("pane_closed").pane(id));
        self.w.prompts.retain(|p| match p {
            Prompt::ClipboardRead(p, ..) | Prompt::Paste(p, _) => *p != id,
            _ => true,
        });
        if self.w.mouse.selecting == Some(id) || self.w.mouse.reported.is_some_and(|r| r.0 == id) {
            self.w.mouse = Mouse { pos: self.w.mouse.pos, ..Mouse::default() };
        }
        if self.w.tabs.is_empty() {
            if self.cfg.keep_last_shell {
                if let Err(e) = self.open_default(None) {
                    tracing::error!("cannot start shell: {e}");
                    self.destroy_window();
                }
            } else {
                self.destroy_window();
            }
            return;
        }
        self.relayout();
        self.after_focus(old);
    }

    /// Bookkeeping after the focused pane may have changed (`old` was
    /// focused before): focus reports, window title, cursor.
    pub(super) fn after_focus(&mut self, old: PaneId) {
        let new = self.focused_id();
        if new != old {
            self.w.hints = None;
            self.unfocus_surface();
        }
        if self.w.focused
            && let Some(p) = self.panes.get_mut(&new)
        {
            p.bell = false;
        }
        if self.w.copy.as_ref().is_some_and(|c| c.pane != new) || (self.w.mode == Mode::Scroll && self.w.copy.is_none())
        {
            self.set_mode(Mode::Normal);
        }
        if new != old && self.w.focused {
            if let Some(p) = self.panes.get_mut(&old)
                && let Some(b) = input::encode_focus(false, p.term.mode())
            {
                p.write(b);
            }
            let p = self.focused_pane_mut();
            if let Some(b) = input::encode_focus(true, p.term.mode()) {
                p.write(b);
            }
        }
        self.update_title();
        self.restart_blink();
        self.w.dirty = true;
    }
}
