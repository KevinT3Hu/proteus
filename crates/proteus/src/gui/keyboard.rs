//! Key routing (prompts, viewer, input bar, hints, bindings, surfaces, panes)
//! and the cursor blink timer.

use super::*;

impl App {
    pub(super) fn key(&mut self, event: KeyEvent, action: KeyAction) {
        self.activate_kbd();
        if keys::is_modifier(event.keysym) {
            // Only for applications that asked for every key (kitty flag 8),
            // and only when nothing of ours takes the keys.
            let ours = !self.w.prompts.is_empty()
                || self.w.input.is_some()
                || self.w.hints.is_some()
                || self.w.viewer.is_some()
                || self.w.surface_focus.is_some()
                || !self.w.mode.passes_keys();
            let wants =
                self.focused_pane().term.keyboard_flags().contains(proteus_core::KeyboardFlags::REPORT_ALL_AS_ESCAPES);
            if ours || !wants {
                return;
            }
        }
        let mods = keys::mods(&self.mods);
        let Some((key, shifted)) = self.levels.key(event.keysym, event.raw_code) else { return };
        // The key as our own UI sees it (keypad keys as their main keys);
        // the application gets `pane_key`.
        let pane_key = key;
        let key = match key {
            input::Key::Keypad(k) => k.main_key(event.utf8.as_deref().unwrap_or("")),
            k => k,
        };
        if action == KeyAction::Release {
            // The press went to the multiplexer: so does the release.
            if let Some(i) = self.consumed.iter().position(|&c| c == event.raw_code) {
                self.consumed.swap_remove(i);
                return;
            }
        } else {
            self.restart_blink();
            if !self.w.prompts.is_empty() {
                self.consume(event.raw_code);
                if action == KeyAction::Press {
                    self.answer_prompt(key);
                }
                return;
            }
            if self.w.viewer.is_some() {
                self.consume(event.raw_code);
                // `+` is typed as Shift+`=` on most layouts.
                let key = match (key, shifted) {
                    (input::Key::Char(_), Some(c)) if mods.contains(input::Mods::SHIFT) && "+".contains(c) => {
                        input::Key::Char(c)
                    }
                    _ => key,
                };
                self.viewer_key(key, mods);
                return;
            }
            if self.w.input.is_some() {
                self.consume(event.raw_code);
                self.input_key(key, mods, event.utf8.as_deref().unwrap_or(""));
                return;
            }
            if self.w.hints.is_some() {
                self.consume(event.raw_code);
                if action == KeyAction::Press {
                    self.hint_key(key, mods);
                }
                return;
            }
            match self.keymap.lookup_shifted(self.w.mode, key, mods, shifted) {
                Lookup::Pass => {}
                Lookup::Swallow => {
                    self.consume(event.raw_code);
                    return;
                }
                Lookup::Do(Action::Passthrough) => self.set_mode(self.keymap.after(self.w.mode, Action::Passthrough)),
                // Not meaningful here (Shift+PgUp in full-screen apps): the app gets the key.
                Lookup::Do(a) if !self.applies(a) => {}
                Lookup::Do(a) => {
                    self.consume(event.raw_code);
                    // Holding a key repeats movement, not structural changes.
                    let repeatable = matches!(
                        a,
                        Action::Focus(_)
                            | Action::FocusOrTab(_)
                            | Action::FocusGlobal(_)
                            | Action::MoveTab(_)
                            | Action::Resize(..)
                            | Action::Swap(_)
                            | Action::Scroll(_)
                            | Action::Font(_)
                            | Action::CopyMove(_)
                            | Action::SearchUp
                            | Action::SearchDown
                    );
                    if action == KeyAction::Press || repeatable {
                        self.dispatch(a);
                    }
                    return;
                }
            }
        }
        let text = event.utf8.as_deref().unwrap_or("");
        // Keys not bound in the mode go to a focused plugin surface.
        if let Some(s) = self.key_surface() {
            if action != KeyAction::Release {
                self.consume(event.raw_code);
                self.surface_key(s, key, text, action == KeyAction::Repeat);
            }
            return;
        }
        let ev = input::KeyEvent { key: pane_key, mods, action, text, shifted };
        let p = self.focused_pane_mut();
        if let Some(bytes) = input::encode_key(&ev, p.term.mode(), p.term.keyboard_flags()) {
            if action != KeyAction::Release {
                p.term.scroll_to_bottom();
            }
            p.write(&bytes);
        }
    }

    /// Run a bound action and move to the mode that follows it (unless the
    /// action switched modes itself).
    pub(super) fn dispatch(&mut self, a: Action) {
        let before = self.w.mode;
        let next = self.keymap.after(before, a);
        self.run_action(a);
        if self.w.mode == before && !self.exit {
            self.set_mode(next);
        }
        self.w.dirty = true;
    }

    fn consume(&mut self, raw_code: u32) {
        if !self.consumed.contains(&raw_code) {
            self.consumed.push(raw_code);
        }
    }

    /// Scrolling bound in Normal/Locked (Shift+PgUp, ...) applies to the
    /// scrollback only; full-screen applications get those keys.
    fn applies(&self, a: Action) -> bool {
        !(matches!(a, Action::Scroll(_)) && self.w.mode.passes_keys() && self.focused_pane().term.is_alt_screen())
    }

    // --- cursor blink ----------------------------------------------------------

    fn wants_blink(&self) -> bool {
        let t = &self.focused_pane().term;
        let app = match self.cfg.cursor_blink {
            Blink::Never => false,
            Blink::Always => true,
            Blink::App => t.mode().contains(TermMode::CURSOR_BLINK),
        };
        app && self.w.focused && t.cursor_visible() && self.last_input.elapsed() < BLINK_IDLE
    }

    /// Input happened: show the cursor and restart the blink cycle.
    pub(super) fn restart_blink(&mut self) {
        self.last_input = Instant::now();
        if self.blink_off {
            self.blink_off = false;
            self.w.dirty = true;
        }
        if let Some(t) = self.blink_timer.take() {
            self.handle.remove(t);
        }
        self.ensure_blink();
    }

    /// Run the blink timer while blinking is wanted; it stops itself (with
    /// the cursor on) when it no longer is, so an idle window never wakes.
    pub(super) fn ensure_blink(&mut self) {
        if self.blink_timer.is_some() || !self.wants_blink() {
            return;
        }
        self.blink_timer = self
            .handle
            .insert_source(Timer::from_duration(BLINK), |_, _, app: &mut App| {
                app.activate_kbd();
                if !app.exit && app.wants_blink() {
                    app.blink_off = !app.blink_off;
                    app.w.dirty = true;
                    TimeoutAction::ToDuration(BLINK)
                } else {
                    app.blink_timer = None;
                    if app.blink_off {
                        app.blink_off = false;
                        app.w.dirty = true;
                    }
                    TimeoutAction::Drop
                }
            })
            .ok();
    }
}
