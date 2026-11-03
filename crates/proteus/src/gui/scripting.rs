//! Lua scripts in the GUI (doc 05): the event queue that hooks run from,
//! the [`Host`] the scripts reach the terminal through, status segments,
//! timers, messages ("toasts") and reloading `init.lua` when it changes.
//!
//! Scripts never run in the middle of an event handler: handlers queue
//! calls ([`App::emit`], [`Action::Lua`]) and the main loop runs them in
//! [`App::run_scripts`] before drawing. While a script runs it is taken out
//! of the [`App`], so events it causes are queued and run in the same pass.

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use proteus_core::BlockPart;
use proteus_core::blocks::BlockState;
use smithay_client_toolkit::reexports::calloop::generic::Generic;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{Interest, Mode as PollMode, PostAction};

use super::{App, Fonts, render};
use crate::config::{self, Config};
use crate::layout::{Axis, PaneId};
use crate::modes::Action;
use crate::script::{BlockInfo, EVENTS, FnId, Host, PaneInfo, Script, ScriptEvent, Spawn, Val};

/// Info messages stay this long; errors twice as long.
const TOAST: Duration = Duration::from_secs(4);
/// Config errors stay up this long.
const CONFIG_ERROR: Duration = Duration::from_secs(15);
/// The same message is not shown again within this time.
const REPEAT: Duration = Duration::from_secs(30);
/// Status segments are brought up to date at least this often.
const SEGMENTS: Duration = Duration::from_secs(1);
/// Wait for writes to settle before reloading the config.
const RELOAD_DELAY: Duration = Duration::from_millis(150);
/// Calls run in one pass at most (hooks that keep causing events).
const MAX_CALLS: usize = 1000;
/// Toasts shown at once.
const MAX_TOASTS: usize = 4;

/// Command-line options that win over the config, also after a reload.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub font: Option<String>,
    pub font_size: Option<f32>,
}

impl Overrides {
    pub fn apply(&self, c: &mut Config) {
        if let Some(f) = &self.font {
            c.font_family = f.clone();
        }
        if let Some(s) = self.font_size {
            c.font_size = s;
        }
    }
}

/// A script call waiting for [`App::run_scripts`].
pub(super) enum ScriptCall {
    /// A bound key or palette entry, on this pane.
    Fn(FnId, Option<PaneId>),
    Event(ScriptEvent),
    /// Timer `id` of script generation `gen` went off.
    Timer(u32, u32),
    /// An event of a `p.ui` surface.
    Ui(u32, proteus_wasm::types::UiEvent),
}

/// A message in the top right corner of a window.
#[derive(Debug, Clone)]
pub(super) struct Toast {
    pub text: String,
    pub error: bool,
    pub until: Instant,
}

/// Scripting state of the [`App`].
#[derive(Default)]
pub(super) struct Scripting {
    /// `None` while a call runs (or if Lua could not start).
    pub script: Option<Script>,
    pub queue: VecDeque<ScriptCall>,
    /// Bumped on every reload: timers and calls of older scripts lapse.
    pub generation: u32,
    /// Events that have hooks (others are not queued).
    pub hooks: Vec<&'static str>,
    /// `p.palette.add` entries.
    pub palette: Vec<(String, FnId)>,
    pub has_segments: bool,
    pub segments_due: bool,
    /// Timers still running: (generation, id).
    pub timers: HashSet<(u32, u32)>,
    /// Focused pane of the keyboard window at the last pass.
    pub last_focus: Option<PaneId>,
    /// When each message was last shown.
    pub shown: HashMap<String, Instant>,
    /// The config file, reloaded when it (or a file next to it) changes.
    pub config_path: Option<PathBuf>,
    pub overrides: Overrides,
    pub reload_at: Option<Instant>,
}

impl App {
    /// Queue an event for its hooks.
    pub(super) fn emit(&mut self, ev: ScriptEvent) {
        self.sc.segments_due = true;
        self.plugins_event(&ev);
        if self.sc.hooks.contains(&ev.name) {
            self.sc.queue.push_back(ScriptCall::Event(ev));
        }
    }

    /// Run a function bound with `p.keymap` on the focused pane.
    pub(super) fn queue_lua(&mut self, f: FnId) {
        let pane = self.focused_id();
        self.sc.queue.push_back(ScriptCall::Fn(f, Some(pane)));
    }

