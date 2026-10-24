//! WASM plugins are resolved exclusively from Lua config paths/URLs.
//! Fetching and source compilation run off the GUI thread. Config Lua uses
//! the same UI backend directly, without a plugin declaration or lifecycle.
//!
//! A surface is a small [`Terminal`] that the plugin's frames are written
//! into (as escape sequences we generate from styled spans, with every
//! control character of the plugin's text removed), so it is drawn by the
//! pane renderer: same fonts, colors, CJK and wide characters.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proteus_core::Terminal;
use proteus_wasm::types::{
    self, Attrs, Color, Event, Frame, MouseButton as PButton, Placement, StatusSide, SurfaceKind, UiEvent,
};
use proteus_wasm::{Answer, Cache, Call, FromPlugin, Key, Pixels, Reply, Resolved, Responder, Runtime, Spec};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use unicode_width::UnicodeWidthChar;

use super::{App, Msg, WinId, render};
use crate::layout::{Axis, PaneId, Rect};
use crate::modes;
use crate::script::{ScriptEvent, Val};

/// Crashes in a session before a plugin is no longer restarted.
const MAX_RESTARTS: u32 = 3;
const RESTART_DELAY: Duration = Duration::from_secs(1);
/// Internal owner key for config UI, outside the WASM source lifecycle.
pub(super) const CONFIG: &str = "";
/// Surfaces one plugin may have open.
const MAX_SURFACES: usize = 8;

/// Surface parts of a frame: sidebars, floats, chrome rectangles, labels.
type SurfaceParts =
    (Vec<super::PanePart>, Vec<super::PanePart>, Vec<([f32; 4], proteus_core::color::Rgb)>, Vec<render::Label>);

/// What a plugin shows.
pub(super) struct Surface {
    pub owner: Key,
    pub win: WinId,
    pub kind: SurfaceKind,
    pub title: String,
    /// Requested size (sidebars: columns only).
    pub want: (u32, u32),
    /// Sidebars, floats and overlays; pane surfaces draw into their pane's
    /// terminal.
    pub term: Terminal,
    /// The pane showing a pane surface.
    pub pane: Option<PaneId>,
    /// Overlays: the cell (of the focused pane, physical pixels) it hangs
    /// below.
    pub anchor: Option<(i32, i32)>,
    /// Size the last frame was painted for.
    pub size: (usize, usize),
    /// Images already sent to the terminal.
    pub sent: HashSet<u32>,
    /// The last frame, painted again after a resize.
    pub frame: Option<Frame>,
    pub accessibility_ids: HashMap<String, u32>,
    next_accessibility_id: u32,
}

/// Work in progress shown in the status bar.
pub(super) struct Task {
    pub title: String,
    pub progress: Option<f32>,
    pub message: String,
}

/// What a plugin palette entry does.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Pick {
    Action(String),
    Restart,
    CancelTask(u32),
}

/// Where a surface is in its window (physical pixels).
#[derive(Debug, Clone, Copy)]
pub(super) struct Placed {
    pub id: u32,
    pub title: Rect,
    pub content: Rect,
    pub float: bool,
}

/// Display labels can change without changing config references or event IDs.
#[derive(Debug, Clone)]
pub(super) struct RegisteredAction {
    id: String,
    name: String,
    default: Option<(modes::Mode, modes::Chord)>,
}
impl RegisteredAction {
    fn parse(id: String, name: String, key: Option<String>) -> Result<Self, String> {
        if id.is_empty() || id.len() > 100 || id.chars().any(char::is_control) {
            return Err("invalid plugin action ID".into());
        }
        let name: String = clean(&name).take(100).collect();
        if name.trim().is_empty() {
            return Err("plugin action label is empty".into());
        }
        let default = key
            .map(|key| suggested_key(&key).ok_or_else(|| format!("invalid plugin default key `{key}`")))
            .transpose()?;
        Ok(Self { id, name, default })
    }
}
fn upsert_action(actions: &mut Vec<RegisteredAction>, action: RegisteredAction) {
    if let Some(old) = actions.iter_mut().find(|old| old.id == action.id) {
        *old = action;
    } else {
        actions.push(action);
    }
}

pub(super) struct Running {
    pub entry: Resolved,
    pub handle: proteus_wasm::Handle,
    pub started: bool,
    /// Stable action IDs, labels and defaults registered by this instance.
    pub actions: Vec<RegisteredAction>,
    /// Status segments: left, right.
    pub status: [String; 2],
}

#[derive(Default)]
pub(super) struct Plugins {
    pub runtime: Option<Runtime>,
    pub cache: Option<Cache>,
    pub generation: u64,
    pub running: HashMap<String, Running>,
    /// Failures this session.
    pub failures: HashMap<String, u32>,
    pub next_instance: u64,
    pub surfaces: BTreeMap<u32, Surface>,
    pub next_surface: u32,
    /// Only successfully resolved declarations from the current config.
    pub resolved: Vec<Resolved>,
    /// Images plugins loaded: (plugin id, image id).
    pub images: HashMap<(String, u32), Arc<Pixels>>,
    /// Prompts and confirmations waiting for the user.
    pub asks: HashMap<u64, (Key, u32)>,
    pub next_ask: u64,
    /// Clipboard reads waiting for the data.
    pub clip_waiters: HashMap<u64, Responder>,
    /// Tasks: (plugin id, task id).
    pub tasks: BTreeMap<(String, u32), Task>,
}

/// 0xRRGGBB.
fn rgb(c: proteus_core::color::Rgb) -> u32 {
    (c.r as u32) << 16 | (c.g as u32) << 8 | c.b as u32
}

/// Plugin text without control characters (no escape sequences can be
/// smuggled into the surface terminal).
fn clean(s: &str) -> impl Iterator<Item = char> + '_ {
    s.chars().filter(|c| !c.is_control())
}

fn sgr(style: &types::Style, base_bg: Option<Color>) -> String {
    let mut s = String::from("\x1b[0");
    for (bit, code) in [
        (Attrs::BOLD, ";1"),
        (Attrs::DIM, ";2"),
        (Attrs::ITALIC, ";3"),
        (Attrs::UNDERLINE, ";4"),
        (Attrs::REVERSE, ";7"),
        (Attrs::STRIKE, ";9"),
    ] {
        if style.attrs.contains(bit) {
            s.push_str(code);
        }
    }
    let color = |s: &mut String, c: Color, base: u8| match c {
        Color::Indexed(i) => s.push_str(&format!(";{base}8;5;{i}")),
        Color::Rgb(v) => s.push_str(&format!(";{base}8;2;{};{};{}", v >> 16 & 255, v >> 8 & 255, v & 255)),
    };
    if let Some(c) = style.fg {
        color(&mut s, c, 3);
    }
    if let Some(c) = style.bg.or(base_bg) {
        color(&mut s, c, 4);
    }
    s.push('m');
    s
}

/// The escape sequences that show `frame` on a `cols` × `rows` terminal.
pub(super) fn paint(frame: &Frame, cols: usize, rows: usize) -> String {
    let mut out = String::from("\x1b[?7l\x1b[?25l");
    out.push_str(&sgr(&types::Style { fg: None, bg: None, attrs: Attrs::empty() }, frame.background));
    out.push_str("\x1b[H\x1b[2J");
    for (r, line) in frame.lines.iter().take(rows).enumerate() {
        out.push_str(&format!("\x1b[{};1H", r + 1));
        let mut col = 0;
        for span in line {
            if col >= cols {
                break;
            }
            out.push_str(&sgr(&span.style, frame.background));
            let link: Option<String> = span.link.as_deref().map(|l| clean(l).filter(|c| *c != ';').collect());
            if let Some(l) = &link {
                out.push_str(&format!("\x1b]8;;{l}\x1b\\"));
            }
            for c in clean(&span.text) {
                let w = c.width().unwrap_or(0);
                if col + w > cols {
                    col = cols;
                    break;
                }
                out.push(c);
                col += w;
            }
            if link.is_some() {
                out.push_str("\x1b]8;;\x1b\\");
            }
        }
    }
    out.push_str("\x1b[0m");
    if let Some((c, r)) = frame.cursor
        && (c as usize) < cols
        && (r as usize) < rows
    {
        out.push_str(&format!("\x1b[{};{}H\x1b[?25h", r + 1, c + 1));
    }
    out
}

/// The escape sequence that puts the cursor where `frame` wants it.
fn cursor_seq(frame: &Frame, cols: usize, rows: usize) -> String {
    match frame.cursor {
        Some((c, r)) if (c as usize) < cols && (r as usize) < rows => format!("\x1b[{};{}H", r + 1, c + 1),
        _ => String::new(),
    }
}

