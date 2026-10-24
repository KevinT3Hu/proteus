//! `proteus.files`: the first-party file manager (docs/design/03-file-manager.md).
//!
//! Three surfaces: a sidebar (`Alt+e`, a tree that follows the focused
//! pane's directory), a pane (`E` in Pane mode or a layout, Miller
//! columns) and a picker (`Alt+p`, a float; Enter inserts the path into the
//! pane's prompt). Everything goes through the public plugin API.

mod fsys;
mod git;
mod i18n;
mod ops;
mod preview;
mod render;
mod view;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use proteus_plugin::types::ProcessId;
use proteus_plugin::types::{
    Attrs, BlockState, Color, Command, Event, ImageId, Line, MouseAction, MouseButton, PaneId, RequestId, Span, Style,
    SurfaceId, SurfaceKind, SurfaceOptions, Theme, TimerId, UiEvent, WatchId,
};
use proteus_plugin::{Edit, Guest, TextInput, fit, host};

use fsys::{Sort, SortKey};
use i18n::tr;
use ops::{Choice, OpKind, Step, Transfer};
use proteus_plugin::types::{Placement, Spawn, TaskId};
use proteus_plugin::ui::Rect;
use render::preview_component;
use view::{Kind, Layout, View};

// Retain the registered action ID so existing user keymaps keep working.
const SIDEBAR: &str = "sidebar.toggle";
const PANE: &str = "pane.open";
const PICKER: &str = "picker.open";
const SIDEBAR_COLS: u32 = 32;

/// A pending two-key command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prefix {
    G,
    Mark,
    Jump,
    Sort,
}

/// One open surface.
struct Ui {
    view: View,
    size: (u32, u32),
    /// Emitted by the declarative renderer, used for keys and hit testing.
    list_area: Rect,
    prefix: Option<Prefix>,
    /// Editing the filter.
    input: Option<TextInput>,
    message: Option<(String, bool)>,
    /// Sidebar: follow the focused pane's directory.
    follow: bool,
    /// The focused pane is on another machine.
    remote: Option<String>,
    /// Picker: the pane it inserts into.
    target: Option<PaneId>,
    watch: Option<WatchId>,
    dragging: bool,
}

/// Settings owned and validated by this plugin, independent of host load options.
#[derive(proteus_plugin::serde::Deserialize)]
#[serde(crate = "proteus_plugin::serde", default, deny_unknown_fields)]
struct Settings {
    position: Position,
    width: u32,
}

#[derive(Default, proteus_plugin::serde::Deserialize)]
#[serde(crate = "proteus_plugin::serde", rename_all = "lowercase")]
enum Position {
    #[default]
    Left,
    Right,
}

impl Default for Settings {
    fn default() -> Self {
        Self { position: Position::Left, width: SIDEBAR_COLS }
    }
}

impl Settings {
    fn validate(self) -> Result<Self, String> {
        if !(1..=4096).contains(&self.width) {
            return Err("file manager config.width must be between 1 and 4096 columns".into());
        }
        Ok(self)
    }
}

#[derive(Default)]
struct State {
    settings: Settings,
    uis: HashMap<SurfaceId, Ui>,
    theme: Option<Theme>,
    sidebar: Option<SurfaceId>,
    picker: Option<SurfaceId>,
    /// Latest block state of each pane (shell integration).
    blocks: HashMap<PaneId, BlockState>,
    /// The last focused terminal pane (not a plugin pane).
    last_pane: Option<PaneId>,
    git: Git,
    watches: HashMap<WatchId, SurfaceId>,
    /// Surfaces to read again when the reload timer fires.
    stale: HashSet<SurfaceId>,
    reload_timer: Option<TimerId>,
    /// `z` prompts: request → surface.
    asks: HashMap<RequestId, (SurfaceId, Ask)>,
    /// Files yanked (`C`) or cut (`x`) for `P`.
    clip: Option<(OpKind, Vec<PathBuf>)>,
    op: Option<Running>,
    undo: Option<Undo>,
    /// A bulk rename being edited in a pane.
    bulk: Option<Bulk>,
    /// The surface the user works in (its cursor is previewed).
    active: Option<SurfaceId>,
    /// The sidebar's floating preview (`p`).
    float: Option<SurfaceId>,
    float_size: (u32, u32),
    preview: Option<Shown>,
    /// Waiting to preview this file (cursor moves are debounced).
    pending: Option<(PathBuf, i64)>,
    preview_timer: Option<TimerId>,
    job: Option<Job>,
}

/// What a `prompt` or `confirm` was for.
enum Ask {
    Zoxide,
    Rename(PathBuf),
    /// Create in this directory.
    New(PathBuf),
    Chmod(Vec<PathBuf>),
    Delete(Vec<PathBuf>),
    Bulk(Vec<(PathBuf, PathBuf)>),
}

/// A copy or move in the background.
struct Running {
    t: Transfer,
    task: Option<TaskId>,
    timer: Option<TimerId>,
    surface: SurfaceId,
    /// Waiting for the user: this destination exists.
    conflict: Option<PathBuf>,
    /// Undoing a move (it is not undone again).
    undoing: bool,
}

enum Undo {
    Renamed(Vec<(PathBuf, PathBuf)>),
    Moved(Vec<(PathBuf, PathBuf)>),
    Trashed(Vec<PathBuf>),
}

struct Bulk {
    pane: PaneId,
    surface: SurfaceId,
    file: PathBuf,
    dir: PathBuf,
    names: Vec<String>,
}

/// A preview of one file.
struct Shown {
    path: PathBuf,
    mtime: i64,
    /// Name, size, date, mime type.
    info: Vec<Line>,
    body: Body,
}

enum Body {
    Loading,
    Lines(Vec<Line>),
    /// Pixel size.
    Image(ImageId, (u32, u32)),
}

/// A helper program making a preview.
struct Job {
    pid: ProcessId,
    path: PathBuf,
    kind: JobKind,
    out: Vec<u8>,
    err: Vec<u8>,
}

enum JobKind {
    Archive,
    File,
    /// Writes the thumbnail at this path.
    Thumb(PathBuf),
}

/// Git status, cached per repository and refreshed in the background
/// (`git status` can take a second in a big tree; drawing never waits).
#[derive(Default)]
struct Git {
    off: bool,
    roots: HashMap<PathBuf, Option<PathBuf>>,
    status: HashMap<PathBuf, git::Status>,
    /// Repositories to ask again (shown with the old status meanwhile).
    stale: HashSet<PathBuf>,
    /// A running `git status`: its repository and output.
    running: Option<(ProcessId, PathBuf, Vec<u8>)>,
}

impl Git {
    /// The repository holding `dir`: the nearest parent with `.git`.
    fn root(&mut self, dir: &Path) -> Option<PathBuf> {
        if let Some(r) = self.roots.get(dir) {
            return r.clone();
        }
        let r = dir.ancestors().find(|a| a.join(".git").exists()).map(Path::to_path_buf);
        self.roots.insert(dir.to_path_buf(), r.clone());
        r
    }

    /// The cached status (possibly old); starts a refresh if needed.
    fn status(&mut self, dir: &Path) -> Option<&git::Status> {
        if self.off {
            return None;
        }
        let root = self.root(dir)?;
        let want = !self.status.contains_key(&root) || self.stale.contains(&root);
        if want && self.running.is_none() {
            self.stale.remove(&root);
            let argv = ["git", "status", "--porcelain=v1", "-z", "--ignored", "--untracked-files=normal"];
            let cmd = Command {
                argv: argv.map(String::from).to_vec(),
                cwd: Some(root.display().to_string()),
                env: vec![("GIT_OPTIONAL_LOCKS".into(), "0".into())],
            };
            match host::spawn_process(&cmd) {
                Ok(pid) => {
                    host::process_close_stdin(pid);
                    self.running = Some((pid, root.clone(), Vec::new()));
                }
                Err(e) => {
                    // Not installed or not allowed: stop asking.
                    host::log(&format!("git: {e}"));
                    self.off = true;
                }
            }
        }
        self.status.get(&root)
    }