    /// Take over a freshly loaded script: its timers, segments and hooks.
    pub(super) fn install_script(&mut self, script: Script) {
        self.sc.generation += 1;
        self.sc.timers.clear();
        // Calls into the old script's functions lapse with it, and so do
        // its surfaces.
        self.sc.queue.retain(|c| matches!(c, ScriptCall::Event(_)));
        self.drop_config_surfaces();
        self.sc.has_segments = script.has_segments();
        self.sc.segments_due = true;
        self.sc.script = Some(script);
        self.after_script();
        if self.sc.has_segments {
            let generation = self.sc.generation;
            let _ = self.handle.insert_source(Timer::from_duration(SEGMENTS), move |_, _, app: &mut App| {
                if app.sc.generation != generation {
                    return TimeoutAction::Drop;
                }
                app.sc.segments_due = true;
                TimeoutAction::ToDuration(SEGMENTS)
            });
        }
        for w in std::iter::once(&mut self.w).chain(&mut self.parked) {
            w.status = Default::default();
        }
    }

    /// After calls: pick up what the script registered meanwhile (hooks,
    /// palette entries, timers) and its messages.
    fn after_script(&mut self) {
        let Some(script) = self.sc.script.as_ref() else { return };
        self.sc.hooks = EVENTS.iter().copied().filter(|e| script.has_hooks(e)).collect();
        self.sc.palette = script.palette();
        self.sc.has_segments = script.has_segments();
        let timers = script.take_new_timers();
        let messages = script.take_messages();
        let generation = self.sc.generation;
        for t in timers {
            self.sc.timers.insert((generation, t.id));
            let (id, after, repeat) = (t.id, t.after, t.repeat);
            let _ = self.handle.insert_source(Timer::from_duration(after), move |_, _, app: &mut App| {
                if !app.sc.timers.contains(&(generation, id)) {
                    return TimeoutAction::Drop;
                }
                app.sc.queue.push_back(ScriptCall::Timer(generation, id));
                if repeat { TimeoutAction::ToDuration(after) } else { TimeoutAction::Drop }
            });
        }
        for m in messages {
            self.toast(m.text, m.error, None);
        }
    }

    /// Run the queued calls, then bring the status segments up to date.
    pub(super) fn run_scripts(&mut self) {
        let focus = self.kbd_focus();
        if focus != self.sc.last_focus {
            let previous = self.sc.last_focus.filter(|p| self.panes.contains_key(p));
            self.sc.last_focus = focus;
            if let Some(f) = focus {
                let mut ev = ScriptEvent::new("focus_changed").pane(f);
                if let Some(p) = previous {
                    ev = ev.with("previous", Val::Pane(p));
                }
                self.emit(ev);
            }
        }
        if self.sc.queue.is_empty() && !(self.sc.segments_due && self.sc.has_segments) {
            return;
        }
        let Some(script) = self.sc.script.take() else {
            self.sc.queue.clear();
            return;
        };
        let mut n = 0;
        while let Some(call) = self.sc.queue.pop_front() {
            n += 1;
            if n > MAX_CALLS {
                self.sc.queue.clear();
                self.toast("Lua: hooks keep causing events; the rest were dropped".into(), true, None);
                break;
            }
            match call {
                ScriptCall::Fn(f, pane) => script.call_fn(self, f, pane),
                ScriptCall::Event(ev) => script.emit(self, &ev),
                ScriptCall::Timer(generation, id) if generation == self.sc.generation => {
                    if !script.timer_fired(self, id) {
                        self.sc.timers.remove(&(generation, id));
                    }
                }
                ScriptCall::Timer(..) => {}
                ScriptCall::Ui(id, e) => {
                    script.surface_event(self, id, &e);
                }
            }
            self.activate_kbd();
        }
        if std::mem::take(&mut self.sc.segments_due) && script.has_segments() {
            for id in self.window_ids() {
                if !self.activate(id) {
                    continue;
                }
                let pane = self.w.tabs.get(self.w.active).map(|t| t.focus);
                let status = script.segments(self, pane);
                if self.activate(id) && status != self.w.status {
                    let bar = self.tab_bar_visible();
                    self.w.status = status;
                    if bar != self.tab_bar_visible() {
                        self.relayout();
                    }
                    self.w.dirty = true;
                }
            }
            self.activate_kbd();
        }
        self.sc.script = Some(script);
        self.after_script();
    }

