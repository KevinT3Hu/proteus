//! Bound actions, scrolling, and the `PROTEUS_ACTIONS` debug aid.

use super::*;

impl App {
    pub(super) fn run_action(&mut self, action: Action) {
        let (area, m) = (self.area(), self.layout_metrics());
        let old = self.focused_id();
        let n = self.w.tabs.len();
        let tab = &mut self.w.tabs[self.w.active];
        match action {
            // The key itself is sent by `key()`.
            Action::Mode(m) => self.set_mode(m),
            Action::Passthrough => {}
            Action::Focus(d) => {
                tab.focus_dir(area, m, d);
            }
            Action::FocusOrTab(d) => {
                if !tab.focus_dir(area, m, d) {
                    match d {
                        Dir::Left if self.w.active > 0 => self.w.active -= 1,
                        Dir::Right if self.w.active + 1 < n => self.w.active += 1,
                        _ => {}
                    }
                }
            }
            Action::Split(axis) => self.split(Some(axis)),
            Action::NewPane => self.split(None),
            Action::ClosePane => self.request_close(vec![old], "ui-this-pane"),
            Action::Zoom => {
                if tab.tiled().len() > 1 && !tab.focus_is_float() {
                    tab.zoomed = !tab.zoomed;
                    self.relayout();
                }
            }
            Action::PaneToNewTab => {
                if tab.remove(old) {
                    self.w.tabs.insert(self.w.active + 1, Tab::new(old));
                    self.w.active += 1;
                    self.relayout();
                }
            }
            Action::Resize(d, cells) => {
                if tab.resize(area, m, d, cells) {
                    self.relayout();
                }
            }
            Action::Equalize => {
                tab.equalize();
                self.relayout();
            }
            Action::Swap(d) => {
                if tab.swap(area, m, d, 1) {
                    self.relayout();
                }
            }
            Action::NewTab => self.new_tab(),
            Action::GotoTab(i) if (i as usize) < n => self.w.active = i as usize,
            Action::GotoTab(_) => {}
            Action::PrevTab => self.w.active = (self.w.active + n - 1) % n,
            Action::NextTab => self.w.active = (self.w.active + 1) % n,
            Action::CloseTab => {
                let ids = tab.panes();
                self.request_close(ids, "ui-this-tab");
            }
            Action::Scroll(s) => self.scroll(s),
            Action::Copy => {
                if let Some(text) = self.focused_pane().term.selection_text().filter(|t| !t.is_empty()) {
                    self.set_selection(ClipboardKind::Clipboard, text);
                }
            }
            Action::Paste => self.paste(ClipboardKind::Clipboard),
            Action::PastePrimary => self.paste(ClipboardKind::Primary),
            Action::Font(0) => self.set_font_size(self.cfg.font_size),
            Action::Font(d) => self.set_font_size(self.w.font_size + d as f32),
            Action::Quit => self.request_close_window(),
            Action::CopyMove(m) => self.copy_move(m),
            Action::CopySelect(k) => self.copy_select(k),
            Action::CopyYank => self.copy_yank(),
            Action::CopyEscape => self.copy_escape(),
            Action::SearchStart => self.search_start(),
            Action::SearchUp => self.search_step(true),
            Action::SearchDown => self.search_step(false),
            Action::RenameTab => {
                let current = self.w.tabs[self.w.active].name.clone().unwrap_or_default();
                self.open_input(Purpose::RenameTab(old), &current, "empty: automatic title".into());
            }
            Action::RenamePane => {
                let current = self.focused_pane().name.clone().unwrap_or_default();
                self.open_input(Purpose::RenamePane(old), &current, "empty: automatic title".into());
            }
            Action::ResetRenderer => self.recover_gpu(),
            Action::ExportScrollback => {
                self.open_input(Purpose::ExportScrollback(old), "", self.cfg.i18n.t("hint-export-scrollback", &[]));
            }
            Action::SaveLayout => {
                let hint = spec::layouts_dir().map(|d| format!("→ {}/NAME.lua", d.display())).unwrap_or_default();
                self.open_input(Purpose::SaveLayout, "", hint);
            }
            Action::LoadLayout => {
                let names = Self::layout_names();
                let hint = if names.is_empty() { "no saved layouts".into() } else { names.join("  ") };
                self.open_input(Purpose::LoadLayout, "", hint);
            }
            Action::FocusNext => self.focus_next(),
            Action::FocusGlobal(forward) => self.focus_global(forward),
            Action::MoveTab(right) => {
                let (i, n) = (self.w.active, self.w.tabs.len());
                let j = if right { (i + 1) % n } else { (i + n - 1) % n };
                if i != j {
                    let t = self.w.tabs.remove(i);
                    self.w.tabs.insert(j, t);
                    self.w.active = j;
                }
            }
            Action::ToggleFloating => {
                if tab.floats.is_empty() {
                    self.new_floating();
                } else {
                    tab.toggle_floats();
                    self.relayout();
                }
            }
            Action::ToggleEmbed => {
                if tab.toggle_float(area, m) {
                    self.relayout();
                }
            }
            Action::NewFloating => self.new_floating(),
            Action::ToggleStack => {
                if tab.toggle_stack() {
                    self.relayout();
                }
            }
            Action::TogglePaneTitles => {
                self.cfg.pane_titles = match self.cfg.pane_titles {
                    PaneTitles::Never => PaneTitles::Always,
                    PaneTitles::Always | PaneTitles::Auto => PaneTitles::Never,
                };
                self.relayout();
            }
            Action::NewWindow => self.open_window(),
            Action::TabToNewWindow => {
                let from = self.w.id;
                self.move_tab(from, old, None, None);
            }
            Action::PaneToNewWindow => self.pane_to_new_window(),
            Action::TabToNextWindow => {
                let from = self.w.id;
                let ids = self.window_ids();
                let next =
                    ids.iter().copied().find(|&i| i > from).or_else(|| ids.first().copied().filter(|&i| i != from));
                self.move_tab(from, old, next, None);
            }
            Action::Palette => self.open_palette(),
            Action::OpenConfig => self.open_config_editor(),
            Action::Ssh(action) => self.ssh_action(action),
            Action::BlockCopy(p) => self.block_copy(p),
            Action::BlockSelect(p) => self.block_select(p),
            Action::BlockFold => self.block_fold(),
            Action::BlockUnfoldAll => self.unfold_all(),
            Action::BlockRerun => self.block_rerun(),
            Action::BlockInfo => self.block_info(),
            Action::Hints(h) => self.hints_start(h),
            Action::ViewImage => self.view_last_image(),
            Action::Lua(f) => self.queue_lua(f),
            Action::Plugin(i) => self.run_plugin_action(i),
            Action::TogglePrivate => {
                if let Some(p) = self.panes.get_mut(&old) {
                    p.private = !p.private;
                }
            }
        }
        if !self.exit {
            self.after_focus(old);
        }
    }