    /// Output of a process; true if it was ours.
    fn output(&mut self, pid: ProcessId, stderr: bool, data: &[u8]) -> bool {
        match self.running.as_mut() {
            Some((p, _, out)) if *p == pid => {
                if !stderr && out.len() < 64 << 20 {
                    out.extend_from_slice(data);
                }
                true
            }
            _ => false,
        }
    }

    /// A process ended; true if it was ours (then redraw).
    fn exited(&mut self, pid: ProcessId, code: Option<i32>) -> bool {
        let Some((_, root, out)) = self.running.take_if(|r| r.0 == pid) else { return false };
        if code == Some(0) {
            self.status.insert(root.clone(), git::Status::parse(&root, &out));
        } else {
            // Not a repository after all.
            self.status.insert(root.clone(), git::Status::default());
        }
        true
    }

    fn forget(&mut self) {
        self.stale.extend(self.status.keys().cloned());
    }
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

// --- colors -------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Colors {
    fg: Color,
    bg: Color,
    accent: Color,
    dim: Color,
    bar: Color,
}

fn colors(t: Option<Theme>) -> Colors {
    match t {
        Some(t) => Colors {
            fg: Color::Rgb(t.fg),
            bg: Color::Rgb(t.bg),
            accent: Color::Rgb(t.accent),
            dim: Color::Rgb(t.dim),
            bar: Color::Rgb(t.bar),
        },
        None => Colors {
            fg: Color::Indexed(7),
            bg: Color::Indexed(0),
            accent: Color::Indexed(4),
            dim: Color::Indexed(8),
            bar: Color::Indexed(8),
        },
    }
}

fn style(fg: Option<Color>, bg: Option<Color>, attrs: Attrs) -> Style {
    Style { fg, bg, attrs }
}

fn span(text: impl Into<String>, style: Style) -> Span {
    Span { text: text.into(), style, link: None }
}

/// `YYYY-MM-DD HH:MM` (UTC) of seconds since the epoch.
fn date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

// --- actions ------------------------------------------------------------------

impl State {
    fn ui(&mut self, id: SurfaceId) -> Option<&mut Ui> {
        self.uis.get_mut(&id)
    }

    fn say(&mut self, id: SurfaceId, msg: impl Into<String>, err: bool) {
        if let Some(ui) = self.ui(id) {
            ui.message = Some((msg.into(), err));
        }
    }

    /// A terminal pane to act on: the focused one unless it is a plugin
    /// pane, else the last focused terminal pane.
    fn target_pane(&self, id: SurfaceId) -> Option<PaneId> {
        if let Some(t) = self.uis.get(&id).and_then(|u| u.target) {
            return Some(t);
        }
        let f = host::focused_pane().ok().flatten();
        match f.and_then(|f| host::get_pane(f).ok().flatten()) {
            Some(p) if p.process.is_some() => Some(p.id),
            _ => self.last_pane,
        }
    }

    fn open_ui(&mut self, kind: Kind, opts: SurfaceOptions, dir: Option<PathBuf>) -> Option<SurfaceId> {
        // Before opening: a new pane surface takes the focus.
        let focused = host::focused_pane().ok().flatten().and_then(|p| host::get_pane(p).ok().flatten());
        let target = if kind == Kind::Picker { self.target_pane(0) } else { None };
        let id = match host::open_surface(&opts) {
            Ok(id) => id,
            Err(e) => {
                host::notify(&tr!("plugin-error", error = e), true);
                return None;
            }
        };
        let remote = focused.as_ref().and_then(|p| host::pane_remote(p.id).ok().flatten());
        let start = dir
            .or_else(|| focused.as_ref().and_then(|p| p.cwd.clone()).map(PathBuf::from))
            .or_else(fsys::home)
            .unwrap_or_else(|| PathBuf::from("/"));
        let size = host::surface_size(id).unwrap_or((opts.cols.max(20), opts.rows.max(10)));
        let t0 = std::time::Instant::now();
        let ui = Ui {
            view: View::new(kind, start),
            size,
            list_area: Rect::default(),
            prefix: None,
            input: None,
            message: None,
            follow: kind == Kind::Sidebar,
            remote: remote.filter(|_| kind == Kind::Sidebar),
            target,
            watch: None,
            dragging: false,
        };
        let listed = t0.elapsed();
        let n = ui.view.rows.len();
        self.uis.insert(id, ui);
        self.active = Some(id);
        self.watch(id);
        self.draw(id);
        host::log(&format!("opened with {n} entries: listed in {listed:?}, drawn after {:?}", t0.elapsed()));
        Some(id)
    }

    fn close_ui(&mut self, id: SurfaceId) {
        if let Some(ui) = self.uis.remove(&id)
            && let Some(w) = ui.watch
        {
            host::unwatch(w);
            self.watches.remove(&w);
        }
        if self.sidebar == Some(id) {
            self.sidebar = None;
            if let Some(f) = self.float.take() {
                host::close_surface(f);
            }
        }
        if self.active == Some(id) {
            self.active = None;
        }
        if self.picker == Some(id) {
            self.picker = None;
        }
        self.stale.remove(&id);
    }

    fn open_sidebar(&mut self) {
        if let Some(id) = self.sidebar {
            let _ = host::focus_surface(id);
            return;
        }
        let opts = SurfaceOptions {
            kind: match self.settings.position {
                Position::Left => SurfaceKind::SidebarLeft,
                Position::Right => SurfaceKind::SidebarRight,
            },
            title: tr!("title-files"),
            cols: self.settings.width,
            rows: 0,
            focus: true,
            pane: None,
        };
        self.sidebar = self.open_ui(Kind::Sidebar, opts, None);
    }

    fn open_picker(&mut self) {
        if let Some(id) = self.picker {
            let _ = host::focus_surface(id);
            return;
        }
        let opts = SurfaceOptions {
            kind: SurfaceKind::Float,
            title: tr!("title-picker"),
            cols: 90,
            rows: 26,
            focus: true,
            pane: None,
        };
        self.picker = self.open_ui(Kind::Picker, opts, None);
    }

    fn open_pane(&mut self) {
        let opts = SurfaceOptions {
            kind: SurfaceKind::PaneRight,
            title: tr!("title-files"),
            cols: 0,
            rows: 0,
            focus: true,
            pane: None,
        };
        self.open_ui(Kind::Pane, opts, None);
    }

    /// Watch the directory a surface shows.
    fn watch(&mut self, id: SurfaceId) {
        let Some(ui) = self.uis.get_mut(&id) else { return };
        if let Some(w) = ui.watch.take() {
            host::unwatch(w);
            self.watches.remove(&w);
        }
        if let Ok(w) = host::watch(&ui.view.dir.display().to_string()) {
            ui.watch = Some(w);
            self.watches.insert(w, id);
        }
    }

    fn cd(&mut self, id: SurfaceId, dir: PathBuf, focus: Option<&Path>) {
        let Some(ui) = self.ui(id) else { return };
        let old = ui.view.dir.clone();
        ui.view.cd(dir, focus);
        if ui.view.dir != old {
            self.watch(id);
        }
    }

    /// Enter a directory, or open / pick a file.
    fn activate(&mut self, id: SurfaceId) {
        let Some(ui) = self.ui(id) else { return };
        let Some(r) = ui.view.current().cloned() else { return };
        if r.entry.dir {
            if ui.view.layout == Layout::Tree {
                return ui.view.toggle_open();
            }
            return self.cd(id, r.path, None);
        }
        if ui.view.kind == Kind::Picker {
            return self.insert(id);
        }
        let text = preview::head(&r.path, 4096).is_ok_and(|h| preview::is_text(&h));
        if text {
            return self.edit_in(id, Placement::SplitRight);
        }
        if let Err(e) = host::open(&r.path.display().to_string()) {
            self.say(id, e, true);
        }
    }