    /// Focused pane of the keyboard window (else the current one).
    fn kbd_focus(&self) -> Option<PaneId> {
        let w = self.kbd_win.and_then(|k| std::iter::once(&self.w).chain(&self.parked).find(|w| w.id == k));
        let w = w.unwrap_or(&self.w);
        w.tabs.get(w.active).map(|t| t.focus)
    }

    // --- toasts ----------------------------------------------------------------

    /// Show a message in the keyboard window. Repeats of the same message
    /// within [`REPEAT`] are dropped.
    pub(super) fn toast(&mut self, text: String, error: bool, timeout: Option<Duration>) {
        eprintln!("proteus: {text}");
        let now = Instant::now();
        self.sc.shown.retain(|_, t| now.duration_since(*t) < REPEAT);
        if self.sc.shown.insert(text.clone(), now).is_some() {
            return;
        }
        self.activate_kbd();
        let timeout = timeout.unwrap_or(if error { 2 * TOAST } else { TOAST });
        self.w.toasts.retain(|t| t.text != text);
        self.w.toasts.push(Toast { text, error, until: now + timeout });
        if self.w.toasts.len() > MAX_TOASTS {
            self.w.toasts.remove(0);
        }
        self.w.dirty = true;
    }

    /// Drop expired toasts and wake up for the next one. Returns true if
    /// one went away.
    pub(super) fn expire_toasts(&mut self) -> bool {
        let now = Instant::now();
        let before = self.w.toasts.len();
        self.w.toasts.retain(|t| t.until > now);
        if let Some(next) = self.w.toasts.iter().map(|t| t.until).min() {
            self.schedule(next - now);
        }
        self.w.toasts.len() != before
    }

    /// Toasts, newest on top, in the top right corner of the pane area.
    pub(super) fn toast_labels(
        &self,
        theme: &render::Theme,
    ) -> (Vec<([f32; 4], proteus_core::color::Rgb)>, Vec<render::Label>) {
        let (mut rects, mut labels) = (Vec::new(), Vec::new());
        if self.w.toasts.is_empty() {
            return (rects, labels);
        }
        let (pw, _) = self.physical();
        let p = self.pad() as f32;
        let (cw, ch) = (self.cell().0 as f32, self.cell().1 as f32);
        let cols = ((pw as f32 - 2.0 * p) / cw).max(1.0) as usize;
        let max = (cols * 2 / 3).max(10).min(cols);
        let red = self.focused_pane().term.palette().indexed(1);
        let mut y = self.area().y as f32 + self.pad_y() as f32;
        for t in self.w.toasts.iter().rev() {
            let lines: Vec<String> = t.text.lines().flat_map(|l| wrap(l, max - 2)).take(6).collect();
            let w = lines.iter().map(|l| super::chrome::text_width(l)).max().unwrap_or(0).saturating_add(2).min(max);
            let x = pw as f32 - p - w as f32 * cw;
            let (fg, bg) = if t.error { (theme.bg, red) } else { (theme.fg, theme.bar) };
            let h = lines.len() as f32 * ch;
            rects.push(([x - 1.0, y - 1.0, w as f32 * cw + 2.0, h + 2.0], theme.dim));
            for (i, l) in lines.iter().enumerate() {
                labels.push(render::Label {
                    x,
                    y: y + i as f32 * ch,
                    cols: w,
                    text: format!(" {l}"),
                    fg,
                    bg: Some(bg),
                    bold: false,
                });
            }
            y += h + p;
        }
        (rects, labels)
    }

    // --- status segments ---------------------------------------------------------

    /// Labels of the status segments in the tab bar: left ones after the
    /// tabs, right ones at the end.
    /// Status segments of scripts and plugins: left, right.
    pub(super) fn status_all(&self) -> (Vec<String>, Vec<String>) {
        let (mut l, mut r) = self.w.status.clone();
        let (pl, pr) = self.plugin_status();
        l.extend(pl);
        r.extend(pr);
        (l, r)
    }