/// A kitty graphics transmission of `px` as image `id` (no placement).
fn kitty_upload(id: u32, px: &Pixels) -> String {
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD.encode(&px.rgba);
    let mut out = String::new();
    let chunks: Vec<&[u8]> = data.as_bytes().chunks(4096).collect();
    for (i, c) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        let c = std::str::from_utf8(c).unwrap_or("");
        if i == 0 {
            out.push_str(&format!("\x1b_Ga=t,f=32,s={},v={},i={id},q=2,m={more};{c}\x1b\\", px.width, px.height));
        } else {
            out.push_str(&format!("\x1b_Gm={more};{c}\x1b\\"));
        }
    }
    out
}

/// A terminal for a surface: no scrollback, no clipboard, no image files.
fn surface_term(cols: usize, rows: usize) -> Terminal {
    let mut c = proteus_core::Config { scrollback: 0, osc52_write: false, ..Default::default() };
    c.osc52_read = proteus_core::ClipboardRead::Deny;
    c.graphics.allow_local_files = false;
    Terminal::new(cols.max(1), rows.max(1), c)
}

fn block_info(b: crate::script::BlockInfo) -> types::BlockInfo {
    types::BlockInfo {
        id: b.id,
        command: b.command,
        cwd: b.cwd,
        exit_code: b.exit_code,
        duration_ms: b.duration.map(|d| (d * 1000.0) as u64),
        state: match b.state {
            "prompt" => types::BlockState::Prompt,
            "input" => types::BlockState::Input,
            "running" => types::BlockState::Running,
            _ => types::BlockState::Finished,
        },
    }
}

/// A terminal event as plugins see it.
fn plugin_event(ev: &ScriptEvent) -> Option<Event> {
    let pane = || {
        ev.fields.iter().find_map(|(k, v)| match (k, v) {
            (&"pane", Val::Pane(p)) => Some(*p),
            _ => None,
        })
    };
    let s = |key: &str| {
        ev.fields.iter().find_map(|(k, v)| match v {
            Val::Str(s) if *k == key => Some(s.clone()),
            _ => None,
        })
    };
    let num = |key: &str| {
        ev.fields.iter().find_map(|(k, v)| match v {
            Val::Int(i) if *k == key => Some(*i as f64),
            Val::Num(n) if *k == key => Some(*n),
            _ => None,
        })
    };
    Some(match ev.name {
        "pane_opened" => Event::PaneOpened(pane()?),
        "pane_closed" => Event::PaneClosed(pane()?),
        "focus_changed" => Event::FocusChanged(pane()?),
        "cwd_changed" => Event::CwdChanged((pane()?, s("cwd")?)),
        "title_changed" => Event::TitleChanged((pane()?, s("title")?)),
        "command_started" => Event::CommandStarted((pane()?, s("command"))),
        "command_finished" => Event::CommandFinished(types::CommandFinished {
            pane: pane()?,
            command: s("command"),
            exit_code: num("exit_code").map(|c| c as i32),
            duration_ms: num("duration").map(|d| (d * 1000.0) as u64),
        }),
        "bell" => Event::Bell(pane()?),
        _ => return None,
    })
}

impl App {
    pub(super) fn plugins_init(&mut self) {
        self.pl.cache = Cache::default_paths();
        self.sync_plugins();
    }