    fn back(&mut self, id: SurfaceId) {
        let Some(ui) = self.ui(id) else { return };
        let old = ui.view.dir.clone();
        if ui.view.layout == Layout::Tree {
            ui.view.tree_back();
        } else {
            ui.view.up();
        }
        if ui.view.dir != old {
            self.watch(id);
        }
    }

    /// `i`: paste the paths into the pane.
    fn insert(&mut self, id: SurfaceId) {
        let Some(paths) = self.uis.get(&id).map(|u| u.view.targets()).filter(|p| !p.is_empty()) else { return };
        let Some(pane) = self.target_pane(id) else { return self.say(id, tr!("no-insert-pane"), true) };
        let text: Vec<String> = paths.iter().map(|p| fsys::shell_quote(&p.display().to_string())).collect();
        match host::send_text(pane, &format!("{} ", text.join(" ")), true) {
            Ok(()) if self.uis.get(&id).is_some_and(|u| u.view.kind == Kind::Picker) => {
                host::close_surface(id);
                self.close_ui(id);
                let _ = host::focus_pane(pane);
            }
            Ok(()) => {
                if let Some(ui) = self.ui(id) {
                    ui.view.selected.clear();
                }
                self.say(id, tr!("inserted-paths", count = paths.len()), false);
            }
            Err(e) => self.say(id, e, true),
        }
    }

    /// `c`: cd the pane here, if it sits at a prompt.
    fn cd_pane(&mut self, id: SurfaceId) {
        let Some(ui) = self.uis.get(&id) else { return };
        let dir = match ui.view.current() {
            Some(r) if r.entry.dir && ui.view.layout == Layout::Tree => r.path.clone(),
            _ => ui.view.dir.clone(),
        };
        let Some(pane) = self.target_pane(id) else { return self.say(id, tr!("no-pane"), true) };
        if self.blocks.get(&pane) != Some(&BlockState::Prompt) {
            return self.say(id, tr!("not-at-prompt"), true);
        }
        let cmd = format!("cd -- {}\r", fsys::shell_quote(&dir.display().to_string()));
        match host::send_text(pane, &cmd, false) {
            Ok(()) => self.say(id, tr!("changed-directory", path = fsys::tilde(&dir, fsys::home().as_deref())), false),
            Err(e) => self.say(id, e, true),
        }
    }

    fn copy_paths(&mut self, id: SurfaceId) {
        let Some(paths) = self.uis.get(&id).map(|u| u.view.targets()).filter(|p| !p.is_empty()) else { return };
        let text: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        match host::clipboard_write(&text.join("\n")) {
            Ok(()) => self.say(id, tr!("copied-paths", paths = fit(&text.join(" "), 60)), false),
            Err(e) => self.say(id, e, true),
        }
    }

    fn bookmark(&mut self, id: SurfaceId, key: &str) {
        let Some(dir) = self.uis.get(&id).map(|u| u.view.dir.clone()) else { return };
        match host::storage_set(&format!("mark:{key}"), &dir.display().to_string()) {
            Ok(()) => {
                self.say(id, tr!("mark-set", key = key, path = fsys::tilde(&dir, fsys::home().as_deref())), false)
            }
            Err(e) => self.say(id, e, true),
        }
    }

    fn jump(&mut self, id: SurfaceId, key: &str) {
        match host::storage_get(&format!("mark:{key}")).map(PathBuf::from) {
            Some(d) if d.is_dir() => self.go(id, d),
            Some(d) => self.say(id, tr!("path-gone", path = d.display().to_string()), true),
            None => self.say(id, tr!("no-mark", key = key), true),
        }
    }

    /// Jump somewhere by hand: a sidebar stops following.
    fn go(&mut self, id: SurfaceId, dir: PathBuf) {
        if let Some(ui) = self.ui(id) {
            ui.follow = false;
        }
        self.cd(id, dir, None);
    }

    fn zoxide(&mut self, id: SurfaceId) {
        match host::prompt(&tr!("zoxide-prompt"), "") {
            Ok(r) => {
                self.asks.insert(r, (id, Ask::Zoxide));
            }
            Err(e) => self.say(id, e, true),
        }
    }

    fn zoxide_answer(&mut self, id: SurfaceId, query: &str) {
        let mut argv = vec!["zoxide".to_string(), "query".into(), "--".into()];
        argv.extend(query.split_whitespace().map(String::from));
        let cmd = Command { argv, cwd: None, env: vec![] };
        match host::run(&cmd, &[], 3000) {
            Ok(o) if o.exit_code == Some(0) => {
                let d = PathBuf::from(String::from_utf8_lossy(&o.stdout).trim_end().to_string());
                self.go(id, d);
            }
            Ok(_) => self.say(id, tr!("zoxide-no-match", query = query), true),
            Err(e) => self.say(id, tr!("zoxide-error", error = e), true),
        }
    }

    /// The sidebar follows the focused pane.
    fn follow(&mut self) {
        let Some(id) = self.sidebar else { return };
        if !self.uis.get(&id).is_some_and(|u| u.follow) {
            return;
        }
        let Some(p) = host::focused_pane().ok().flatten().and_then(|p| host::get_pane(p).ok().flatten()) else {
            return;
        };
        if p.process.is_none() && p.cwd.is_none() {
            return; // a plugin pane
        }
        let remote = host::pane_remote(p.id).ok().flatten();
        let local = remote.is_none();
        if let Some(ui) = self.ui(id) {
            ui.remote = remote;
        }
        if local
            && let Some(cwd) = p.cwd.map(PathBuf::from)
            && self.uis.get(&id).is_some_and(|u| u.view.dir != cwd)
        {
            self.cd(id, cwd, None);
        }
        self.draw(id);
    }