    pub(super) fn status_labels(&self, tabs: &[render::Label], theme: &render::Theme) -> Vec<render::Label> {
        let (left, right) = &self.status_all();
        let mut out = Vec::new();
        let (pw, _) = self.physical();
        let p = self.pad() as f32;
        let cw = self.cell().0 as f32;
        let cols = ((pw as f32 - 2.0 * p) / cw).max(1.0) as usize;
        let right_text = right.join("  ");
        let rw = super::chrome::text_width(&right_text).min(cols / 2);
        if rw > 0 {
            out.push(render::Label {
                x: p + (cols - rw) as f32 * cw,
                y: (self.title_bar_h() + self.pad_y() as i32) as f32,
                cols: rw,
                text: right_text,
                fg: theme.fg,
                bg: None,
                bold: false,
            });
        }
        let left_text = left.join("  ");
        let start = tabs.last().map_or(0, |l| ((l.x - p) / cw).round() as usize + l.cols + 2);
        let room = cols.saturating_sub(start + rw + usize::from(rw > 0) * 2);
        let lw = super::chrome::text_width(&left_text).min(room);
        if lw > 0 {
            out.push(render::Label {
                x: p + start as f32 * cw,
                y: (self.title_bar_h() + self.pad_y() as i32) as f32,
                cols: lw,
                text: left_text,
                fg: theme.dim,
                bg: None,
                bold: false,
            });
        }
        out
    }

    /// Columns of the tab bar taken by the right status segments.
    pub(super) fn status_cols(&self, cols: usize) -> usize {
        let w = super::chrome::text_width(&self.status_all().1.join("  ")).min(cols / 2);
        if w > 0 { w + 2 } else { 0 }
    }

    // --- reloading -----------------------------------------------------------------