    /// Start a new resolution generation. Removed sources stop immediately;
    /// slow fetch/build results from superseded configs can never start a plugin.
    pub(super) fn sync_plugins(&mut self) {
        self.pl.generation += 1;
        let generation = self.pl.generation;
        let stale: Vec<_> = self
            .pl
            .running
            .iter()
            .filter(|(_, r)| !self.cfg.plugin_should_load(&r.entry))
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            self.stop_plugin(&id);
        }
        self.pl.resolved.retain(|e| self.cfg.plugin_should_load(e));
        if self.cfg.plugins.is_empty() {
            return;
        }
        let Some(cache) = self.pl.cache.clone() else {
            self.toast("plugins: no cache/data directory available".into(), true, None);
            return;
        };
        let sources = self.cfg.plugins.clone();
        let tx = self.sender.clone();
        // No directory scans, installed-plugin registry, or bundled defaults.
        if let Err(e) = std::thread::Builder::new().name("plugin-sources".into()).spawn(move || {
            let results = sources.iter().map(|s| cache.resolve(s)).collect();
            let _ = tx.send(Msg::PluginsResolved(generation, results));
        }) {
            self.toast(format!("plugins: {e}"), true, None);
        }
    }

    pub(super) fn plugins_resolved(&mut self, generation: u64, results: Vec<Result<Resolved, String>>) {
        if generation != self.pl.generation {
            return;
        }
        let mut resolved = Vec::new();
        let mut duplicates = HashSet::new();
        for result in results {
            match result {
                Ok(e) if self.cfg.plugin_should_load(&e) => {
                    if resolved.iter().any(|other: &Resolved| other.id == e.id) {
                        duplicates.insert(e.id.clone());
                    }
                    resolved.push(e);
                }
                Ok(_) => {}
                Err(e) => self.toast(format!("plugin source: {e}"), true, None),
            }
        }
        for id in &duplicates {
            self.toast(format!("plugins: multiple sources declare ID `{id}`; neither is loaded"), true, None);
        }
        resolved.retain(|e| !duplicates.contains(&e.id));
        let stale: Vec<_> =
            self.pl.running.iter().filter(|(_, r)| !resolved.contains(&r.entry)).map(|(id, _)| id.clone()).collect();
        for id in stale {
            self.stop_plugin(&id);
            self.pl.failures.remove(&id);
        }
        self.pl.resolved = resolved;
        for e in self.pl.resolved.clone() {
            if !self.pl.running.contains_key(&e.id) && !self.pl.failures.get(&e.id).is_some_and(|&n| n > MAX_RESTARTS) {
                self.start_plugin(e);
            }
        }
        self.keymap = self.cfg.keymap();
        self.apply_plugin_keys();
        self.mark_all_dirty();
    }

    fn start_plugin(&mut self, entry: Resolved) {
        // The final gate covers synchronization, delayed crash restarts and manual actions.
        if !self.cfg.plugin_should_load(&entry) {
            return;
        }
        let Some(cache) = &self.pl.cache else { return };
        let wasm = (*entry.wasm).clone();
        if self.pl.runtime.is_none() {
            match Runtime::new() {
                Ok(r) => self.pl.runtime = Some(r),
                Err(e) => return self.toast(format!("plugins: cannot start the WASM runtime: {e}"), true, None),
            }
        }
        self.pl.next_instance += 1;
        let key = Key { id: entry.id.clone(), instance: self.pl.next_instance };
        let tx = Mutex::new(self.sender.clone());
        let sink: proteus_wasm::Sink = Arc::new(move |k, m| {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(Msg::Plugin(k, m));
            }
        });
        let spec = Spec {
            config: entry.source.config.clone(),
            key,
            wasm,
            hash: entry.hash.clone(),
            granted: entry.granted.clone(),
            data_dir: cache.data_dir(&entry.id),
            storage: cache.storage_path(&entry.id),
            memory: proteus_wasm::runtime::MEMORY,
            stderr: std::env::var_os("PROTEUS_DEBUG").is_some(),
            compiled: Some(cache.compiled_dir()),
        };
        let Some(rt) = &self.pl.runtime else { return };
        match rt.start(spec, sink) {
            Ok(handle) => {
                let r = Running { entry, handle, started: false, actions: Vec::new(), status: Default::default() };
                self.pl.running.insert(r.entry.id.clone(), r);
            }
            Err(e) => self.toast(format!("plugin {}: {e}", entry.name), true, None),
        }
    }

    /// Stop a plugin and remove what it showed.
    pub(super) fn stop_plugin(&mut self, id: &str) {
        if let Some(r) = self.pl.running.remove(id) {
            r.handle.stop();
            self.remove_plugin_ui(&r.handle.key);
            self.keymap = self.cfg.keymap();
            self.apply_plugin_keys();
        }
    }

    /// Remove what a stopped plugin showed. Its panes stay, saying so,
    /// and are handed to the plugin when it starts again.
    fn remove_plugin_ui(&mut self, key: &Key) {
        let mine: Vec<u32> = self.pl.surfaces.iter().filter(|(_, s)| s.owner == *key).map(|(id, _)| *id).collect();
        for id in mine {
            if self.pl.surfaces[&id].pane.is_some() {
                self.paint_orphan(id);
            } else {
                self.drop_surface(id);
            }
        }
        let id = &key.id;
        self.pl.images.retain(|(p, _), _| p != id);
        self.pl.tasks.retain(|(p, _), _| p != id);
        self.pl.asks.retain(|_, (k, _)| k != key);
        let asks: HashSet<u64> = self.pl.asks.keys().copied().collect();
        for w in std::iter::once(&mut self.w).chain(self.parked.iter_mut()) {
            w.prompts.retain(|p| !matches!(p, super::Prompt::PluginConfirm(n, _) if !asks.contains(n)));
            if let Some(super::textinput::Purpose::PluginPrompt(n)) = w.input.as_ref().map(|i| i.purpose)
                && !asks.contains(&n)
            {
                w.input = None;
            }
        }
        self.status_changed();
        self.mark_all_dirty();
    }

    /// A pane surface whose plugin is not running.
    fn paint_orphan(&mut self, id: u32) {
        let Some(s) = self.pl.surfaces.get(&id) else { return };
        let text = format!("plugin {} is not running", s.owner.id);
        let style = types::Style { fg: None, bg: None, attrs: Attrs::DIM };
        let frame = Frame {
            lines: vec![vec![types::Span { text, style, link: None }]],
            cursor: None,
            background: None,
            images: Vec::new(),
            accessibility: Vec::new(),
        };
        let _ = self.show_frame(id, frame);
    }

    fn name_of(&self, id: &str) -> String {
        self.pl.running.get(id).map(|r| r.entry.name.clone()).unwrap_or_else(|| id.to_string())
    }

    /// A message from a plugin thread.
    pub(super) fn plugin_msg(&mut self, key: Key, m: FromPlugin) {
        let current = self.pl.running.get(&key.id).is_some_and(|r| r.handle.key == key);
        match m {
            FromPlugin::Call(Call::ClipboardRead, Some(reply)) if current => {
                self.pl.next_ask += 1;
                let n = self.pl.next_ask;
                self.pl.clip_waiters.insert(n, reply);
                self.read_clipboard(super::ClipboardKind::Clipboard, super::ClipTarget::Plugin(n));
            }
            FromPlugin::Call(call, reply) => {
                let answer = if current { self.plugin_call(&key, call) } else { Err("the plugin was stopped".into()) };
                if let Some(r) = reply {
                    let _ = r.send(answer);
                }
            }
            _ if !current => {}
            FromPlugin::Started => {
                let theme = self.plugin_theme();
                let Some(r) = self.pl.running.get_mut(&key.id) else { return };
                r.started = true;
                r.handle.event(Event::ThemeChanged(theme));
                // Its panes from layouts and from before a restart.
                let panes = &self.panes;
                for (&sid, s) in self.pl.surfaces.iter_mut() {
                    if s.owner.id == key.id && s.owner != key && s.pane.is_some() {
                        s.owner = key.clone();
                        let args = s.pane.and_then(|p| panes.get(&p)?.plugin.clone()).map(|p| p.1).unwrap_or_default();
                        r.handle.event(Event::SurfaceOpened((sid, args)));
                        let (c, rw) = s.size;
                        r.handle.ui(sid, UiEvent::Resize((c as u32, rw as u32)));
                    }
                }
            }
            FromPlugin::Exited => self.stop_plugin(&key.id),
            FromPlugin::Failed(e) => {
                let name = self.name_of(&key.id);
                self.stop_plugin(&key.id);
                let n = self.pl.failures.entry(key.id.clone()).or_insert(0);
                *n += 1;
                // Failing to load or initialize happens again the same way.
                let fatal = e.starts_with("init failed") || e.starts_with("not a valid plugin");
                if fatal || *n > MAX_RESTARTS {
                    *n = MAX_RESTARTS + 1;
                    self.toast(format!("plugin {name}: {e}; not restarted (palette: plugins: restart)"), true, None);
                } else {
                    self.toast(format!("plugin {name}: {e}; restarting"), true, None);
                    let id = key.id.clone();
                    let _ =
                        self.handle.insert_source(Timer::from_duration(RESTART_DELAY), move |_, _, app: &mut App| {
                            if !app.pl.running.contains_key(&id)
                                && let Some(e) = app
                                    .pl
                                    .resolved
                                    .iter()
                                    .find(|e| e.id == id && app.cfg.plugin_should_load(e))
                                    .cloned()
                            {
                                app.start_plugin(e);
                            }
                            TimeoutAction::Drop
                        });
                }
            }
        }
    }

    /// Send a terminal event to the plugins.
    pub(super) fn plugins_event(&self, ev: &ScriptEvent) {
        if self.pl.running.is_empty() {
            return;
        }
        let Some(e) = plugin_event(ev) else { return };
        for r in self.pl.running.values().filter(|r| r.started) {
            r.handle.event(e.clone());
        }
    }

    fn plugin_theme(&self) -> types::Theme {
        let t = self.theme();
        let luma = 0.299 * t.bg.r as f32 + 0.587 * t.bg.g as f32 + 0.114 * t.bg.b as f32;
        types::Theme {
            fg: rgb(t.fg),
            bg: rgb(t.bg),
            accent: rgb(t.accent),
            bar: rgb(t.bar),
            dim: rgb(t.dim),
            dark: luma < 128.0,
        }
    }

    /// Status segments of all plugins: left, right.
    pub(super) fn plugin_status(&self) -> (Vec<String>, Vec<String>) {
        let mut ids: Vec<&String> = self.pl.running.keys().collect();
        ids.sort();
        let (mut l, mut r) = (Vec::new(), Vec::new());
        for id in ids {
            let s = &self.pl.running[id].status;
            if !s[0].is_empty() {
                l.push(s[0].clone());
            }
            if !s[1].is_empty() {
                r.push(s[1].clone());
            }
        }
        for t in self.pl.tasks.values() {
            let mut seg = t.title.clone();
            if let Some(p) = t.progress {
                seg.push_str(&format!(" {:.0}%", p * 100.0));
            } else {
                seg.push_str(" …");
            }
            if !t.message.is_empty() {
                seg.push_str(&format!(" ({})", t.message));
            }
            l.push(seg);
        }
        (l, r)
    }

    /// Palette entries: plugin actions, tasks to cancel, and managing
    /// resolved plugins.
    pub(super) fn plugin_palette(&self) -> Vec<(String, String, Pick)> {
        let mut v = Vec::new();
        let mut ids: Vec<&String> = self.pl.running.keys().collect();
        ids.sort();
        for id in ids {
            for action in &self.pl.running[id].actions {
                v.push((action.name.clone(), id.clone(), Pick::Action(action.id.clone())));
            }
        }
        for ((id, n), t) in &self.pl.tasks {
            v.push((format!("plugins: cancel {} ({})", t.title, self.name_of(id)), id.clone(), Pick::CancelTask(*n)));
        }
        for e in &self.pl.resolved {
            v.push((format!("plugins: restart {}", e.name), e.id.clone(), Pick::Restart));
        }
        v
    }

    /// Run a palette entry from [`App::plugin_palette`].
    pub(super) fn plugin_palette_run(&mut self, _label: &str, id: &str, pick: Pick) {
        match pick {
            Pick::Action(a) => {
                if let Some(r) = self.pl.running.get(id) {
                    r.handle.event(Event::Action(a));
                }
            }
            Pick::CancelTask(n) => {
                if self.pl.tasks.remove(&(id.to_string(), n)).is_some() {
                    if let Some(r) = self.pl.running.get(id) {
                        r.handle.event(Event::TaskCancelled(n));
                    }
                    self.status_changed();
                }
            }
            Pick::Restart => {
                self.pl.failures.remove(id);
                self.stop_plugin(id);
                self.sync_plugins();
            }
        }
    }

    /// A key bound to a plugin's action.
    pub(super) fn run_plugin_action(&mut self, i: u16) {
        let Some((id, name)) = modes::plugin_action_name(i) else { return };
        if let Some(r) = self.pl.running.get(&id).filter(|r| r.started && r.actions.iter().any(|a| a.id == name)) {
            r.handle.event(Event::Action(name));
        }
    }

    /// Rebuild from preset + plugin defaults + explicit user overrides.
    pub(super) fn apply_plugin_keys(&mut self) {
        let mut actions = Vec::new();
        for (owner, running) in &self.pl.running {
            let priority = self.cfg.plugins.iter().position(|s| s == &running.entry.source).unwrap_or(usize::MAX);
            for registered in &running.actions {
                if let Some((mode, chord)) = registered.default
                    && let Some(action) = modes::plugin_action(owner, &registered.id)
                {
                    actions.push((priority, owner.clone(), registered.id.clone(), (mode, chord, action)));
                }
            }
        }
        // Conflicting plugin defaults are deterministic: source declaration
        // order, then stable ID, independent of thread startup order.
        actions.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
        let defaults = actions.into_iter().map(|(_, _, _, binding)| binding).collect::<Vec<_>>();
        self.keymap = modes::Keymap::with_plugin_defaults(&self.cfg.keys, &defaults);
    }

    pub(super) fn retheme_surfaces(&mut self, palette: &proteus_core::color::Palette) {
        for s in self.pl.surfaces.values_mut() {
            s.term.rebase_palette(palette);
        }
    }

    pub(super) fn plugins_config_changed(&self) {
        let theme = self.plugin_theme();
        for r in self.pl.running.values().filter(|r| r.started) {
            r.handle.event(Event::ConfigChanged);
            r.handle.event(Event::ThemeChanged(theme));
        }
    }

    /// A block of pane `id` started, ran or finished.
    pub(super) fn block_state_changed(&mut self, id: PaneId) {
        use crate::script::Host as _;
        let Some(b) = self.read_blocks(id).pop() else { return };
        self.emit(
            ScriptEvent::new("block_state_changed")
                .pane(id)
                .with("block", Val::Int(b.id as i64))
                .with("state", Val::Str(b.state.to_string())),
        );
        if self.pl.running.is_empty() || self.panes.get(&id).is_some_and(|p| p.private) {
            return;
        }
        let e = Event::BlockStateChanged((id, block_info(b)));
        for r in self.pl.running.values().filter(|r| r.started) {
            r.handle.event(e.clone());
        }
    }

    /// The user answered a plugin's prompt or confirmation.
    pub(super) fn plugin_answer(&mut self, n: u64, answer: Option<String>) {
        let Some((key, req)) = self.pl.asks.remove(&n) else { return };
        if let Some(r) = self.pl.running.get(&key.id).filter(|r| r.handle.key == key) {
            r.handle.event(Event::Answered((req, answer)));
        }
    }

    /// Clipboard text a plugin asked for.
    pub(super) fn plugin_clipboard(&mut self, n: u64, text: String) {
        if let Some(r) = self.pl.clip_waiters.remove(&n) {
            let _ = r.send(Ok(Reply::Text(text)));
        }
    }

    /// The surface that gets keys: a focused sidebar or float, else the
    /// focused pane's when it is a plugin's.
    pub(super) fn key_surface(&self) -> Option<u32> {
        self.w.surface_focus.or_else(|| self.panes.get(&self.focused_id())?.surface)
    }

    // --- calls -----------------------------------------------------------------

    fn plugin_call(&mut self, key: &Key, call: Call) -> Answer {
        use crate::script::Host as _;
        let pane_info = |app: &App, id: PaneId| -> Option<types::PaneInfo> {
            let i = app.pane_info(id)?;
            Some(types::PaneInfo {
                id: i.id,
                title: i.title,
                name: i.name,
                cwd: i.cwd,
                process: i.process,
                command: i.command,
                window: i.window,
                tab: i.tab as u32,
                focused: i.focused,
                floating: i.floating,
                cols: i.cols as u32,
                rows: i.rows as u32,
            })
        };
        let exists = |app: &App, id: PaneId| {
            if app.panes.contains_key(&id) { Ok(()) } else { Err(format!("no pane {id}")) }
        };
        let private = |app: &App, id: PaneId| {
            if app.panes.get(&id).is_some_and(|p| p.private) { Err(format!("pane {id} is private")) } else { Ok(()) }
        };
        match call {
            Call::Locale => Ok(Reply::Text(self.cfg.i18n.locale().into())),
            Call::Log(m) => {
                if std::env::var_os("PROTEUS_DEBUG").is_some() {
                    eprintln!("proteus: plugin {}: {m}", key.id);
                }
                Ok(Reply::Unit)
            }
            Call::Notify { text, error } => {
                let name = self.name_of(&key.id);
                let text: String = text.chars().filter(|c| !c.is_control() || *c == '\n').take(1000).collect();
                self.toast(format!("{name}: {text}"), error, None);
                Ok(Reply::Unit)
            }
            Call::ListPanes => {
                Ok(Reply::Panes(self.pane_ids().into_iter().filter_map(|id| pane_info(self, id)).collect()))
            }
            Call::GetPane(id) => Ok(Reply::Pane(pane_info(self, id))),
            Call::PaneRemote(id) => {
                exists(self, id)?;
                Ok(Reply::Remote(self.pane_info(id).and_then(|i| i.remote)))
            }
            Call::FocusedPane => Ok(Reply::Id(self.focused())),
            Call::FocusPane(id) => {
                exists(self, id)?;
                self.reveal_pane(id, None);
                self.unfocus_surface();
                self.activate_kbd();
                Ok(Reply::Unit)
            }
            Call::SendText { pane, text, paste } => {
                exists(self, pane)?;
                self.send_text(pane, &text, paste);
                Ok(Reply::Unit)
            }
            Call::ReadScreen { pane, scrollback } => {
                exists(self, pane)?;
                private(self, pane)?;
                self.read_screen(pane, scrollback as usize).map(Reply::Text).ok_or_else(|| "no screen".into())
            }
            Call::ReadBlocks(pane) => {
                exists(self, pane)?;
                private(self, pane)?;
                let blocks = self.read_blocks(pane).into_iter().map(block_info).collect();
                Ok(Reply::Blocks(blocks))
            }
            Call::OpenPane(at, what) => {
                self.activate_kbd();
                let cwd = what.cwd.map(PathBuf::from);
                let argv = (!what.argv.is_empty()).then(|| what.argv.into_iter().map(Into::into).collect());
                let r = match at {
                    Placement::SplitRight => self.split_with(Some(crate::layout::Axis::X), cwd, argv),
                    Placement::SplitDown => self.split_with(Some(crate::layout::Axis::Y), cwd, argv),
                    Placement::NewTab => self.new_tab_with(cwd, argv),
                    Placement::Float => self.new_floating_with(cwd, argv),
                };
                r.map(|id| Reply::Id(Some(id)))
            }
            Call::OpenSurface(o) => self.open_surface(key, o).map(Reply::Surface),
            Call::StartDrag(id, paths) => {
                // Only while the user drags on that surface of this plugin.
                let ours = self.pl.surfaces.get(&id).is_some_and(|s| s.owner == *key);
                if ours && self.w.surface_drag.is_some_and(|d| d.0 == id) {
                    self.w.drag_paths = Some(paths);
                }
                Ok(Reply::Unit)
            }
            Call::Draw(id, frame) => {
                proteus_wasm::accessibility::validate(&frame.accessibility)?;
                if !self.pl.surfaces.get(&id).is_some_and(|s| s.owner == *key) {
                    return Err(format!("no surface {id}"));
                }
                self.show_frame(id, frame)?;
                Ok(Reply::Unit)
            }
            Call::CloseSurface(id) => {
                if self.pl.surfaces.get(&id).is_some_and(|s| s.owner == *key) {
                    self.drop_surface(id);
                }
                Ok(Reply::Unit)
            }
            Call::FocusSurface(id) => {
                let win = self.pl.surfaces.get(&id).filter(|s| s.owner == *key).map(|s| s.win);
                let Some(win) = win else { return Err(format!("no surface {id}")) };
                if self.activate(win) {
                    self.focus_surface(id);
                }
                self.activate_kbd();
                Ok(Reply::Unit)
            }
            Call::SetSurfaceTitle(id, title) => {
                let s = self.pl.surfaces.get_mut(&id).filter(|s| s.owner == *key).ok_or(format!("no surface {id}"))?;
                let title: String = clean(&title).take(100).collect();
                if let Some(p) = s.pane.and_then(|id| self.panes.get_mut(&id))
                    && p.name.as_deref() == Some(&s.title)
                {
                    p.name = Some(title.clone());
                }
                s.title = title;
                self.update_title();
                self.mark_all_dirty();
                Ok(Reply::Unit)
            }
            Call::SurfaceSize(id) => {
                let s = self.pl.surfaces.get(&id).filter(|s| s.owner == *key).ok_or(format!("no surface {id}"))?;
                Ok(Reply::Size(s.size.0 as u32, s.size.1 as u32))
            }
            Call::SetStatus(side, text) => {
                let text: String = clean(&text).take(200).collect();
                if let Some(r) = self.pl.running.get_mut(&key.id) {
                    r.status[usize::from(side == StatusSide::Right)] = text;
                }
                self.status_changed();
                Ok(Reply::Unit)
            }
            Call::RegisterAction { id, name, key: default_key } => {
                if self
                    .pl
                    .running
                    .get(&key.id)
                    .is_some_and(|r| r.actions.len() >= 256 && !r.actions.iter().any(|a| a.id == id))
                {
                    return Err("at most 256 plugin actions".into());
                }
                let action = RegisteredAction::parse(id, name, default_key)?;
                let Some(running) = self.pl.running.get_mut(&key.id) else {
                    return Err("plugin is not running".into());
                };
                upsert_action(&mut running.actions, action);
                self.apply_plugin_keys();
                Ok(Reply::Unit)
            }
            Call::LoadImage(id, px) => {
                self.pl.images.insert((key.id.clone(), id), px);
                // Surfaces showing an older image of this id send it again.
                for s in self.pl.surfaces.values_mut().filter(|s| s.owner == *key) {
                    s.sent.remove(&id);
                }
                Ok(Reply::Unit)
            }
            Call::DropImage(id) => {
                self.pl.images.remove(&(key.id.clone(), id));
                Ok(Reply::Unit)
            }
            Call::Prompt { id, question, initial } => {
                if self.pl.asks.values().filter(|(owner, _)| owner == key).count() >= 16 {
                    return Err("too many pending questions".into());
                }
                let n = self.ask(key, id);
                let q: String = clean(&question).take(200).collect();
                self.open_input(
                    super::textinput::Purpose::PluginPrompt(n),
                    &initial,
                    format!("{}: {q}", self.name_of(&key.id)),
                );
                Ok(Reply::Unit)
            }
            Call::Confirm { id, question } => {
                if self.pl.asks.values().filter(|(owner, _)| owner == key).count() >= 16 {
                    return Err("too many pending questions".into());
                }
                let n = self.ask(key, id);
                let q: String = clean(&question).take(300).collect();
                self.w.prompts.push_back(super::Prompt::PluginConfirm(n, format!("{}: {q}", self.name_of(&key.id))));
                self.w.dirty = true;
                Ok(Reply::Unit)
            }
            Call::TaskStart { id, title } => {
                let title: String = clean(&title).take(60).collect();
                self.pl.tasks.insert((key.id.clone(), id), Task { title, progress: None, message: String::new() });
                self.status_changed();
                Ok(Reply::Unit)
            }
            Call::TaskUpdate { id, progress, message } => {
                if let Some(t) = self.pl.tasks.get_mut(&(key.id.clone(), id)) {
                    t.progress = progress.filter(|p| p.is_finite()).map(|p| p.clamp(0.0, 1.0));
                    t.message = clean(&message).take(60).collect();
                }
                self.status_changed();
                Ok(Reply::Unit)
            }
            Call::TaskEnd(id) => {
                self.pl.tasks.remove(&(key.id.clone(), id));
                self.status_changed();
                Ok(Reply::Unit)
            }
            Call::ClipboardRead => {
                // Answered later through `plugin_clipboard`; the caller's
                // responder is taken in `plugin_msg`.
                Err("clipboard-read needs a responder".into())
            }
            Call::ClipboardWrite(text) => {
                self.set_selection(super::ClipboardKind::Clipboard, text);
                Ok(Reply::Unit)
            }
            Call::Open(target, file) => {
                static OPENERS: std::sync::OnceLock<proteus_pty::budget::Budget> = std::sync::OnceLock::new();
                let permit = OPENERS
                    .get_or_init(|| proteus_pty::budget::Budget::new(4, 4))
                    .try_acquire(1)
                    .ok_or("too many pending desktop opens")?;
                let mut cmd = std::process::Command::new("xdg-open");
                cmd.arg(&target)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                let mut child = cmd.spawn().map_err(|e| e.to_string())?;
                std::thread::Builder::new()
                    .name("plugin-open".into())
                    .spawn(move || {
                        let _guard = (file, permit);
                        let _ = child.wait();
                    })
                    .map_err(|e| e.to_string())?;
                Ok(Reply::Unit)
            }
        }
    }

    /// Remember a question of a plugin; its number for the answer.
    fn ask(&mut self, key: &Key, req: u32) -> u64 {
        self.pl.next_ask += 1;
        self.pl.asks.insert(self.pl.next_ask, (key.clone(), req));
        self.pl.next_ask
    }

    /// Bars may have appeared or gone: lay out every window again.
    fn status_changed(&mut self) {
        for id in self.window_ids() {
            if self.activate(id) {
                self.relayout();
            }
        }
        self.activate_kbd();
    }

    fn dirty_win(&mut self, win: WinId) {
        if self.w.id == win {
            self.w.dirty = true;
        } else if let Some(w) = self.parked.iter_mut().find(|w| w.id == win) {
            w.dirty = true;
        }
    }

    // --- surfaces --------------------------------------------------------------

    pub(super) fn open_surface(&mut self, key: &Key, o: types::SurfaceOptions) -> Result<u32, String> {
        if self.pl.surfaces.values().filter(|s| s.owner == *key).count() >= MAX_SURFACES {
            return Err(format!("at most {MAX_SURFACES} surfaces"));
        }
        self.activate_kbd();
        if let Some(p) = o.pane {
            if !self.panes.contains_key(&p) {
                return Err(format!("no pane {p}"));
            }
            self.reveal_pane(p, None);
        }
        let title: String = clean(&o.title).take(100).collect();
        let want = (o.cols.clamp(4, 500), o.rows.clamp(0, 500));
        let id = self.new_surface(key.clone(), o.kind, title.clone(), want);
        let pane = match o.kind {
            SurfaceKind::PaneRight | SurfaceKind::PaneDown | SurfaceKind::Tab => {
                let make = move |app: &mut App, c: u16, r: u16| Ok(app.surface_pane(id, &title, c, r));
                let r = match o.kind {
                    SurfaceKind::PaneRight => self.split_new(Some(Axis::X), make),
                    SurfaceKind::PaneDown => self.split_new(Some(Axis::Y), make),
                    _ => self.new_tab_new(make),
                };
                match r {
                    Ok(p) => Some(p),
                    Err(e) => {
                        self.pl.surfaces.remove(&id);
                        return Err(e);
                    }
                }
            }
            _ => None,
        };
        if o.kind == SurfaceKind::Overlay {
            let r = self.view().rect(self.focused_id());
            let t = &self.focused_pane().term;
            let (cw, ch) = (self.cell().0 as i32, self.cell().1 as i32);
            let anchor = r.map(|r| (r.x + t.cursor().x as i32 * cw, r.y + t.cursor().y as i32 * ch));
            if let Some(s) = self.pl.surfaces.get_mut(&id) {
                s.anchor = anchor;
            }
        }
        self.relayout();
        // A new pane is focused like any split.
        if o.focus && pane.is_none() {
            self.focus_surface(id);
        }
        Ok(id)
    }

    fn new_surface(&mut self, owner: Key, kind: SurfaceKind, title: String, want: (u32, u32)) -> u32 {
        self.pl.next_surface += 1;
        let id = self.pl.next_surface;
        let mut term = surface_term(1, 1);
        term.set_bidi(self.cfg.bidi);
        term.set_palette(self.cfg.theme.palette());
        let s = Surface {
            owner,
            win: self.w.id,
            kind,
            title,
            want,
            term,
            pane: None,
            anchor: None,
            size: (0, 0),
            sent: HashSet::new(),
            frame: None,
            accessibility_ids: HashMap::new(),
            next_accessibility_id: 4,
        };
        self.pl.surfaces.insert(id, s);
        id
    }

    /// A pane for pane surface `id`.
    fn surface_pane(&mut self, id: u32, title: &str, cols: u16, rows: u16) -> PaneId {
        let pid = self.next_id;
        self.next_id += 1;
        let mut term = surface_term(cols.max(2) as usize, rows.max(1) as usize);
        term.set_bidi(self.cfg.bidi);
        term.set_palette(self.cfg.theme.palette());
        term.set_cell_pixel_size(self.cell().0, self.cell().1);
        self.panes.insert(pid, super::pane::Pane::plugin(term, id, title.to_string()));
        if let Some(s) = self.pl.surfaces.get_mut(&id) {
            s.pane = Some(pid);
            s.size = (cols.max(2) as usize, rows.max(1) as usize);
        }
        self.emit(ScriptEvent::new("pane_opened").pane(pid));
        pid
    }

    /// A pane from a layout's `plugin = "ID"` node. The plugin hears of it
    /// (`surface-opened`) once it runs.
    pub(super) fn plugin_pane(&mut self, plugin: &str, args: &str, cols: u16, rows: u16) -> PaneId {
        let owner = match self.pl.running.get(plugin) {
            Some(r) if r.started => r.handle.key.clone(),
            _ => Key { id: plugin.to_string(), instance: 0 },
        };
        let title = self.pl.resolved.iter().find(|e| e.id == plugin).map_or(plugin.to_string(), |e| e.name.clone());
        let id = self.new_surface(owner.clone(), SurfaceKind::Tab, title.clone(), (0, 0));
        let pid = self.surface_pane(id, &title, cols, rows);
        if let Some(p) = self.panes.get_mut(&pid) {
            p.plugin = Some((plugin.to_string(), args.to_string()));
        }
        match self.pl.running.get(plugin) {
            Some(r) if r.handle.key == owner => {
                r.handle.event(Event::SurfaceOpened((id, args.to_string())));
                r.handle.ui(id, UiEvent::Resize((cols.max(2) as u32, rows.max(1) as u32)));
            }
            _ => self.paint_orphan(id),
        }
        pid
    }

    /// Show `frame` on surface `id` (with its images).
    pub(super) fn show_frame(&mut self, id: u32, frame: Frame) -> Result<(), String> {
        proteus_wasm::accessibility::validate(&frame.accessibility)?;
        let Some(s) = self.pl.surfaces.get_mut(&id) else {
            return Err(format!("no surface {id}"));
        };
        let added = frame.accessibility.iter().filter(|n| !s.accessibility_ids.contains_key(&n.id)).count();
        s.next_accessibility_id
            .checked_add((added as u32).saturating_mul(4))
            .ok_or("accessibility node IDs exhausted")?;
        let keys: HashSet<_> = frame.accessibility.iter().map(|n| n.id.as_str()).collect();
        s.accessibility_ids.retain(|id, _| keys.contains(id.as_str()));
        for node in &frame.accessibility {
            if !s.accessibility_ids.contains_key(&node.id) {
                s.accessibility_ids.insert(node.id.clone(), s.next_accessibility_id);
                s.next_accessibility_id += 4;
            }
        }
        let term = match s.pane {
            Some(p) => match self.panes.get_mut(&p) {
                Some(p) => &mut p.term,
                None => return Err("surface pane was closed".into()),
            },
            None => &mut s.term,
        };
        let (cols, rows) = (term.cols(), term.lines());
        let mut out = paint(&frame, cols, rows);
        if !frame.images.is_empty() || !s.sent.is_empty() {
            out.push_str("\x1b_Ga=d,d=a,q=2\x1b\\");
            for place in frame.images.iter().take(64) {
                let Some(px) = self.pl.images.get(&(s.owner.id.clone(), place.image)) else { continue };
                if place.col as usize >= cols || place.row as usize >= rows {
                    continue;
                }
                let kid = place.image.wrapping_add(1).max(1);
                if s.sent.insert(place.image) {
                    out.push_str(&kitty_upload(kid, px));
                }
                let c = place.cols.clamp(1, (cols - place.col as usize) as u32);
                let r = place.rows.clamp(1, (rows - place.row as usize) as u32);
                out.push_str(&format!(
                    "\x1b[{};{}H\x1b_Ga=p,i={kid},c={c},r={r},C=1,q=2\x1b\\",
                    place.row + 1,
                    place.col + 1
                ));
            }
            out.push_str(&cursor_seq(&frame, cols, rows));
        }
        term.advance(out.as_bytes());
        let _ = term.take_pty_output();
        let _ = term.take_events();
        s.size = (cols, rows);
        s.frame = Some(frame);
        let win = s.win;
        self.dirty_win(win);
        Ok(())
    }

    /// Remove a surface without telling its plugin (a pane surface closes
    /// its pane).
    pub(super) fn drop_surface(&mut self, id: u32) {
        let Some(s) = self.pl.surfaces.remove(&id) else { return };
        if let Some(p) = s.pane {
            if self.panes.contains_key(&p) {
                self.close_pane(p);
            }
            return;
        }
        let win = s.win;
        if self.activate(win) {
            if self.w.surface_focus == Some(id) {
                self.w.surface_focus = None;
            }
            if self.w.surface_drag.is_some_and(|d| d.0 == id) {
                self.w.surface_drag = None;
            }
            self.relayout();
        }
        self.activate_kbd();
    }

    /// A pane was closed: if it showed a surface, the plugin hears of it.
    pub(super) fn surface_pane_gone(&mut self, surface: Option<u32>) {
        let Some(id) = surface else { return };
        if let Some(s) = self.pl.surfaces.remove(&id)
            && let Some(r) = self.pl.running.get(&s.owner.id).filter(|r| r.handle.key == s.owner)
        {
            r.handle.ui(id, UiEvent::Closed);
        }
    }

    pub(super) fn surface_ui(&mut self, id: u32, mut e: UiEvent) {
        let Some(s) = self.pl.surfaces.get(&id) else { return };
        if let UiEvent::Mouse(mouse) = &mut e {
            let t = match s.pane {
                Some(p) => self.panes.get(&p).map(|p| &p.term),
                None => Some(&s.term),
            };
            if let Some(t) = t {
                let row = (mouse.row as usize).min(t.lines() - 1);
                let map = t.visual_row(t.grid().viewport_line(row));
                let visual = (mouse.col as usize).min(t.cols() - 1);
                let logical = map.logical(visual);
                mouse.col = (logical + visual.saturating_sub(map.visual(logical))) as u32;
            }
        }
        if s.owner.id == CONFIG {
            self.sc.queue.push_back(super::scripting::ScriptCall::Ui(id, e));
            return;
        }
        if let Some(r) = self.pl.running.get(&s.owner.id).filter(|r| r.handle.key == s.owner) {
            r.handle.ui(id, e);
        }
    }

    /// Give a surface of the current window the keyboard.
    pub(super) fn focus_surface(&mut self, id: u32) {
        if self.w.surface_focus == Some(id) {
            return;
        }
        if let Some(old) = self.w.surface_focus.take() {
            self.surface_ui(old, UiEvent::Focus(false));
        }
        self.w.surface_focus = Some(id);
        self.surface_ui(id, UiEvent::Focus(true));
        self.w.dirty = true;
    }

    /// Keys go back to the panes.
    pub(super) fn unfocus_surface(&mut self) {
        if let Some(old) = self.w.surface_focus.take() {
            self.surface_ui(old, UiEvent::Focus(false));
            self.w.dirty = true;
        }
    }

    /// Widths (physical pixels, with the gap) taken by sidebars on the
    /// left and right of the current window.
    pub(super) fn sidebar_widths(&self) -> (i32, i32) {
        let cw = self.cell().0 as i32;
        let max = (self.physical().0 as i32 / 2 / cw).max(4) as u32;
        let (mut l, mut r) = (0, 0);
        for s in self.pl.surfaces.values().filter(|s| s.win == self.w.id) {
            let w = s.want.0.min(max) as i32 * cw + cw;
            match s.kind {
                SurfaceKind::SidebarLeft => l += w,
                SurfaceKind::SidebarRight => r += w,
                _ => {}
            }
        }
        (l, r)
    }

    /// Where this window's surfaces are: sidebars from the outside in, then
    /// floats in the middle of the pane area.
    pub(super) fn surfaces_here(&self) -> Vec<Placed> {
        let (pw, ph) = self.physical();
        let p = self.pad() as i32;
        let (cw, ch) = (self.cell().0 as i32, self.cell().1 as i32);
        let py = self.pad_y() as i32;
        let mut top = py;
        if self.tab_bar_visible() {
            top += ch + py;
        }
        let mut bottom = ph as i32 - py;
        if self.cfg.hint_bar == crate::config::HintBar::Always {
            bottom -= ch + py;
        }
        let h = (bottom - top).max(2 * ch);
        let max = (pw as i32 / 2 / cw).max(4) as u32;
        let (mut left, mut right) = (p, pw as i32 - p);
        let mut out = Vec::new();
        for (&id, s) in self.pl.surfaces.iter().filter(|(_, s)| s.win == self.w.id) {
            let w = s.want.0.min(max) as i32 * cw;
            let x = match s.kind {
                SurfaceKind::SidebarLeft => {
                    left += w + cw;
                    left - w - cw
                }
                SurfaceKind::SidebarRight => {
                    right -= w + cw;
                    right + cw
                }
                _ => continue,
            };
            out.push(Placed {
                id,
                title: Rect::new(x, top, w, ch),
                content: Rect::new(x, top + ch, w, (h - ch) / ch * ch),
                float: false,
            });
        }
        let area = self.area();
        for (&id, s) in self
            .pl
            .surfaces
            .iter()
            .filter(|(_, s)| s.win == self.w.id && matches!(s.kind, SurfaceKind::Float | SurfaceKind::Overlay))
        {
            let cols = (s.want.0 as i32).min(area.w / cw - 2).max(4);
            let rows = (if s.want.1 == 0 { 10 } else { s.want.1 as i32 }).min(area.h / ch - 3).max(1);
            let (w, hh) = (cols * cw, rows * ch);
            let (x, y) = match (s.kind, s.anchor) {
                // Below the cursor's cell, or above it when there is no room.
                (SurfaceKind::Overlay, Some((ax, ay))) => {
                    let x = ax.min(area.right() - w).max(area.x);
                    let y = if ay + ch + ch + hh <= area.bottom() { ay + ch } else { (ay - ch - hh).max(area.y) };
                    (x, y)
                }
                _ => (area.x + (area.w - w) / 2, area.y + (area.h - hh - ch) / 2),
            };
            out.push(Placed { id, title: Rect::new(x, y, w, ch), content: Rect::new(x, y + ch, w, hh), float: true });
        }
        out
    }

    /// Fit this window's surface terminals to their places; plugins hear
    /// about new sizes (and their last frame is painted again).
    pub(super) fn relayout_surfaces(&mut self) {
        let (cw, ch) = (self.cell().0 as i32, self.cell().1 as i32);
        // Pane surfaces: their panes were resized.
        let resized: Vec<(u32, (usize, usize))> = self
            .pl
            .surfaces
            .iter()
            .filter_map(|(&id, s)| {
                let t = &self.panes.get(&s.pane?)?.term;
                let size = (t.cols(), t.lines());
                (size != s.size).then_some((id, size))
            })
            .collect();
        for (id, size) in resized {
            let Some(s) = self.pl.surfaces.get_mut(&id) else { continue };
            s.size = size;
            s.sent.clear();
            if let Some(f) = s.frame.take() {
                let _ = self.show_frame(id, f);
            }
            self.surface_ui(id, UiEvent::Resize((size.0 as u32, size.1 as u32)));
        }
        for p in self.surfaces_here() {
            let cols = (p.content.w / cw).max(1) as usize;
            let rows = (p.content.h / ch).max(1) as usize;
            let Some(s) = self.pl.surfaces.get_mut(&p.id) else { continue };
            if s.term.cols() == cols && s.term.lines() == rows {
                continue;
            }
            s.term = surface_term(cols, rows);
            s.term.set_bidi(self.cfg.bidi);
            s.term.set_palette(self.cfg.theme.palette());
            s.term.set_cell_pixel_size(cw as u32, ch as u32);
            s.sent.clear();
            s.size = (cols, rows);
            if let Some(f) = s.frame.take() {
                let _ = self.show_frame(p.id, f);
            }
            self.surface_ui(p.id, UiEvent::Resize((cols as u32, rows as u32)));
        }
    }

    /// Surface (and cell inside it) at a logical position.
    pub(super) fn surface_at(&self, pos: (f64, f64)) -> Option<(u32, Option<(u32, u32)>)> {
        let (x, y) = self.phys(pos);
        let (cw, ch) = (self.cell().0 as i32, self.cell().1 as i32);
        // Floats are on top.
        for p in self.surfaces_here().iter().rev() {
            if p.content.contains(x, y) {
                return Some((p.id, Some((((x - p.content.x) / cw) as u32, ((y - p.content.y) / ch) as u32))));
            }
            if p.title.contains(x, y) {
                return Some((p.id, None));
            }
        }
        None
    }

    /// A mouse button on a surface: focus it, tell its plugin. Ctrl+click
    /// opens a link.
    pub(super) fn surface_press(&mut self, id: u32, cell: Option<(u32, u32)>, button: PButton) {
        if self.pl.surfaces.get(&id).is_some_and(|s| s.pane.is_none()) {
            self.focus_surface(id);
        } else {
            self.unfocus_surface();
        }
        let Some((col, row)) = cell else { return };
        let uri = self.pl.surfaces.get(&id).and_then(|s| {
            let t = match s.pane {
                Some(p) => &self.panes.get(&p)?.term,
                None => &s.term,
            };
            let logical = t.visual_row(t.grid().viewport_line(row as usize)).logical(col as usize);
            let cell = t.grid().row(row as usize).cells.get(logical)?;
            Some(t.hyperlink(t.style(cell.style).hyperlink?)?.uri.clone())
        });
        if button == PButton::Left
            && self.mods.ctrl
            && let Some(uri) = uri
        {
            self.xdg_open(&uri, None);
            return;
        }
        self.w.surface_drag = Some((id, button, (col, row)));
        self.surface_mouse(id, button, types::MouseAction::Press, (col, row));
    }

    fn surface_mouse(&mut self, id: u32, button: PButton, action: types::MouseAction, (col, row): (u32, u32)) {
        let mods = surface_mods(&self.mods);
        self.surface_ui(id, UiEvent::Mouse(types::MouseEvent { button, action, col, row, mods }));
    }

    /// The cell of surface `id` under a logical position, clamped to it.
    fn surface_cell(&self, id: u32, pos: (f64, f64)) -> Option<(u32, u32)> {
        let s = self.pl.surfaces.get(&id)?;
        if let Some(p) = s.pane {
            return self.cell_in(p, pos).map(|(c, r, _)| (c as u32, r as u32));
        }
        let p = self.surfaces_here().into_iter().find(|p| p.id == id)?;
        let (x, y) = self.phys(pos);
        let (cw, ch) = (self.cell().0 as i32, self.cell().1 as i32);
        let col = ((x - p.content.x).max(0) / cw).min((p.content.w / cw - 1).max(0));
        let row = ((y - p.content.y).max(0) / ch).min((p.content.h / ch - 1).max(0));
        Some((col as u32, row as u32))
    }

    /// The pointer moved: a drag on a surface goes to it.
    pub(super) fn surface_motion(&mut self, pos: (f64, f64)) -> bool {
        let Some((id, button, last)) = self.w.surface_drag else { return false };
        let Some(cell) = self.surface_cell(id, pos) else { return true };
        if cell != last {
            self.w.surface_drag = Some((id, button, cell));
            self.surface_mouse(id, button, types::MouseAction::Drag, cell);
        }
        true
    }

    /// A button released after a press on a surface.
    pub(super) fn surface_release(&mut self, pos: (f64, f64)) -> bool {
        let Some((id, button, last)) = self.w.surface_drag.take() else { return false };
        let paths = self.w.drag_paths.take();
        if let Some(paths) = paths.filter(|p| !p.is_empty())
            && self.surface_at(pos).is_none()
            && let Some(p) = self.pane_at(pos).filter(|p| self.panes.get(p).is_some_and(|p| p.surface.is_none()))
        {
            use crate::script::Host as _;
            let text: Vec<String> =
                paths.iter().map(|p| super::hint::shell_quote(&clean(p).collect::<String>())).collect();
            self.send_text(p, &format!("{} ", text.join(" ")), true);
        }
        let cell = self.surface_cell(id, pos).unwrap_or(last);
        self.surface_mouse(id, button, types::MouseAction::Release, cell);
        true
    }

    /// The composition changed: the surface with the keyboard hears it.
    pub(super) fn surface_preedit(&mut self) {
        if self.w.input.is_some() {
            return;
        }
        let Some(s) = self.key_surface() else { return };
        let p = self.ime.as_ref().and_then(|i| i.preedit.as_ref());
        let e = p.map(|p| (p.text.clone(), p.cursor.map_or(p.text.len(), |c| c.0) as u32));
        self.surface_ui(s, UiEvent::Preedit(e));
    }

    /// Where the input method's cursor goes on a focused sidebar or float:
    /// rectangle (physical pixels), columns and the cursor cell.
    pub(super) fn surface_ime_spot(&self) -> Option<(Rect, usize, (usize, usize))> {
        let id = self.w.surface_focus?;
        let p = self.surfaces_here().into_iter().find(|p| p.id == id)?;
        let t = &self.pl.surfaces.get(&id)?.term;
        let x = t.visual_row(t.grid().abs_line(t.cursor().y)).visual(t.cursor().x);
        Some((p.content, t.cols(), (x, t.cursor().y)))
    }

    /// Files dropped on surface `id`.
    pub(super) fn surface_dropped(&mut self, id: u32, uri_list: &str) {
        let paths = uri_paths(uri_list);
        if !paths.is_empty() {
            self.surface_ui(id, UiEvent::Drop(paths));
        }
    }

    /// A key for the focused surface.
    pub(super) fn surface_key(&mut self, id: u32, key: proteus_core::input::Key, text: &str, repeat: bool) {
        use proteus_core::input::Key as K;
        let name = match key {
            K::Char(c) => c.to_lowercase().to_string(),
            K::Enter => "enter".into(),
            K::Tab => "tab".into(),
            K::Backspace => "backspace".into(),
            K::Escape => "escape".into(),
            K::Up => "up".into(),
            K::Down => "down".into(),
            K::Left => "left".into(),
            K::Right => "right".into(),
            K::Home => "home".into(),
            K::End => "end".into(),
            K::PageUp => "page-up".into(),
            K::PageDown => "page-down".into(),
            K::Insert => "insert".into(),
            K::Delete => "delete".into(),
            K::F(n) => format!("f{n}"),
            K::Keypad(_) | K::Code(_) => return,
        };
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        let mods = surface_mods(&self.mods);
        self.surface_ui(id, UiEvent::Key(types::KeyEvent { key: name, text, mods, repeat }));
    }

    /// Frame parts of this window's surfaces: title labels, dividers, and
    /// the panes to draw (sidebars under the floating panes, floats over).
    pub(super) fn surface_parts(&self, theme: &render::Theme) -> SurfaceParts {
        let (mut tiled, mut floats, mut rects, mut labels) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let cw = self.cell().0 as f32;
        let thick = (self.cfg.border_width * self.w.scale) as f32;
        for p in self.surfaces_here() {
            let Some(s) = self.pl.surfaces.get(&p.id) else { continue };
            let on = self.w.surface_focus == Some(p.id) && self.w.focused;
            let (fg, bg) = if on { (theme.bg, theme.accent) } else { (theme.fg, theme.bar) };
            let label = render::Label {
                x: p.title.x as f32,
                y: p.title.y as f32,
                cols: (p.title.w as f32 / cw) as usize,
                text: format!(" {}", s.title),
                fg,
                bg: Some(bg),
                bold: on,
            };
            let mut part = super::PanePart::surface(p.id, p.content, on, &s.term);
            if on && self.w.input.is_none() {
                part.preedit = self.ime.as_ref().and_then(|i| i.preedit.clone());
            }
            if p.float {
                let c = p.content;
                let line = if on { theme.accent } else { theme.dim };
                let (x, y, w, h) = (
                    c.x as f32 - thick,
                    p.title.y as f32,
                    c.w as f32 + 2.0 * thick,
                    (c.bottom() - p.title.y) as f32 + thick,
                );
                part.backdrop = vec![
                    ([x, y, w, h], theme.bg),
                    ([x, y, thick, h], line),
                    ([x + w - thick, y, thick, h], line),
                    ([x, y + h - thick, w, thick], line),
                ];
                part.labels.push(label);
                part.floating = true;
                floats.push(part);
            } else {
                labels.push(label);
                // A divider in the gap towards the panes.
                let left_side = s.kind == SurfaceKind::SidebarLeft;
                let gx = if left_side { p.title.right() as f32 + cw / 2.0 } else { p.title.x as f32 - cw / 2.0 };
                let h = (p.content.bottom() - p.title.y) as f32;
                rects.push(([(gx - thick / 2.0).round(), p.title.y as f32, thick, h], theme.dim));
                tiled.push(part);
            }
        }
        (tiled, floats, rects, labels)
    }
}