    fn key(&mut self, id: SurfaceId, key: &str, text: &str, ctrl: bool) {
        let rows = self.uis.get(&id).map_or(0, |u| u.list_area.height);
        if self.op.as_ref().is_some_and(|o| o.surface == id && o.conflict.is_some()) {
            let choice = match text {
                "o" | "O" => Some(Choice::Overwrite),
                "s" | "S" => Some(Choice::Skip),
                "r" | "R" => Some(Choice::Rename),
                _ => None,
            };
            if let Some(c) = choice {
                let all = text.chars().all(char::is_uppercase);
                if let Some(o) = self.op.as_mut() {
                    o.t.resolve(c, all);
                    o.conflict = None;
                }
                self.op_tick();
            } else if key == "escape" {
                self.op_cancel();
            }
            return;
        }
        let Some(ui) = self.ui(id) else { return };
        ui.message = None;

        // The filter field takes the keys while open.
        if let Some(input) = ui.input.as_mut() {
            match input.key(key, text, ctrl) {
                Edit::Changed => {
                    let f = input.text.clone();
                    ui.view.set_filter(&f);
                }
                Edit::Submit => ui.input = None,
                Edit::Cancel => {
                    ui.input = None;
                    ui.view.set_filter("");
                }
                Edit::Moved | Edit::Ignored => {}
            }
            return;
        }

        if let Some(p) = ui.prefix.take() {
            match (p, text) {
                (_, "") if key != "escape" => ui.prefix = Some(p), // a modifier
                (Prefix::G, "g") => {
                    ui.view.list.selected = 0;
                    ui.view.update_visual();
                }
                (Prefix::Mark, t) if t.chars().count() == 1 && key != "escape" => self.bookmark(id, t),
                (Prefix::Jump, t) if t.chars().count() == 1 && key != "escape" => self.jump(id, t),
                (Prefix::Sort, t) => {
                    let s = ui.view.sort;
                    let sort = match t {
                        "n" => Sort { key: SortKey::Name, ..s },
                        "m" => Sort { key: SortKey::Mtime, ..s },
                        "s" => Sort { key: SortKey::Size, ..s },
                        "e" => Sort { key: SortKey::Ext, ..s },
                        "d" => Sort { dirs_first: !s.dirs_first, ..s },
                        "r" => Sort { reverse: !s.reverse, ..s },
                        _ => s,
                    };
                    if sort != s {
                        ui.view.set_sort(sort);
                    }
                }
                _ => {}
            }
            return;
        }
        if ctrl {
            return;
        }

        match (key, text) {
            (_, "g") => ui.prefix = Some(Prefix::G),
            (_, "m") => ui.prefix = Some(Prefix::Mark),
            (_, "'") => ui.prefix = Some(Prefix::Jump),
            (_, ",") => ui.prefix = Some(Prefix::Sort),
            ("enter", _) | (_, "l" | "o") | ("right", _) => self.activate(id),
            (_, "h") | ("left", _) => self.back(id),
            (_, "-") => {
                if let Some(ui) = self.ui(id) {
                    ui.follow = false;
                    ui.view.up();
                }
                self.watch(id);
            }
            (_, "~") => {
                if let Some(h) = fsys::home() {
                    self.go(id, h);
                }
            }
            (_, ".") => ui.view.toggle_hidden(),
            (_, "/") => ui.input = Some(TextInput::new(&ui.view.filter.clone())),
            (_, "z") => self.zoxide(id),
            (_, "w") => {
                let l = ui.view.layout.next();
                ui.view.set_layout(l);
                let label = l.label();
                self.say(id, tr!("view-changed", layout = tr!(label)), false);
            }
            (_, " ") => ui.view.toggle_selected(),
            (_, "v") => ui.view.toggle_visual(),
            (_, "C") => self.yank(id, OpKind::Copy),
            (_, "x") => self.yank(id, OpKind::Move),
            (_, "P") => self.paste(id),
            (_, "r") => self.ask_rename(id),
            (_, "n") => self.ask_new(id),
            (_, "L") => self.link(id),
            (_, "M") => self.ask_chmod(id),
            (_, "d") => self.trash(id),
            (_, "D") => self.ask_delete(id),
            (_, "u") => self.undo(id),
            (_, "b") => self.bulk_rename(id),
            (_, "s") => self.edit_in(id, Placement::SplitRight),
            (_, "t") => self.edit_in(id, Placement::NewTab),
            (_, "f") => self.edit_in(id, Placement::Float),
            (_, "*") => ui.view.invert_selected(),
            (_, "c") => self.cd_pane(id),
            (_, "i") => self.insert(id),
            (_, "y") => self.copy_paths(id),
            (_, "p") if ui.view.kind == Kind::Sidebar => self.toggle_float(),
            (_, "F") if ui.view.kind == Kind::Sidebar => {
                ui.follow = !ui.follow;
                let f = ui.follow;
                self.say(id, tr!(if f { "following" } else { "pinned" }), false);
                if f {
                    self.follow();
                }
            }
            (_, "R") => {
                ui.view.reload();
                self.git.forget();
            }
            (_, "q") => {
                host::close_surface(id);
                self.close_ui(id);
            }
            ("escape", _) => {
                if ui.view.visual.is_some() {
                    ui.view.visual = None;
                } else if !ui.view.filter.is_empty() {
                    ui.view.set_filter("");
                } else if !ui.view.selected.is_empty() {
                    ui.view.selected.clear();
                } else if ui.view.kind == Kind::Picker {
                    host::close_surface(id);
                    self.close_ui(id);
                } else if let Some(p) = self.target_pane(id) {
                    let _ = host::focus_pane(p);
                }
            }
            (k, t) => {
                if ui.view.list.key(k, t, rows) {
                    ui.view.update_visual();
                }
            }
        }
    }

    fn mouse(&mut self, id: SurfaceId, m: proteus_plugin::types::MouseEvent) {
        let Some(ui) = self.ui(id) else { return };
        let area = ui.list_area;
        let inside = area.contains(m.col as usize, m.row as usize);
        match (m.button, m.action) {
            (MouseButton::WheelUp, _) if inside => ui.view.list.scroll(-3, area.height),
            (MouseButton::WheelDown, _) if inside => ui.view.list.scroll(3, area.height),
            (MouseButton::Left, MouseAction::Press) => {
                ui.dragging = false;
                let Some(i) = ui.view.list.at_point(area, m.col as usize, m.row as usize) else { return };
                if ui.view.list.selected == i {
                    self.activate(id);
                } else {
                    ui.view.list.selected = i;
                }
            }
            (MouseButton::Left, MouseAction::Drag) if !ui.dragging && inside => {
                ui.dragging = true;
                let paths: Vec<String> = ui.view.targets().iter().map(|p| p.display().to_string()).collect();
                let _ = host::start_drag(id, &paths);
            }
            (MouseButton::Left, MouseAction::Release) => ui.dragging = false,
            _ => {}
        }
    }

    fn fs_changed(&mut self, watch: WatchId) {
        let Some(&id) = self.watches.get(&watch) else { return };
        self.stale.insert(id);
        if self.reload_timer.is_none() {
            self.reload_timer = Some(host::set_timer(150, false));
        }
    }

    fn reload_stale(&mut self) {
        self.reload_timer = None;
        self.git.forget();
        for id in std::mem::take(&mut self.stale) {
            if let Some(ui) = self.ui(id) {
                ui.view.reload();
            }
            self.draw(id);
        }
    }
}

// --- previews -----------------------------------------------------------------

/// Output kept from a preview helper.
const JOB_OUTPUT: usize = 256 << 10;

impl State {
    /// The file whose preview is shown now, if any.
    fn wanted(&self) -> Option<(PathBuf, i64)> {
        let ui = self.uis.get(&self.active?)?;
        let r = ui.view.current().filter(|r| !r.entry.dir)?;
        let miller = ui.view.layout == Layout::Miller && ui.size.0 >= 30;
        let float = ui.view.kind == Kind::Sidebar && self.float.is_some();
        (miller || float).then(|| (r.path.clone(), r.entry.mtime))
    }

    /// Preview the cursor's file soon (moving on cancels it).
    fn sync_preview(&mut self) {
        let want = self.wanted();
        let have = self.preview.as_ref().map(|p| (p.path.clone(), p.mtime));
        if want.is_none() || want == have || want == self.pending {
            return;
        }
        self.pending = want;
        if let Some(t) = self.preview_timer.take() {
            host::cancel_timer(t);
        }
        self.preview_timer = Some(host::set_timer(60, false));
    }

    fn clear_preview(&mut self) {
        if let Some(j) = self.job.take() {
            host::process_kill(j.pid);
        }
        if let Some(Shown { body: Body::Image(id, _), .. }) = self.preview.take() {
            host::drop_image(id);
        }
    }

    fn redraw_preview(&mut self) {
        if let Some(a) = self.active {
            self.draw(a);
        }
        if self.active != self.sidebar {
            self.draw_float();
        }
    }