    /// Watch the config file's directory (and its subdirectories, for
    /// `require`d modules) and reload when a `.lua` file in it changes.
    pub(super) fn watch_config(&mut self) {
        use rustix::fs::inotify;
        let Some(dir) = self.sc.config_path.as_deref().and_then(Path::parent).map(Path::to_path_buf) else { return };
        let Ok(fd) = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK) else { return };
        let flags = inotify::WatchFlags::CLOSE_WRITE
            | inotify::WatchFlags::MOVED_TO
            | inotify::WatchFlags::MOVED_FROM
            | inotify::WatchFlags::CREATE
            | inotify::WatchFlags::DELETE;
        let dirs = config_dirs(&dir);
        let mut any = false;
        for d in &dirs {
            any |= inotify::add_watch(&fd, d.as_path(), flags).is_ok();
        }
        if !any {
            return;
        }
        let source = Generic::new(fd, Interest::READ, PollMode::Level);
        let _ = self.handle.insert_source(source, move |_, fd, app: &mut App| {
            let relevant = drain_inotify(fd.as_fd());
            if relevant {
                // New module directories become watched as soon as they appear.
                for d in config_dirs(&dir) {
                    let _ = inotify::add_watch(fd.as_fd(), &d, flags);
                }
                app.schedule_reload();
            }
            Ok(PostAction::Continue)
        });
    }

    fn schedule_reload(&mut self) {
        let at = Instant::now() + RELOAD_DELAY;
        self.sc.reload_at = Some(at);
        let _ = self.handle.insert_source(Timer::from_deadline(at), move |_, _, app: &mut App| {
            if app.sc.reload_at == Some(at) {
                app.sc.reload_at = None;
                app.reload_config();
            }
            TimeoutAction::Drop
        });
    }

    /// Load the config again. On an error the running config stays.
    pub(super) fn reload_config(&mut self) {
        let Some(path) = self.sc.config_path.clone() else { return };
        match config::load(&path, false) {
            Err(e) => {
                self.toast(format!("config error (still using the previous config): {e}"), true, Some(CONFIG_ERROR))
            }
            Ok(mut l) => {
                self.sc.overrides.apply(&mut l.config);
                self.install_script(l.script);
                self.apply_config(l.config);
                self.sc.shown.clear();
                self.toast(self.cfg.i18n.t("ui-config-reloaded", &[]), false, Some(Duration::from_secs(2)));
                self.emit(ScriptEvent::new("config_changed"));
            }
        }
    }

    /// Use new settings. Terminal settings (scrollback, clipboard, images)
    /// apply to panes opened from now on.
    fn apply_config(&mut self, new: Config) {
        let old = std::mem::replace(&mut self.cfg, new);
        if old.plugins != self.cfg.plugins {
            self.pl.failures.clear();
        }
        // Re-run Cargo incrementally on config reload so local source edits are picked up.
        self.sync_plugins();
        self.keymap = self.cfg.keymap();
        self.apply_plugin_keys();
        if old.theme != self.cfg.theme {
            let palette = self.cfg.theme.palette();
            for pane in self.panes.values_mut() {
                pane.term.rebase_palette(&palette);
            }
            self.retheme_surfaces(&palette);
            self.emit(ScriptEvent::new("theme_changed"));
        }
        if old.bidi != self.cfg.bidi {
            for pane in self.panes.values_mut() {
                pane.term.set_bidi(self.cfg.bidi);
            }
            for surface in self.pl.surfaces.values_mut() {
                surface.term.set_bidi(self.cfg.bidi);
            }
        }
        self.plugins_config_changed();
        self.hint_patterns = super::hint::patterns(&self.cfg.hint_patterns);
        let font_changed = old.font_family != self.cfg.font_family
            || old.font_size != self.cfg.font_size
            || old.font_settings != self.cfg.font_settings;
        for id in self.window_ids() {
            if !self.activate(id) {
                continue;
            }
            self.w.renderer.images.budget = self.cfg.graphics.gpu_memory;
            if old.decorations != self.cfg.decorations {
                use smithay_client_toolkit::shell::xdg::window::DecorationMode;
                self.w.window.request_decoration_mode(Some(match self.cfg.decorations {
                    crate::config::Decorations::Auto => DecorationMode::Server,
                    crate::config::Decorations::None | crate::config::Decorations::Client => DecorationMode::Client,
                }));
            }
            if (old.background_opacity, old.background_blur) != (self.cfg.background_opacity, self.cfg.background_blur)
            {
                self.w.styled = None;
                self.blur_warned = false;
                self.style_window();
            }
            if font_changed {
                match Fonts::from_config(&self.cfg, self.w.scale) {
                    Ok(f) => {
                        self.w.fonts = f;
                        self.w.font_size = self.cfg.font_size;
                        self.w.renderer.clear_glyphs();
                    }
                    Err(e) => self.toast(format!("font: {e}"), true, None),
                }
                self.apply_size();
            } else if old.padding != self.cfg.padding || old.padding_vertical != self.cfg.padding_vertical {
                self.apply_size();
            } else {
                self.relayout();
            }
            // The mode's keys may be gone.
            if !self.w.mode.passes_keys() {
                self.set_mode(crate::modes::Mode::Normal);
            }
            self.restart_blink();
        }
        self.activate_kbd();
        self.mark_all_dirty();
    }

    // --- host helpers ----------------------------------------------------------------

    /// Make pane `id` the focused pane of its tab, and that tab active.
    pub(super) fn focus_in_place(&mut self, id: PaneId) -> bool {
        if !self.activate_pane(id) {
            return false;
        }
        let Some(i) = self.w.tabs.iter().position(|t| t.contains(id)) else { return false };
        let old = self.focused_id();
        let switch = i != self.w.active;
        self.w.active = i;
        self.w.tabs[i].set_focus(id);
        if switch {
            self.relayout();
        }
        self.after_focus(old);
        true
    }

    fn spawn_args(spawn: &Spawn) -> (Option<PathBuf>, Option<Vec<std::ffi::OsString>>) {
        let cwd = spawn.cwd.as_ref().map(PathBuf::from);
        let argv = spawn.cmd.as_ref().map(|c| c.argv().into_iter().map(Into::into).collect());
        (cwd, argv)
    }

    fn spawned(&mut self, r: Result<PaneId, String>, spawn: Spawn) -> Result<PaneId, String> {
        if let (Ok(id), Some(cmd)) = (&r, spawn.cmd)
            && let Some(p) = self.panes.get_mut(id)
        {
            p.cmd = Some(cmd);
        }
        r
    }
}