    fn scroll(&mut self, s: Scroll) {
        let t = &mut self.focused_pane_mut().term;
        let page = t.lines() as isize;
        match s {
            Scroll::Lines(n) => t.scroll_display(-n as isize),
            Scroll::HalfPages(n) => t.scroll_display(-(n as isize) * (page / 2).max(1)),
            Scroll::Pages(n) => t.scroll_display(-(n as isize) * (page - 1).max(1)),
            Scroll::Top => t.scroll_display(isize::MAX / 2),
            Scroll::Bottom => t.scroll_to_bottom(),
            Scroll::Prompt(forward) => {
                t.scroll_to_prompt(forward);
            }
        }
    }

    /// Debug aid: `PROTEUS_ACTIONS=pane.split_right,tab.new,...` runs
    /// actions once the window is up (`PROTEUS_ACTIONS_AFTER=secs` waits
    /// first). `type:TEXT` types into the input line (or the pane),
    /// `key:enter` / `key:esc` press a key in the input line, `key:y`
    /// answers a prompt, `key:X` types X in hint mode, `sleep:SECS` waits.
    pub(super) fn run_env_actions(&mut self) {
        let Ok(list) = std::env::var("PROTEUS_ACTIONS") else { return };
        let after = std::env::var("PROTEUS_ACTIONS_AFTER").ok().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let steps: Vec<String> = list.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
        self.run_env_steps(steps, after);
    }

    fn run_env_steps(&mut self, steps: Vec<String>, after: f64) {
        let run = move |app: &mut App| {
            // The first window, whichever was drawn last.
            app.activate(1);
            for (i, name) in steps.iter().enumerate() {
                let name = name.as_str();
                if let Some(secs) = name.strip_prefix("sleep:").and_then(|s| s.parse::<f64>().ok()) {
                    app.run_env_steps(steps[i + 1..].to_vec(), secs.max(0.001));
                    return;
                }
                if let Some(c) =
                    name.strip_prefix("key:").filter(|_| app.w.hints.is_some()).and_then(|k| k.chars().next())
                {
                    app.hint_key(input::Key::Char(c), input::Mods::empty());
                    continue;
                }
                if let Some(s) = app.w.surface_focus
                    && let Some(k) = name.strip_prefix("key:")
                {
                    let key = match k {
                        "enter" => input::Key::Enter,
                        "esc" => input::Key::Escape,
                        "up" => input::Key::Up,
                        "down" => input::Key::Down,
                        _ => input::Key::Char(k.chars().next().unwrap_or(' ')),
                    };
                    let text = if k.chars().count() == 1 { k } else { "" };
                    app.surface_key(s, key, text, false);
                    continue;
                }
                if let Some(k) = name.strip_prefix("key:").filter(|_| app.w.viewer.is_some()) {
                    let key = match k {
                        "esc" => input::Key::Escape,
                        "left" => input::Key::Left,
                        "right" => input::Key::Right,
                        "up" => input::Key::Up,
                        "down" => input::Key::Down,
                        _ => input::Key::Char(k.chars().next().unwrap_or(' ')),
                    };
                    app.viewer_key(key, input::Mods::empty());
                    continue;
                }
                if let Some(text) = name.strip_prefix("type:") {
                    // `\n` stands for Enter.
                    let text = &text.replace("\\n", "\r");
                    if app.w.input.is_some() {
                        app.input_text(text);
                    } else {
                        app.focused_pane_mut().write(text.as_bytes());
                    }
                    continue;
                }
                match (name, Action::from_name(name)) {
                    ("key:enter", _) => app.input_key(input::Key::Enter, input::Mods::empty(), ""),
                    ("key:esc", _) => app.input_key(input::Key::Escape, input::Mods::empty(), ""),
                    ("key:y", _) => app.answer_prompt(input::Key::Char('y')),
                    (_, Some(a)) => app.dispatch(a),
                    (_, None) => tracing::warn!("PROTEUS_ACTIONS: unknown action `{name}`"),
                }
            }
        };
        if after <= 0.0 {
            return run(self);
        }
        let Ok(after) = Duration::try_from_secs_f64(after) else {
            return tracing::warn!("PROTEUS_ACTIONS: invalid delay {after}");
        };
        let mut run = Some(run);
        let _ = self.handle.insert_source(Timer::from_duration(after), move |_, _, app| {
            if let Some(f) = run.take() {
                f(app);
            }
            TimeoutAction::Drop
        });
    }
}