    fn preview_now(&mut self) {
        self.preview_timer = None;
        let Some((path, mtime)) = self.pending.take() else { return };
        self.clear_preview();
        let c = colors(self.theme);
        let dim = style(Some(c.dim), None, Attrs::empty());
        let line = |t: String| vec![span(t, dim)];
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let ps = path.display().to_string();
        let meta = std::fs::metadata(&path);
        let size = meta.as_ref().map_or(0, |m| m.len());
        let mime = host::mime_type(&ps).unwrap_or_default();
        let mut info = vec![
            vec![span(name.clone(), style(None, None, Attrs::BOLD))],
            line(tr!("preview-size", size = fsys::human(size), count = size)),
            line(tr!("preview-modified", date = date(mtime))),
        ];
        if !mime.is_empty() {
            info.push(line(mime.clone()));
        }
        let mut sh = Shown { path: path.clone(), mtime, info, body: Body::Lines(Vec::new()) };
        let head = match preview::head(&path, preview::TEXT_BYTES) {
            Ok(h) => h,
            Err(e) => {
                sh.info.push(vec![span(e.to_string(), style(Some(Color::Indexed(1)), None, Attrs::empty()))]);
                self.preview = Some(sh);
                return self.redraw_preview();
            }
        };
        let what = preview::classify(&mime, &name, &head[..head.len().min(8192)]);
        let fail = |sh: &mut Shown, e: String| sh.info.push(line(fit(&e, 200)));
        match what {
            preview::What::Text => {
                let text = String::from_utf8_lossy(&head);
                let dark = self.theme.is_none_or(|t| t.dark);
                sh.body = Body::Lines(preview::highlight(&name, &text, 200, 500, dark, c.dim));
            }
            preview::What::Image | preview::What::Svg if size > preview::IMAGE_BYTES => {
                fail(&mut sh, tr!("preview-too-big"));
            }
            preview::What::Image => match load_image_file(&path) {
                Ok((id, px)) => {
                    sh.info.push(line(tr!("preview-pixels", width = px.0, height = px.1)));
                    sh.body = Body::Image(id, px);
                }
                Err(e) => fail(&mut sh, e),
            },
            preview::What::Svg => {
                let r = std::fs::read(&path).map_err(|e| e.to_string()).and_then(|d| preview::svg_png(&d, 512));
                match r.and_then(|(png, px)| host::load_image(&png).map(|id| (id, px))) {
                    Ok((id, px)) => sh.body = Body::Image(id, px),
                    Err(e) => fail(&mut sh, e),
                }
            }
            preview::What::Video | preview::What::Pdf => {
                let home = fsys::home().unwrap_or_default();
                let thumb = preview::thumbnail_path(&path, &home);
                let fresh = std::fs::metadata(&thumb)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .is_some_and(|t| t.as_secs() as i64 >= mtime);
                if fresh && let Ok((id, px)) = load_image_file(&thumb) {
                    sh.body = Body::Image(id, px);
                } else {
                    let ts = thumb.display().to_string();
                    let argv: Vec<String> = if what == preview::What::Video {
                        ["ffmpegthumbnailer", "-i", &ps, "-o", &ts, "-s", "256"].map(String::from).to_vec()
                    } else {
                        let prefix = ts.trim_end_matches(".png");
                        ["pdftoppm", "-png", "-singlefile", "-f", "1", "-l", "1", "-scale-to", "256", &ps, prefix]
                            .map(String::from)
                            .to_vec()
                    };
                    if let Some(dir) = thumb.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let prog = argv[0].clone();
                    match self.spawn(argv, &path, JobKind::Thumb(thumb)) {
                        Ok(()) => sh.body = Body::Loading,
                        Err(e) => fail(&mut sh, tr!("preview-helper-error", program = prog, error = e)),
                    }
                }
            }
            preview::What::Archive => {
                let tar = self.spawn(vec!["bsdtar".into(), "-tf".into(), ps.clone()], &path, JobKind::Archive);
                let r = tar.or_else(|e| {
                    if name.to_ascii_lowercase().ends_with(".zip") {
                        self.spawn(vec!["unzip".into(), "-Z1".into(), ps.clone()], &path, JobKind::Archive)
                    } else {
                        Err(e)
                    }
                });
                match r {
                    Ok(()) => sh.body = Body::Loading,
                    Err(e) => fail(&mut sh, tr!("preview-listing-error", error = e)),
                }
            }
            preview::What::Binary => {
                if self.spawn(vec!["file".into(), "-b".into(), "--".into(), ps.clone()], &path, JobKind::File).is_ok() {
                    sh.body = Body::Loading;
                }
            }
        }
        self.preview = Some(sh);
        self.redraw_preview();
    }

    fn spawn(&mut self, argv: Vec<String>, path: &Path, kind: JobKind) -> Result<(), String> {
        let cwd = path.parent().map(|p| p.display().to_string());
        let pid = host::spawn_process(&Command { argv, cwd, env: vec![] })?;
        host::process_close_stdin(pid);
        self.job = Some(Job { pid, path: path.to_path_buf(), kind, out: Vec::new(), err: Vec::new() });
        Ok(())
    }

    fn job_output(&mut self, pid: ProcessId, stderr: bool, data: &[u8]) {
        let Some(j) = self.job.as_mut().filter(|j| j.pid == pid) else { return };
        let buf = if stderr { &mut j.err } else { &mut j.out };
        buf.extend_from_slice(&data[..data.len().min(JOB_OUTPUT.saturating_sub(buf.len()))]);
        if j.out.len() >= JOB_OUTPUT {
            // Enough for a listing.
            host::process_kill(pid);
            self.job_done(pid, Some(0));
        }
    }

    fn job_done(&mut self, pid: ProcessId, code: Option<i32>) {
        let Some(j) = self.job.take_if(|j| j.pid == pid) else { return };
        let Some(sh) = self.preview.as_mut().filter(|s| s.path == j.path) else { return };
        let c = colors(self.theme);
        let dim = style(Some(c.dim), None, Attrs::empty());
        let first_err =
            || String::from_utf8_lossy(&j.err).lines().next().map(str::to_owned).unwrap_or_else(|| tr!("failed"));
        sh.body = Body::Lines(Vec::new());
        match j.kind {
            JobKind::Archive if j.out.is_empty() => sh.info.push(vec![span(fit(&first_err(), 200), dim)]),
            JobKind::Archive => {
                let (names, total) = preview::archive_lines(&j.out, 300);
                let mut l = vec![vec![span(tr!("archive-entries", count = total), dim)]];
                l.extend(names.into_iter().map(|n| vec![span(n, Style::PLAIN)]));
                sh.body = Body::Lines(l);
            }
            JobKind::File => {
                let t = String::from_utf8_lossy(&j.out).trim().to_string();
                if !t.is_empty() {
                    sh.info.push(vec![span(t, dim)]);
                }
            }
            JobKind::Thumb(thumb) => match code {
                Some(0) => match load_image_file(&thumb) {
                    Ok((id, px)) => sh.body = Body::Image(id, px),
                    Err(e) => sh.info.push(vec![span(e, dim)]),
                },
                _ => sh.info.push(vec![span(fit(&tr!("preview-failed", error = first_err()), 200), dim)]),
            },
        }
        self.redraw_preview();
    }

    /// `p` in the sidebar: a floating preview of the cursor.
    fn toggle_float(&mut self) {
        if let Some(f) = self.float.take() {
            host::close_surface(f);
            return;
        }
        let opts = SurfaceOptions {
            kind: SurfaceKind::Float,
            title: tr!("title-preview"),
            cols: 84,
            rows: 30,
            focus: false,
            pane: None,
        };
        match host::open_surface(&opts) {
            Ok(f) => {
                self.float = Some(f);
                self.float_size = host::surface_size(f).unwrap_or((84, 30));
                self.draw_float();
            }
            Err(e) => host::notify(&tr!("plugin-error", error = e), true),
        }
    }

    fn draw_float(&mut self) {
        let (Some(f), Some(sid)) = (self.float, self.sidebar) else { return };
        let c = colors(self.theme);
        let Some(ui) = self.uis.get_mut(&sid) else { return };
        let (cols, rows) = (self.float_size.0 as usize, self.float_size.1 as usize);
        let rendered = preview_component(&mut ui.view, self.preview.as_ref(), None, c).render(cols, rows);
        let _ = host::draw(f, &rendered.frame);
    }
}

/// A PNG/JPEG/GIF/WebP file for frames, and its pixel size.
fn load_image_file(path: &Path) -> Result<(ImageId, (u32, u32)), String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let px = preview::image_size(&data).unwrap_or((256, 256));
    Ok((host::load_image(&data)?, px))
}

// --- operations ---------------------------------------------------------------

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

/// `$EDITOR` (else `vi`) on `file`, through the user's shell.
fn editor_argv(file: &Path) -> Vec<String> {
    let script = "exec ${VISUAL:-${EDITOR:-vi}} \"$1\"";
    vec!["sh".into(), "-c".into(), script.into(), "sh".into(), file.display().to_string()]
}