/// Local paths of a `text/uri-list` (RFC 2483); other URIs are skipped.
fn uri_paths(list: &str) -> Vec<String> {
    list.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let rest = l.strip_prefix("file://")?;
            // `file://host/path`: only this machine.
            let path = &rest[rest.find('/')?..];
            let mut out = Vec::new();
            let b = path.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'%'
                    && let Some(v) = path.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            String::from_utf8(out).ok().filter(|p| !p.contains('\0'))
        })
        .take(100)
        .collect()
}

fn surface_mods(m: &smithay_client_toolkit::seat::keyboard::Modifiers) -> types::Modifiers {
    let mut out = types::Modifiers::empty();
    for (on, bit) in [
        (m.shift, types::Modifiers::SHIFT),
        (m.ctrl, types::Modifiers::CTRL),
        (m.alt, types::Modifiers::ALT),
        (m.logo, types::Modifiers::LOGO),
    ] {
        if on {
            out |= bit;
        }
    }
    out
}

/// A key suggested by a plugin: `KEY` (Normal mode) or `MODE:KEY`.
fn suggested_key(s: &str) -> Option<(modes::Mode, modes::Chord)> {
    let (mode, key) = match s.split_once(':') {
        Some((m, k)) if !k.is_empty() => (modes::Mode::from_key(m)?, k),
        _ => (modes::Mode::Normal, s),
    };
    Some((mode, modes::Chord::parse(key).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proteus_wasm::types::{Span, Style};

    fn span(t: &str, style: Style) -> Span {
        Span { text: t.into(), style, link: None }
    }

    #[test]
    fn action_ids_are_stable_and_reregistration_replaces_defaults_and_labels() {
        let mut actions = Vec::new();
        upsert_action(
            &mut actions,
            RegisteredAction::parse("sidebar.toggle".into(), "Files: sidebar".into(), Some("alt+e".into())).unwrap(),
        );
        let identity = modes::plugin_action("proteus.files", &actions[0].id).unwrap();
        upsert_action(
            &mut actions,
            RegisteredAction::parse("sidebar.toggle".into(), "Browse files".into(), Some("pane:E".into())).unwrap(),
        );
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].name, "Browse files");
        assert_eq!(actions[0].default, Some((modes::Mode::Pane, modes::Chord::parse("E").unwrap())));
        assert_eq!(modes::plugin_action("proteus.files", &actions[0].id), Some(identity));
        upsert_action(
            &mut actions,
            RegisteredAction::parse("sidebar.toggle".into(), "Browse files".into(), None).unwrap(),
        );
        assert!(actions[0].default.is_none());
        assert!(RegisteredAction::parse("id".into(), "label".into(), Some("nope:e".into())).is_err());
    }

    #[test]
    fn suggested_keys() {
        use modes::Mode;
        let c = |s| modes::Chord::parse(s).unwrap();
        assert_eq!(suggested_key("alt+e"), Some((Mode::Normal, c("alt+e"))));
        assert_eq!(suggested_key("pane:e"), Some((Mode::Pane, c("e"))));
        assert_eq!(suggested_key("ctrl+:"), Some((Mode::Normal, c("ctrl+:"))));
        assert_eq!(suggested_key("nope:e"), None);
    }

    #[test]
    fn frames_paint_safely() {
        let plain = Style { fg: None, bg: None, attrs: Attrs::empty() };
        let bold_red = Style { fg: Some(Color::Indexed(1)), bg: None, attrs: Attrs::BOLD };
        let frame = Frame {
            lines: vec![
                vec![span("ab", plain), span("中文字长", bold_red)],
                // An escape sequence in the text must not reach the terminal.
                vec![span("x\x1b]52;c;aGk=\x07y\u{9b}31mz", plain)],
                vec![Span { text: "link".into(), style: plain, link: Some("https://e.x/\x1b;q".into()) }],
            ],
            cursor: Some((1, 0)),
            background: None,
            images: Vec::new(),
            accessibility: Vec::new(),
        };
        let mut t = surface_term(6, 3);
        t.advance(paint(&frame, 6, 3).as_bytes());
        assert!(t.take_events().is_empty());
        assert_eq!(t.row_text(0), "ab中文");
        assert_eq!(t.row_text(1), "x]52;c");
        assert_eq!(t.row_text(2), "link");
        let st = t.style(t.grid().row(0).cells[2].style);
        assert!(st.attrs.contains(proteus_core::style::Attrs::BOLD));
        let link = t.style(t.grid().row(2).cells[0].style).hyperlink.and_then(|h| t.hyperlink(h)).unwrap();
        assert_eq!(link.uri, "https://e.x/q");
        assert_eq!((t.cursor().x, t.cursor().y, t.cursor_visible()), (1, 0, true));
        // Too many rows are cut off.
        let mut t = surface_term(4, 1);
        t.advance(paint(&frame, 4, 1).as_bytes());
        assert_eq!(t.screen_text(), "ab中");
    }

    #[test]
    fn dropped_uris() {
        let list = "# comment\r\nfile:///tmp/a%20b.txt\r\nfile://localhost/etc/x\r\nhttps://e.x/\r\n";
        assert_eq!(uri_paths(list), ["/tmp/a b.txt", "/etc/x"]);
    }

    #[test]
    fn events_for_plugins() {
        let ev = ScriptEvent::new("command_finished")
            .pane(3)
            .with("command", Val::Str("make".into()))
            .with("exit_code", Val::Int(2))
            .with("duration", Val::Num(1.5));
        let Some(Event::CommandFinished(c)) = plugin_event(&ev) else { panic!() };
        assert_eq!((c.pane, c.command.as_deref(), c.exit_code, c.duration_ms), (3, Some("make"), Some(2), Some(1500)));
        assert!(matches!(plugin_event(&ScriptEvent::new("bell").pane(1)), Some(Event::Bell(1))));
        assert!(plugin_event(&ScriptEvent::new("config_changed")).is_none());
    }
}