/// Split a line into pieces at most `cols` wide, at spaces where possible.
fn wrap(line: &str, cols: usize) -> Vec<String> {
    use unicode_segmentation::UnicodeSegmentation;
    let cols = cols.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut w = 0;
    for g in line.graphemes(true) {
        let gw = super::chrome::text_width(g);
        if w + gw > cols && !cur.is_empty() {
            // Break at the last space if there is one in the second half.
            match cur.rfind(' ').filter(|&i| super::chrome::text_width(&cur[..i]) > cols / 2) {
                Some(i) => {
                    let rest = cur[i + 1..].to_string();
                    cur.truncate(i);
                    out.push(std::mem::replace(&mut cur, rest));
                }
                None => out.push(std::mem::take(&mut cur)),
            }
            w = super::chrome::text_width(&cur);
        }
        cur.push_str(g);
        w += gw;
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// Watch nested config modules without following symlinks or layout data.
fn config_dirs(dir: &Path) -> Vec<std::path::PathBuf> {
    if !dir.is_dir() {
        return dir.ancestors().find(|d| d.is_dir()).map(|d| vec![d.to_path_buf()]).unwrap_or_default();
    }
    let mut dirs = vec![dir.to_path_buf()];
    let mut i = 0;
    while i < dirs.len() {
        if let Ok(rd) = std::fs::read_dir(&dirs[i]) {
            dirs.extend(
                rd.flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) && e.file_name() != "layouts")
                    .map(|e| e.path()),
            );
        }
        i += 1;
    }
    dirs
}

/// Read pending inotify events; true if one concerns a `.lua` file.
fn drain_inotify(fd: std::os::fd::BorrowedFd<'_>) -> bool {
    use rustix::fs::inotify;
    let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 4096];
    let mut reader = inotify::Reader::new(fd, &mut buf);
    let mut relevant = false;
    while let Ok(ev) = reader.next() {
        let name = ev.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        relevant |= name.ends_with(".lua") || ev.events().contains(inotify::ReadFlags::ISDIR);
    }
    relevant
}

fn block_state(s: BlockState) -> &'static str {
    match s {
        BlockState::Prompt => "prompt",
        BlockState::Input => "input",
        BlockState::Running => "running",
        BlockState::Finished => "finished",
    }
}

impl Host for App {
    fn pane_ids(&self) -> Vec<PaneId> {
        let mut ids: Vec<PaneId> = self.panes.keys().copied().collect();
        ids.sort();
        ids
    }

    fn focused(&self) -> Option<PaneId> {
        self.kbd_focus()
    }

    fn pane_info(&self, id: PaneId) -> Option<PaneInfo> {
        let p = self.panes.get(&id)?;
        let w = self.win_with_pane(id)?;
        let tab = w.tabs.iter().position(|t| t.contains(id))?;
        let t = &p.term;
        let running = t.blocks().last().filter(|b| b.state == BlockState::Running);
        Some(PaneInfo {
            id,
            title: p.title().to_string(),
            name: p.name.clone(),
            cwd: p.cwd(&self.hostname).map(|c| c.display().to_string()),
            remote: t.cwd().filter(|c| !c.is_local(&self.hostname)).and_then(|c| c.host.clone()),
            process: p.process(),
            command: running.and_then(|b| b.command.clone()),
            window: w.id,
            tab: tab + 1,
            focused: tab == w.active && w.tabs[tab].focus == id,
            floating: w.tabs[tab].is_float(id),
            cols: t.cols(),
            rows: t.lines(),
        })
    }

    fn split(&mut self, id: PaneId, axis: Option<Axis>, spawn: Spawn) -> Result<PaneId, String> {
        if !self.focus_in_place(id) {
            return Err(format!("no pane {id}"));
        }
        let (cwd, argv) = Self::spawn_args(&spawn);
        let r = self.split_with(axis, cwd, argv);
        self.spawned(r, spawn)
    }

    fn new_tab(&mut self, spawn: Spawn) -> Result<PaneId, String> {
        self.activate_kbd();
        let (cwd, argv) = Self::spawn_args(&spawn);
        let r = self.new_tab_with(cwd, argv);
        self.spawned(r, spawn)
    }

    fn float(&mut self, spawn: Spawn) -> Result<PaneId, String> {
        self.activate_kbd();
        let (cwd, argv) = Self::spawn_args(&spawn);
        let r = self.new_floating_with(cwd, argv);
        self.spawned(r, spawn)
    }

