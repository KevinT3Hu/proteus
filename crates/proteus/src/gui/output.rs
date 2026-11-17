//! PTY output and the terminal events it raises: titles, clipboard,
//! bells, desktop notifications and revealing panes.

use super::*;

impl App {
    pub(super) fn pty_output(&mut self, id: PaneId, bytes: &[u8]) {
        // Events (title, OSC 52 prompts) concern the pane's window.
        self.activate_pane(id);
        let focused = id == self.focused_id();
        let Some(p) = self.panes.get_mut(&id) else { return };
        p.update_locality(id, &self.session_scanner);
        p.term.advance(bytes);
        p.deliver_run(workspace::RUN_TIMEOUT);
        let reply = p.term.take_pty_output();
        if !reply.is_empty() {
            p.write(&reply);
        }
        let events = p.term.take_events();
        // A command started or ended: the foreground process changed.
        if events.iter().any(|e| matches!(e, Event::PromptStarted { .. } | Event::CommandStarted { .. })) {
            p.expire_proc_title();
        }
        let mut title = p.refresh_proc_title();
        let block_changed = events.iter().any(|e| {
            matches!(e, Event::PromptStarted { .. } | Event::CommandStarted { .. } | Event::CommandFinished { .. })
        });
        for ev in events {
            match ev {
                Event::TitleChanged(t) => {
                    title = true;
                    self.emit(ScriptEvent::new("title_changed").pane(id).with("title", Val::Str(t)));
                }
                Event::CwdChanged(c) => {
                    self.emit(ScriptEvent::new("cwd_changed").pane(id).with("cwd", Val::Str(c.path)));
                }
                Event::CommandStarted { command, .. } => {
                    let command = command.map_or(Val::Nil, Val::Str);
                    self.emit(ScriptEvent::new("command_started").pane(id).with("command", command));
                }
                Event::ClipboardStore { kind, text } => self.set_selection(kind, text),
                Event::ClipboardLoad { kind, terminator } => self.clipboard_load(id, kind, terminator),
                Event::Bell => {
                    self.emit(ScriptEvent::new("bell").pane(id));
                    self.bell(id)
                }
                Event::Notification { title, body } => {
                    if self.cfg.notify_osc {
                        let summary =
                            title.unwrap_or_else(|| self.panes.get(&id).map_or(APP_NAME, |p| p.title()).to_string());
                        self.notifier.send(id, summary, body);
                    }
                }
                Event::CommandFinished { block, exit_code, duration } => {
                    let command = self.panes.get(&id).and_then(|p| p.term.blocks().get(block)?.command.clone());
                    self.emit(
                        ScriptEvent::new("command_finished")
                            .pane(id)
                            .with("command", command.map_or(Val::Nil, Val::Str))
                            .with("exit_code", exit_code.map_or(Val::Nil, |c| Val::Int(c as i64)))
                            .with("duration", duration.map_or(Val::Nil, |d| Val::Num(d.as_secs_f64()))),
                    );
                    self.command_finished(id, block, exit_code, duration)
                }
                _ => {}
            }
        }
        if block_changed {
            self.block_state_changed(id);
        }
        if title {
            self.update_title();
            if self.tab_bar_visible() {
                self.w.dirty = true;
            }
        }
        if focused {
            self.ensure_blink();
        }
    }

    // --- bell and notifications -----------------------------------------------

    /// Window holding pane `id`, without making it current.
    pub(super) fn win_with_pane(&self, id: PaneId) -> Option<&Win> {
        std::iter::once(&self.w).chain(&self.parked).find(|w| w.tabs.iter().any(|t| t.contains(id)))
    }

    /// Whether the user is looking at pane `id`: its window has focus and
    /// it is the focused pane of the active tab.
    pub(super) fn pane_watched(&self, id: PaneId) -> bool {
        self.win_with_pane(id).is_some_and(|w| w.focused && w.tabs.get(w.active).is_some_and(|t| t.focus == id))
    }

    fn bell(&mut self, id: PaneId) {
        let watched = self.pane_watched(id);
        let visual = self.cfg.visual_bell;
        let Some(p) = self.panes.get_mut(&id) else { return };
        if visual {
            p.flash_until = Some(Instant::now() + FLASH);
        }
        if !watched {
            p.bell = true;
        }
        self.w.dirty = true;
    }

    fn command_finished(
        &mut self,
        id: PaneId,
        block: proteus_core::blocks::BlockId,
        exit_code: Option<i32>,
        duration: Option<Duration>,
    ) {
        let (Some(min), Some(d)) = (self.cfg.notify_after, duration) else { return };
        if d < min || self.pane_watched(id) {
            return;
        }
        let Some(p) = self.panes.get(&id) else { return };
        let command = p.term.blocks().get(block).and_then(|b| b.command.clone()).filter(|c| !c.is_empty());
        let summary = match exit_code {
            Some(c) if c != 0 => self.cfg.i18n.t("ui-command-failed-exit-code", &[("code", &c.to_string())]),
            _ => self.cfg.i18n.t("ui-command-finished", &[]),
        };
        let mut body = command.map(|c| c.lines().next().unwrap_or("").to_string()).unwrap_or_default();
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&self.cfg.i18n.t("ui-took-duration", &[("duration", &blocks::human_duration(d))]));
        self.notifier.send(id, summary, body);
    }

    /// Show pane `id`: its window, tab and focus. A window other than the
    /// keyboard window is raised with xdg-activation (`token` from a
    /// notification daemon, else one we request).
    pub(super) fn reveal_pane(&mut self, id: PaneId, token: Option<String>) {
        if !self.activate_pane(id) {
            return;
        }
        let old = self.focused_id();
        if let Some(i) = self.w.tabs.iter().position(|t| t.contains(id)) {
            self.w.active = i;
            self.w.tabs[i].set_focus(id);
            self.relayout();
            self.after_focus(old);
        }
        let target = self.w.id;
        match (&self.activation, token) {
            (Some(a), Some(t)) => a.activate::<App>(self.w.window.wl_surface(), t),
            (Some(_), None) if self.kbd_win != Some(target) => self.request_activation(target),
            _ => {}
        }
        self.w.dirty = true;
    }

    /// Ask the compositor to focus window `target`, vouched for by the
    /// window that has the keyboard.
    pub(super) fn request_activation(&mut self, target: WinId) {
        let Some(a) = &self.activation else { return };
        let from = self.kbd_win.and_then(|k| std::iter::once(&self.w).chain(&self.parked).find(|w| w.id == k));
        let seat = self.seat_state.seats().next();
        let data = RequestData {
            app_id: Some(APP_ID.into()),
            seat_and_serial: seat.map(|s| (s, self.serial)),
            surface: from.map(|w| w.window.wl_surface().clone()),
            udata: target,
        };
        a.request_token(&self.qh, data);
    }

    pub(super) fn notification_clicked(&mut self, id: PaneId, token: Option<String>) {
        self.reveal_pane(id, token);
        self.activate_kbd();
    }
}