impl State {
    fn targets(&mut self, id: SurfaceId) -> Vec<PathBuf> {
        let Some(ui) = self.ui(id) else { return Vec::new() };
        ui.view.visual = None;
        ui.view.targets()
    }

    /// After a change: read every view again.
    fn changed(&mut self) {
        self.git.forget();
        for ui in self.uis.values_mut() {
            ui.view.reload();
        }
        self.draw_all();
    }

    fn yank(&mut self, id: SurfaceId, kind: OpKind) {
        let t = self.targets(id);
        if t.is_empty() {
            return;
        }
        self.say(id, tr!(if kind == OpKind::Copy { "yank-copy" } else { "yank-move" }, count = t.len()), false);
        self.clip = Some((kind, t));
        if let Some(ui) = self.ui(id) {
            ui.view.selected.clear();
        }
    }

    /// The directory new things go into: the open tree node under the
    /// cursor, else the view's directory.
    fn here(&self, id: SurfaceId) -> Option<PathBuf> {
        let ui = self.uis.get(&id)?;
        Some(match ui.view.current() {
            Some(r) if r.entry.dir && r.open => r.path.clone(),
            Some(r) if ui.view.layout == Layout::Tree && r.depth > 0 => r.path.parent()?.to_path_buf(),
            _ => ui.view.dir.clone(),
        })
    }

    fn paste(&mut self, id: SurfaceId) {
        let Some(dir) = self.here(id) else { return };
        let Some((kind, paths)) = self.clip.clone() else {
            return self.say(id, tr!("nothing-to-paste"), true);
        };
        let pairs = paths.iter().map(|p| (p.clone(), dir.join(name_of(p)))).collect();
        if kind == OpKind::Move {
            self.clip = None;
        }
        self.start_op(id, Transfer::new(kind, pairs), false);
    }

    fn start_op(&mut self, id: SurfaceId, t: Transfer, undoing: bool) {
        if self.op.is_some() {
            return self.say(id, tr!("operation-busy"), true);
        }
        let title = match (t.kind, undoing) {
            (_, true) => tr!("task-undo"),
            (OpKind::Copy, _) => tr!("task-copy"),
            (OpKind::Move, _) => tr!("task-move"),
        };
        let task = host::task_start(&title).ok();
        self.op = Some(Running { t, task, timer: None, surface: id, conflict: None, undoing });
        self.op_tick();
    }

    fn op_tick(&mut self) {
        let Some(op) = self.op.as_mut() else { return };
        op.timer = None;
        match op.t.step(std::time::Duration::from_millis(40)) {
            Step::Working => {
                if let Some(task) = op.task {
                    let msg = tr!("progress", done = fsys::human(op.t.done), total = fsys::human(op.t.total));
                    host::task_update(task, op.t.progress(), &msg);
                }
                op.timer = Some(host::set_timer(1, false));
            }
            Step::Conflict(p) => {
                op.conflict = Some(p);
                let s = op.surface;
                let _ = host::focus_surface(s);
                self.draw(s);
            }
            Step::Done => self.op_done(),
        }
    }

    fn op_cancel(&mut self) {
        if let Some(op) = self.op.as_mut() {
            op.t.cancel();
            if let Some(t) = op.timer.take() {
                host::cancel_timer(t);
            }
        }
        self.op_done();
    }

    fn op_done(&mut self) {
        let Some(op) = self.op.take() else { return };
        if let Some(t) = op.task {
            host::task_end(t);
        }
        let n = op.t.finished.len();
        if op.t.kind == OpKind::Move && !op.undoing {
            self.undo = (op.t.undoable && n > 0).then(|| Undo::Moved(op.t.finished.clone()));
        }
        match op.t.errors.first() {
            Some(e) => self.say(
                op.surface,
                tr!(
                    if op.t.kind == OpKind::Copy { "copy-errors" } else { "move-errors" },
                    count = n,
                    failed = op.t.errors.len(),
                    error = e
                ),
                true,
            ),
            None => {
                let mut message =
                    tr!(if op.t.kind == OpKind::Copy { "copied-files" } else { "moved-files" }, count = n);
                if op.t.kind == OpKind::Move && !op.t.undoable {
                    message.push_str("; ");
                    message.push_str(&tr!("undo-unavailable"));
                }
                self.say(op.surface, message, false)
            }
        }
        self.changed();
    }

    fn ask(&mut self, id: SurfaceId, q: &str, initial: &str, ask: Ask) {
        match host::prompt(q, initial) {
            Ok(r) => {
                self.asks.insert(r, (id, ask));
            }
            Err(e) => self.say(id, e, true),
        }
    }

    fn ask_rename(&mut self, id: SurfaceId) {
        let Some(p) = self.uis.get(&id).and_then(|u| u.view.current()).map(|r| r.path.clone()) else { return };
        self.ask(id, &tr!("rename-prompt"), &name_of(&p), Ask::Rename(p));
    }

    fn ask_new(&mut self, id: SurfaceId) {
        let Some(dir) = self.here(id) else { return };
        self.ask(id, &tr!("new-prompt"), "", Ask::New(dir));
    }

    fn ask_chmod(&mut self, id: SurfaceId) {
        let t = self.targets(id);
        if !t.is_empty() {
            let q = tr!("chmod-prompt", count = t.len());
            self.ask(id, &q, "", Ask::Chmod(t));
        }
    }

    fn ask_delete(&mut self, id: SurfaceId) {
        let t = self.targets(id);
        if !t.is_empty() {
            let q = tr!("delete-prompt", count = t.len());
            self.ask(id, &q, "", Ask::Delete(t));
        }
    }

    fn answered(&mut self, id: SurfaceId, ask: Ask, answer: Option<String>) {
        let Some(a) = answer else { return };
        let a = a.trim().to_string();
        match ask {
            Ask::Zoxide if !a.is_empty() => self.zoxide_answer(id, &a),
            Ask::Rename(from) if !a.is_empty() && a != name_of(&from) => {
                if a.contains('/') {
                    return self.say(id, tr!("name-slash"), true);
                }
                let to = from.with_file_name(&a);
                if std::fs::symlink_metadata(&to).is_ok() {
                    return self.say(id, tr!("name-exists", name = a), true);
                }
                match std::fs::rename(&from, &to) {
                    Ok(()) => {
                        self.undo = Some(Undo::Renamed(vec![(from, to.clone())]));
                        self.changed();
                        if let Some(ui) = self.ui(id) {
                            ui.view.focus(&to);
                        }
                    }
                    Err(e) => self.say(id, e.to_string(), true),
                }
            }
            Ask::New(dir) if !a.is_empty() => {
                let dirwise = a.ends_with('/');
                let path = dir.join(a.trim_end_matches('/'));
                if !path.starts_with(&dir) || a.split('/').any(|c| c == "..") {
                    return self.say(id, tr!("stay-inside"), true);
                }
                let r = if dirwise {
                    std::fs::create_dir_all(&path)
                } else {
                    let parent = path.parent().map(std::fs::create_dir_all).unwrap_or(Ok(()));
                    parent.and_then(|_| std::fs::File::create_new(&path).map(|_| ()))
                };
                match r {
                    Ok(()) => {
                        self.changed();
                        if let Some(ui) = self.ui(id) {
                            ui.view.focus(&path);
                        }
                    }
                    Err(e) => self.say(id, format!("{}: {e}", path.display()), true),
                }
            }
            Ask::Chmod(paths) if !a.is_empty() => {
                if !a.chars().all(|c| c.is_ascii_alphanumeric() || "+-=,".contains(c)) {
                    return self.say(id, tr!("bad-mode", mode = a), true);
                }
                let mut argv = vec!["chmod".to_string(), a, "--".into()];
                argv.extend(paths.iter().map(|p| p.display().to_string()));
                self.run_tool(id, argv);
            }
            Ask::Delete(paths) if a == "yes" => {
                let mut failed = Vec::new();
                for p in &paths {
                    let r = match std::fs::symlink_metadata(p) {
                        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
                        Ok(_) => std::fs::remove_file(p),
                        Err(e) => Err(e),
                    };
                    if let Err(e) = r {
                        failed.push(format!("{}: {e}", name_of(p)));
                    }
                }
                self.changed();
                match failed.first() {
                    Some(e) => self.say(id, tr!("delete-errors", count = failed.len(), error = e), true),
                    None => self.say(id, tr!("deleted-files", count = paths.len()), false),
                }
            }
            Ask::Delete(_) => self.say(id, tr!("not-deleted"), false),
            Ask::Bulk(pairs) => match ops::rename_all(&pairs) {
                Ok(done) => {
                    self.say(id, tr!("renamed-files", count = done.len()), false);
                    self.undo = Some(Undo::Renamed(done));
                    self.changed();
                }
                Err(e) => self.say(id, e, true),
            },
            _ => {}
        }
    }