    fn close(&mut self, id: PaneId) {
        if self.activate_pane(id) {
            self.request_close(vec![id], "this pane");
        }
    }

    fn focus(&mut self, id: PaneId) {
        if self.panes.contains_key(&id) {
            self.reveal_pane(id, None);
        }
    }

    fn send_text(&mut self, id: PaneId, text: &str, paste: bool) {
        if paste {
            return self.paste_now(id, text);
        }
        if let Some(p) = self.panes.get_mut(&id) {
            p.term.scroll_to_bottom();
            p.write(text.as_bytes());
        }
    }

    fn set_name(&mut self, id: PaneId, name: Option<String>) {
        if let Some(p) = self.panes.get_mut(&id) {
            p.name = name;
            if self.activate_pane(id) {
                self.update_title();
                self.w.dirty = true;
            }
        }
    }

    fn read_screen(&self, id: PaneId, scrollback: usize) -> Option<String> {
        let t = &self.panes.get(&id)?.term;
        if scrollback == 0 {
            return Some(t.screen_text());
        }
        let g = t.grid();
        let start = g.abs_line(0).saturating_sub(scrollback as u64).max(g.first_line());
        let end = g.end_line().saturating_sub(1).max(start);
        let text = t.text_range(proteus_core::Point::new(start, 0), proteus_core::Point::new(end, t.cols() - 1));
        Some(text.trim_end_matches('\n').to_string())
    }

    fn read_blocks(&self, id: PaneId) -> Vec<BlockInfo> {
        let Some(p) = self.panes.get(&id) else { return Vec::new() };
        p.term
            .blocks()
            .iter()
            .map(|b| BlockInfo {
                id: b.id,
                command: b.command.clone(),
                cwd: b.cwd.clone(),
                exit_code: b.exit_code,
                duration: b.duration().map(|d| d.as_secs_f64()),
                state: block_state(b.state),
            })
            .collect()
    }

    fn block_text(&self, id: PaneId, block: u64, part: BlockPart) -> Option<String> {
        self.panes.get(&id)?.term.block_text(block, part)
    }

    fn run_action(&mut self, action: Action) {
        self.activate_kbd();
        self.dispatch(action);
    }

    fn notify(&mut self, text: String, error: bool, timeout: Option<Duration>) {
        // Messages from scripts are shown every time.
        self.sc.shown.remove(&text);
        self.toast(text, error, timeout);
    }

    fn ui_open(&mut self, o: proteus_wasm::types::SurfaceOptions) -> Result<u32, String> {
        self.config_surface_open(o)
    }

    fn ui_draw(&mut self, id: u32, frame: proteus_wasm::types::Frame) -> Result<(), String> {
        self.config_surface_draw(id, frame)
    }

    fn ui_close(&mut self, id: u32) {
        self.config_surface_close(id);
    }

    fn ui_focus(&mut self, id: u32) -> Result<(), String> {
        self.config_surface_focus(id)
    }

    fn ui_size(&self, id: u32) -> Option<(u32, u32)> {
        self.config_surface_size(id)
    }
}

#[cfg(test)]
mod tests {
    use super::{config_dirs, wrap};

    #[test]
    fn config_modules_are_watched_recursively_without_loading_directories() {
        let root = std::env::temp_dir().join(format!("proteus-config-watch-{}", std::process::id()));
        let nested = root.join("extensions/nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join("layouts/ignored")).unwrap();
        std::os::unix::fs::symlink(&root, nested.join("cycle")).unwrap();
        let dirs = config_dirs(&root);
        assert!(dirs.contains(&nested));
        assert_eq!(dirs.len(), 3);
        let later = nested.join("added");
        std::fs::create_dir_all(&later).unwrap();
        assert!(config_dirs(&root).contains(&later));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wrapping() {
        assert_eq!(wrap("short", 10), ["short"]);
        assert_eq!(wrap("", 10), [""]);
        assert_eq!(
            wrap("config error: init.lua:3: unexpected symbol", 20),
            ["config error:", "init.lua:3:", "unexpected symbol"]
        );
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("中文中文中文", 5), ["中文", "中文", "中文"]);
    }
}