    /// Run `chmod`/`ln` and report.
    fn run_tool(&mut self, id: SurfaceId, argv: Vec<String>) {
        let prog = argv[0].clone();
        match host::run(&Command { argv, cwd: None, env: vec![] }, &[], 10_000) {
            Ok(o) if o.exit_code == Some(0) => self.changed(),
            Ok(o) => {
                let e = String::from_utf8_lossy(&o.stderr)
                    .lines()
                    .next()
                    .map(str::to_owned)
                    .unwrap_or_else(|| tr!("failed"));
                self.say(id, format!("{prog}: {e}"), true);
                self.changed();
            }
            Err(e) => self.say(id, format!("{prog}: {e}"), true),
        }
    }

    /// `L`: links here to the yanked files.
    fn link(&mut self, id: SurfaceId) {
        let Some(dir) = self.here(id) else { return };
        let Some((_, paths)) = self.clip.take() else { return self.say(id, tr!("nothing-to-link"), true) };
        let mut argv = vec!["ln".to_string(), "-s".into(), "--".into()];
        argv.extend(paths.iter().map(|p| p.display().to_string()));
        argv.push(format!("{}/", dir.display()));
        self.run_tool(id, argv);
    }

    fn trash(&mut self, id: SurfaceId) {
        let t = self.targets(id);
        let mut done = Vec::new();
        let mut err = None;
        for p in t {
            match host::trash(&p.display().to_string()) {
                Ok(()) => done.push(p),
                Err(e) => {
                    err.get_or_insert(e);
                }
            }
        }
        if !done.is_empty() {
            self.undo = Some(Undo::Trashed(done.clone()));
        }
        self.changed();
        match err {
            Some(e) => self.say(id, e, true),
            None => self.say(id, tr!("trashed-files", count = done.len()), false),
        }
    }

    fn undo(&mut self, id: SurfaceId) {
        let Some(u) = self.undo.take() else { return self.say(id, tr!("nothing-to-undo"), true) };
        match u {
            Undo::Renamed(pairs) => {
                let back: Vec<_> = pairs.into_iter().map(|(a, b)| (b, a)).collect();
                match ops::rename_all(&back) {
                    Ok(d) => self.say(id, tr!("undid-renames", count = d.len()), false),
                    Err(e) => self.say(id, e, true),
                }
                self.changed();
            }
            Undo::Moved(pairs) => {
                let back = pairs.into_iter().map(|(a, b)| (b, a)).collect();
                self.start_op(id, Transfer::new(OpKind::Move, back), true);
            }
            Undo::Trashed(paths) => {
                let trash = fsys::home().unwrap_or_default().join(".local/share/Trash");
                let errs: Vec<String> = paths.iter().filter_map(|p| ops::restore(&trash, p).err()).collect();
                self.changed();
                match errs.first() {
                    Some(e) => self.say(id, e.clone(), true),
                    None => self.say(id, tr!("restored-files", count = paths.len()), false),
                }
            }
        }
    }

    /// `s`/`t`/`f`: `$EDITOR` on the file (a shell for a directory).
    fn edit_in(&mut self, id: SurfaceId, at: Placement) {
        let Some(r) = self.uis.get(&id).and_then(|u| u.view.current()).cloned() else { return };
        let (argv, cwd) = if r.entry.dir {
            (Vec::new(), r.path.clone())
        } else {
            (editor_argv(&r.path), r.path.parent().map(Path::to_path_buf).unwrap_or_default())
        };
        if let Err(e) = host::open_pane(at, &Spawn { argv, cwd: Some(cwd.display().to_string()) }) {
            self.say(id, e, true);
        }
    }

    /// `b`: the names in `$EDITOR`; renamed when it exits.
    fn bulk_rename(&mut self, id: SurfaceId) {
        let t = self.targets(id);
        let Some(dir) = t.first().and_then(|p| p.parent()).map(Path::to_path_buf) else { return };
        if t.iter().any(|p| p.parent() != Some(&dir)) {
            return self.say(id, tr!("bulk-one-directory"), true);
        }
        let names: Vec<String> = t.iter().map(|p| name_of(p)).collect();
        if names.iter().any(|n| n.contains('\n')) {
            return self.say(id, tr!("bulk-newline"), true);
        }
        let cache = fsys::home().unwrap_or_default().join(".cache/proteus-files");
        let file = cache.join(format!("rename-{}.txt", std::process::id()));
        let w = std::fs::create_dir_all(&cache).and_then(|_| std::fs::write(&file, names.join("\n") + "\n"));
        if let Err(e) = w {
            return self.say(id, format!("{}: {e}", file.display()), true);
        }
        let spawn = Spawn { argv: editor_argv(&file), cwd: Some(dir.display().to_string()) };
        match host::open_pane(Placement::Float, &spawn) {
            Ok(pane) => self.bulk = Some(Bulk { pane, surface: id, file, dir, names }),
            Err(e) => self.say(id, e, true),
        }
    }

    fn bulk_edited(&mut self) {
        let Some(b) = self.bulk.take() else { return };
        let text = std::fs::read_to_string(&b.file).unwrap_or_default();
        let _ = std::fs::remove_file(&b.file);
        match ops::rename_plan(&b.dir, &b.names, &text) {
            Ok(p) if p.is_empty() => self.say(b.surface, tr!("bulk-no-change"), false),
            Ok(p) => match host::confirm(&tr!("bulk-confirm", count = p.len())) {
                Ok(r) => {
                    self.asks.insert(r, (b.surface, Ask::Bulk(p)));
                }
                Err(e) => self.say(b.surface, e, true),
            },
            Err(e) => self.say(b.surface, e, true),
        }
        self.draw(b.surface);
    }
}

struct Files;

fn register_actions() -> Result<(), String> {
    host::register_action(SIDEBAR, &tr!("action-sidebar"), Some("alt+e"))?;
    host::register_action(PANE, &tr!("action-pane"), Some("pane:E"))?;
    host::register_action(PICKER, &tr!("action-picker"), Some("alt+p"))
}

impl Guest for Files {
    fn init() -> Result<(), String> {
        i18n::set_locale(&host::locale()?)?;
        let settings = host::config::<Settings>()?.validate()?;
        STATE.with_borrow_mut(|s| s.settings = settings);
        register_actions()?;
        STATE.with_borrow_mut(|s| s.last_pane = host::focused_pane().ok().flatten());
        Ok(())
    }

    fn on_event(event: Event) {
        STATE.with_borrow_mut(|s| {
            s.event(event);
            s.sync_preview();
        })
    }

    fn on_ui(surface: SurfaceId, event: UiEvent) {
        STATE.with_borrow_mut(|s| {
            s.ui_event(surface, event);
            s.sync_preview();
        })
    }
}

impl State {
    fn event(&mut self, event: Event) {
        let s = self;
        match event {
            Event::Action(a) if a == SIDEBAR => s.open_sidebar(),
            Event::Action(a) if a == PANE => s.open_pane(),
            Event::Action(a) if a == PICKER => s.open_picker(),
            Event::ConfigChanged => match host::locale().and_then(|locale| i18n::set_locale(&locale)) {
                Ok(true) => {
                    let _ = register_actions();
                    for (&id, ui) in &mut s.uis {
                        ui.message = None;
                        let title = tr!(if ui.view.kind == Kind::Picker { "title-picker" } else { "title-files" });
                        let _ = host::set_surface_title(id, &title);
                    }
                    if let Some(id) = s.float {
                        let _ = host::set_surface_title(id, &tr!("title-preview"));
                    }
                    s.clear_preview();
                    s.pending = None;
                    s.draw_all();
                }
                Ok(false) => {}
                Err(e) => host::notify(&tr!("plugin-error", error = e), true),
            },
            Event::ThemeChanged(t) => {
                s.theme = Some(t);
                s.draw_all();
            }
            Event::SurfaceOpened((id, args)) => {
                let _ = host::set_surface_title(id, &tr!("title-files"));
                let dir = Some(args.trim()).filter(|a| !a.is_empty()).map(|a| match a.strip_prefix("~/") {
                    Some(rest) => fsys::home().unwrap_or_default().join(rest),
                    None => PathBuf::from(a),
                });
                let size = host::surface_size(id).unwrap_or((80, 24));
                let ui = Ui {
                    view: View::new(Kind::Pane, dir.or_else(fsys::home).unwrap_or_else(|| PathBuf::from("/"))),
                    size,
                    list_area: Rect::default(),
                    prefix: None,
                    input: None,
                    message: None,
                    follow: false,
                    remote: None,
                    target: None,
                    watch: None,
                    dragging: false,
                };
                s.uis.insert(id, ui);
                s.watch(id);
                s.draw(id);
            }
            Event::FocusChanged(p) => {
                if host::get_pane(p).ok().flatten().is_some_and(|i| i.process.is_some()) {
                    s.last_pane = Some(p);
                }
                s.follow();
            }
            Event::CwdChanged(_) => s.follow(),
            Event::PaneClosed(p) => {
                if s.bulk.as_ref().is_some_and(|b| b.pane == p) {
                    s.bulk_edited();
                }
                s.blocks.remove(&p);
                if s.last_pane == Some(p) {
                    s.last_pane = None;
                }
            }
            Event::BlockStateChanged((p, b)) => {
                s.blocks.insert(p, b.state);
            }
            Event::CommandFinished(_) => {
                // Commands (git add, mv, ...) change what is shown.
                s.git.forget();
                s.stale.extend(s.uis.keys().copied());
                if s.reload_timer.is_none() {
                    s.reload_timer = Some(host::set_timer(150, false));
                }
            }
            Event::FsChanged(c) => s.fs_changed(c.watch),
            Event::Timer(t) if s.reload_timer == Some(t) => s.reload_stale(),
            Event::Timer(t) if s.preview_timer == Some(t) => s.preview_now(),
            Event::ProcessOutput(o) => {
                if !s.git.output(o.id, o.stderr, &o.data) {
                    s.job_output(o.id, o.stderr, &o.data);
                }
            }
            Event::ProcessExited((pid, code)) => {
                if s.git.exited(pid, code) {
                    s.draw_all();
                } else {
                    s.job_done(pid, code);
                }
            }
            Event::Answered((r, answer)) => {
                if let Some((id, ask)) = s.asks.remove(&r) {
                    s.answered(id, ask, answer);
                    s.draw(id);
                }
            }
            Event::Timer(t) if s.op.as_ref().is_some_and(|o| o.timer == Some(t)) => s.op_tick(),
            Event::TaskCancelled(t) if s.op.as_ref().is_some_and(|o| o.task == Some(t)) => s.op_cancel(),
            _ => {}
        }
    }

    fn accessibility(&mut self, surface: SurfaceId, event: proteus_plugin::types::AccessibilityEvent) {
        use proteus_plugin::types::AccessibilityAction as A;
        if self.op.as_ref().is_some_and(|o| o.surface == surface && o.conflict.is_some()) {
            return;
        }
        let Some(ui) = self.ui(surface) else {
            return;
        };
        if event.node == "filter" {
            match event.action {
                A::SetValue => {
                    let value = event.value.unwrap_or_default();
                    if value.len() > 16384 || value.chars().any(char::is_control) {
                        return;
                    }
                    ui.input = Some(TextInput::new(&value));
                    ui.view.set_filter(&value);
                }
                A::Focus => {
                    if ui.input.is_none() {
                        ui.input = Some(TextInput::new(&ui.view.filter));
                    }
                }
                _ => {}
            }
            return;
        }
        if event.node == "files" {
            ui.input = None;
            let rows = ui.list_area.height;
            match event.action {
                A::ScrollUp => ui.view.list.scroll(-(rows.max(1) as isize), rows),
                A::ScrollDown => ui.view.list.scroll(rows.max(1) as isize, rows),
                _ => {}
            }
            return;
        }
        let Some(index) = ui.view.rows.iter().position(|r| render::file_node_id(&r.path) == event.node) else {
            return;
        };
        if matches!(event.action, A::Focus | A::Select | A::Activate) {
            ui.input = None;
            ui.view.list.selected = index;
            ui.view.update_visual();
            if event.action == A::Activate {
                self.activate(surface);
            }
        }
    }

    fn ui_event(&mut self, surface: SurfaceId, event: UiEvent) {
        let s = self;
        {
            if s.float == Some(surface) {
                match event {
                    UiEvent::Resize(size) => {
                        s.float_size = size;
                        s.draw_float();
                    }
                    UiEvent::Closed => s.float = None,
                    _ => {}
                }
                return;
            }
            if !s.uis.contains_key(&surface) {
                return;
            }
            if matches!(event, UiEvent::Key(_) | UiEvent::Mouse(_) | UiEvent::Accessibility(_)) {
                s.active = Some(surface);
            }
            match event {
                UiEvent::Key(k) => {
                    s.key(surface, &k.key, &k.text, k.mods.contains(proteus_plugin::types::Modifiers::CTRL))
                }
                UiEvent::Text(t) => {
                    let Some(ui) = s.ui(surface) else { return };
                    if let Some(input) = ui.input.as_mut() {
                        input.insert(&t);
                        let f = input.text.clone();
                        ui.view.set_filter(&f);
                    }
                }
                UiEvent::Accessibility(event) => s.accessibility(surface, event),
                UiEvent::Mouse(m) => s.mouse(surface, m),
                UiEvent::Resize(size) => {
                    if let Some(ui) = s.ui(surface) {
                        ui.size = size;
                    }
                }
                UiEvent::Closed => return s.close_ui(surface),
                UiEvent::Preedit(_) | UiEvent::Focus(_) | UiEvent::Drop(_) => return,
            }
            s.draw(surface);
        }
    }
}

proteus_plugin::export!(Files);

#[cfg(test)]
mod tests {
    #[test]
    fn dates() {
        assert_eq!(super::date(0), "1970-01-01 00:00");
        assert_eq!(super::date(1_700_000_000), "2023-11-14 22:13");
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;
    #[test]
    fn plugin_validates_its_own_settings() {
        let settings: Settings = proteus_plugin::serde_json::from_str(r#"{"position":"right","width":48}"#).unwrap();
        assert!(matches!(settings.position, Position::Right));
        assert_eq!(settings.validate().unwrap().width, 48);
        let defaults: Settings = proteus_plugin::serde_json::from_str("{}").unwrap();
        assert_eq!(defaults.width, 32);
        for bad in [r#"{"position":"top"}"#, r#"{"width":"large"}"#, r#"{"unknown":true}"#] {
            assert!(proteus_plugin::serde_json::from_str::<Settings>(bad).is_err());
        }
        assert!(Settings { width: 0, ..Default::default() }.validate().is_err());
    }
}
