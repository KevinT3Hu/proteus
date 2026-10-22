//! `proteus.files`: the first-party file manager (docs/design/03-file-manager.md).
//!
//! Three surfaces: a sidebar (`Alt+e`, a tree that follows the focused
//! pane's directory), a pane (`E` in Pane mode or a layout, Miller
//! columns) and a picker (`Alt+p`, a float; Enter inserts the path into the
//! pane's prompt). Everything goes through the public plugin API.

mod fsys;
mod git;
mod view;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use proteus_plugin::types::{
    Attrs, BlockState, Color, Command, Event, Frame, Line, MouseAction, MouseButton, PaneId, RequestId, Span, Style,
    SurfaceId, SurfaceKind, SurfaceOptions, Theme, TimerId, UiEvent, WatchId,
};
use proteus_plugin::{Edit, Guest, TextInput, fit, host, pad, width};

use fsys::{Sort, SortKey};
use view::{Kind, Layout, View};

const SIDEBAR: &str = "Files: sidebar";
const PANE: &str = "Files: pane";
const PICKER: &str = "Files: pick a path";
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

#[derive(Default)]
struct State {
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
    asks: HashMap<RequestId, SurfaceId>,
}

/// Git status, cached per repository.
#[derive(Default)]
struct Git {
    off: bool,
    roots: HashMap<PathBuf, Option<PathBuf>>,
    status: HashMap<PathBuf, git::Status>,
}

impl Git {
    fn run(&mut self, dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
        if self.off {
            return None;
        }
        let mut argv = vec!["git".to_string()];
        argv.extend(args.iter().map(|a| a.to_string()));
        let cmd = Command {
            argv,
            cwd: Some(dir.display().to_string()),
            env: vec![("GIT_OPTIONAL_LOCKS".into(), "0".into())],
        };
        match host::run(&cmd, &[], 3000) {
            Ok(o) if o.exit_code == Some(0) => Some(o.stdout),
            Ok(_) => None,
            Err(e) => {
                // Not installed or not allowed: stop asking.
                host::log(&format!("git: {e}"));
                self.off = true;
                None
            }
        }
    }

    fn status(&mut self, dir: &Path) -> Option<&git::Status> {
        if !self.roots.contains_key(dir) {
            let root = self
                .run(dir, &["rev-parse", "--show-toplevel"])
                .map(|o| PathBuf::from(String::from_utf8_lossy(&o).trim_end_matches(['\n', '\r']).to_string()));
            self.roots.insert(dir.to_path_buf(), root);
        }
        let root = self.roots.get(dir)?.clone()?;
        if !self.status.contains_key(&root) {
            let out = self.run(&root, &["status", "--porcelain=v1", "-z", "--ignored", "--untracked-files=normal"])?;
            self.status.insert(root.clone(), git::Status::parse(&root, &out));
        }
        self.status.get(&root)
    }

    fn forget(&mut self) {
        self.status.clear();
    }
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

// --- colors -------------------------------------------------------------------

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

// --- drawing ------------------------------------------------------------------

/// How a row is drawn.
#[derive(Clone, Copy)]
struct RowLook {
    cursor: bool,
    marked: bool,
    /// The miller parent column's current directory.
    here: bool,
    git: Option<git::Mark>,
    tree: bool,
    size: bool,
}

fn row_line(r: &view::Row, look: RowLook, cols: usize, c: &Colors) -> Line {
    let e = &r.entry;
    let mut st = if e.dir {
        style(Some(c.accent), None, Attrs::BOLD)
    } else if e.link {
        style(Some(Color::Indexed(6)), None, Attrs::empty())
    } else {
        Style::PLAIN
    };
    if e.hidden() {
        st.attrs |= Attrs::DIM;
    }
    let fill = if look.cursor {
        st = style(Some(c.bg), Some(c.accent), st.attrs & !Attrs::DIM);
        st
    } else if look.here {
        st.bg = Some(c.bar);
        st
    } else {
        Style::PLAIN
    };
    let mut line = Vec::new();
    let mark = if look.marked { "●" } else { " " };
    line.push(span(mark, style(Some(c.accent), fill.bg, Attrs::BOLD)));
    let (gc, gcol) = match look.git {
        Some(git::Mark::Modified) => ('M', Color::Indexed(3)),
        Some(git::Mark::Staged) => ('+', Color::Indexed(2)),
        Some(git::Mark::Untracked) => ('?', Color::Indexed(1)),
        Some(git::Mark::Ignored) => ('!', c.dim),
        None => (' ', c.dim),
    };
    let gst = if look.cursor { fill } else { style(Some(gcol), fill.bg, Attrs::empty()) };
    line.push(span(gc.to_string(), gst));
    let mut name = String::from(" ");
    if look.tree {
        name.push_str(&"  ".repeat(r.depth));
        name.push_str(if !e.dir {
            "  "
        } else if r.open {
            "▾ "
        } else {
            "▸ "
        });
    }
    name.push_str(&e.name);
    if e.dir {
        name.push('/');
    }
    if e.link {
        name.push('@');
    }
    let size = if look.size && !e.dir { format!(" {} ", fsys::human(e.size)) } else { " ".into() };
    let room = cols.saturating_sub(2 + width(&size));
    let name = fit(&name, room);
    let gap = room.saturating_sub(width(&name));
    line.push(span(name, st));
    line.push(span(" ".repeat(gap), fill));
    if cols > 2 + width(&size) {
        let mut sz = fill;
        if !look.cursor {
            sz.fg = Some(c.dim);
        }
        line.push(span(size, sz));
    }
    line
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

impl State {
    fn draw(&mut self, id: SurfaceId) {
        let c = colors(self.theme);
        let Some(ui) = self.uis.get_mut(&id) else { return };
        let (cols, rows) = (ui.size.0 as usize, ui.size.1 as usize);
        if cols == 0 || rows == 0 {
            return;
        }
        let home = fsys::home();
        let mut lines: Vec<Line> = Vec::new();
        let mut cursor = None;

        // Header: the directory and the sidebar's follow state.
        let flag = match (ui.view.kind, ui.follow) {
            _ if ui.remote.is_some() => " remote",
            (Kind::Sidebar, true) => "",
            (Kind::Sidebar, false) => " pinned",
            _ => "",
        };
        let path = fit(&fsys::tilde(&ui.view.dir, home.as_deref()), cols.saturating_sub(width(flag)));
        let gap = cols.saturating_sub(width(&path) + width(flag));
        lines.push(vec![
            span(path, style(Some(c.accent), None, Attrs::BOLD)),
            span(" ".repeat(gap), Style::PLAIN),
            span(flag, style(Some(c.dim), None, Attrs::empty())),
        ]);

        let body = rows.saturating_sub(2);
        if let Some(host) = &ui.remote {
            lines.push(vec![span(
                fit(&format!("{host}: remote: not available in v1"), cols),
                style(Some(c.dim), None, Attrs::ITALIC),
            )]);
        } else if let Some(e) = ui.view.error() {
            lines.push(vec![span(fit(e, cols), style(Some(Color::Indexed(1)), None, Attrs::empty()))]);
        } else {
            let status = self.git.status(&ui.view.dir).cloned();
            let mark = |p: &Path| status.as_ref().and_then(|s| s.mark(p));
            let miller = ui.view.layout == Layout::Miller && cols >= 30;
            let tree = ui.view.layout == Layout::Tree;
            // Column widths: parent | current | preview.
            let (pw, cw) = if !miller {
                (0, cols)
            } else if cols >= 60 {
                (cols / 5, cols * 2 / 5)
            } else {
                (0, cols / 2)
            };
            let vw = cols.saturating_sub(pw + cw + if pw > 0 { 2 } else { 1 });
            let size = !miller && ui.view.kind != Kind::Sidebar || cw >= 40;
            let shown: Vec<(usize, bool)> = ui.view.list.visible(body).collect();
            let mut current: Vec<Line> = shown
                .iter()
                .map(|&(i, sel)| {
                    let r = &ui.view.rows[i];
                    let look = RowLook {
                        cursor: sel,
                        marked: ui.view.selected.contains(&r.path),
                        here: false,
                        git: mark(&r.path),
                        tree,
                        size,
                    };
                    row_line(r, look, cw, &c)
                })
                .collect();
            if ui.view.rows.is_empty() {
                let what = if ui.view.filter.is_empty() { "empty" } else { "no match" };
                current.push(vec![span(what, style(Some(c.dim), None, Attrs::ITALIC))]);
            }
            if miller {
                let parent = if pw > 0 { column_parent(&mut ui.view, pw, body, &mark, &c) } else { Vec::new() };
                let preview = column_preview(&mut ui.view, vw, body, &mark, &c);
                let sep = span("│", style(Some(c.dim), None, Attrs::empty()));
                for i in 0..body {
                    let mut l = Vec::new();
                    if pw > 0 {
                        l.extend(pad(parent.get(i).cloned().unwrap_or_default(), pw, Style::PLAIN));
                        l.push(sep.clone());
                    }
                    l.extend(pad(current.get(i).cloned().unwrap_or_default(), cw, Style::PLAIN));
                    l.push(sep.clone());
                    l.extend(preview.get(i).cloned().unwrap_or_default());
                    lines.push(l);
                }
            } else {
                lines.extend(current);
            }
        }
        lines.resize(rows - 1, Vec::new());

        // Footer: the filter being typed, a message, or the state.
        let dim = style(Some(c.dim), None, Attrs::empty());
        let footer = if let Some(input) = &ui.input {
            let (field, col) = input.render(cols.saturating_sub(1), Style::PLAIN);
            cursor = Some((col + 1, rows as u32 - 1));
            let mut l = vec![span("/", style(Some(c.accent), None, Attrs::BOLD))];
            l.extend(field);
            l
        } else if let Some((m, err)) = &ui.message {
            let st = if *err {
                style(Some(Color::Indexed(1)), None, Attrs::empty())
            } else {
                style(Some(c.fg), None, Attrs::empty())
            };
            vec![span(fit(m, cols), st)]
        } else {
            let v = &ui.view;
            let pos =
                if v.rows.is_empty() { "0/0".into() } else { format!("{}/{}", v.list.selected + 1, v.rows.len()) };
            let mut s = format!("{pos}  {}{}", v.sort.key.label(), if v.sort.reverse { "↓" } else { "" });
            if v.hidden {
                s.push_str("  .*");
            }
            if !v.filter.is_empty() {
                s.push_str(&format!("  /{}", v.filter));
            }
            if !v.selected.is_empty() {
                s.push_str(&format!("  {} marked", v.selected.len()));
            }
            if let Some(p) = ui.prefix {
                s.push_str(match p {
                    Prefix::G => "  g-",
                    Prefix::Mark => "  mark-",
                    Prefix::Jump => "  jump-",
                    Prefix::Sort => "  sort: n m s e d(irs) r(everse)",
                });
            }
            if ui.view.kind == Kind::Picker && cols > 40 {
                s.push_str("  ⏎ insert");
            }
            vec![span(fit(&s, cols), dim)]
        };
        lines.push(footer);
        let _ = host::draw(id, &Frame { lines, cursor, background: None, images: vec![] });
    }

    fn draw_all(&mut self) {
        let ids: Vec<SurfaceId> = self.uis.keys().copied().collect();
        for id in ids {
            self.draw(id);
        }
    }
}

type MarkOf<'a> = dyn Fn(&Path) -> Option<git::Mark> + 'a;

/// Miller: the parent directory, with the current one highlighted.
fn column_parent(v: &mut View, cols: usize, rows: usize, mark: &MarkOf, c: &Colors) -> Vec<Line> {
    let Some(parent) = v.dir.parent().map(Path::to_path_buf) else { return Vec::new() };
    let hidden = v.hidden;
    let entries: Vec<_> = match v.listing(&parent) {
        Ok(e) => e.iter().filter(|e| hidden || !e.hidden()).cloned().collect(),
        Err(_) => return Vec::new(),
    };
    let here = entries.iter().position(|e| parent.join(&e.name) == v.dir).unwrap_or(0);
    let start = here.saturating_sub(rows / 2).min(entries.len().saturating_sub(rows));
    entries
        .into_iter()
        .enumerate()
        .skip(start)
        .take(rows)
        .map(|(i, e)| {
            let path = parent.join(&e.name);
            let r = view::Row { path, depth: 0, entry: e, open: false };
            let look =
                RowLook { cursor: false, marked: false, here: i == here, git: mark(&r.path), tree: false, size: false };
            row_line(&r, look, cols, c)
        })
        .collect()
}

/// Miller: the directory under the cursor, or facts about the file.
fn column_preview(v: &mut View, cols: usize, rows: usize, mark: &MarkOf, c: &Colors) -> Vec<Line> {
    let Some(r) = v.current().cloned() else { return Vec::new() };
    let dim = style(Some(c.dim), None, Attrs::empty());
    if r.entry.dir {
        let hidden = v.hidden;
        return match v.listing(&r.path) {
            Ok(e) if e.is_empty() => vec![vec![span("empty", style(Some(c.dim), None, Attrs::ITALIC))]],
            Ok(e) => e
                .iter()
                .filter(|e| hidden || !e.hidden())
                .take(rows)
                .map(|e| {
                    let row = view::Row { path: r.path.join(&e.name), depth: 0, entry: e.clone(), open: false };
                    let look = RowLook {
                        cursor: false,
                        marked: false,
                        here: false,
                        git: mark(&row.path),
                        tree: false,
                        size: false,
                    };
                    row_line(&row, look, cols, c)
                })
                .collect(),
            Err(e) => vec![vec![span(fit(e, cols), style(Some(Color::Indexed(1)), None, Attrs::empty()))]],
        };
    }
    let e = &r.entry;
    let mut out = vec![vec![span(fit(&e.name, cols), style(None, None, Attrs::BOLD))]];
    out.push(vec![span(fit(&format!("{} ({} bytes)", fsys::human(e.size), e.size), cols), dim)]);
    out.push(vec![span(fit(&format!("modified {}", date(e.mtime)), cols), dim)]);
    if let Ok(m) = host::mime_type(&r.path.display().to_string()) {
        out.push(vec![span(fit(&m, cols), dim)]);
    }
    out
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
                host::notify(&format!("files: {e}"), true);
                return None;
            }
        };
        let remote = focused.as_ref().and_then(|p| host::pane_remote(p.id).ok().flatten());
        let start = dir
            .or_else(|| focused.as_ref().and_then(|p| p.cwd.clone()).map(PathBuf::from))
            .or_else(fsys::home)
            .unwrap_or_else(|| PathBuf::from("/"));
        let size = host::surface_size(id).unwrap_or((opts.cols.max(20), opts.rows.max(10)));
        let ui = Ui {
            view: View::new(kind, start),
            size,
            prefix: None,
            input: None,
            message: None,
            follow: kind == Kind::Sidebar,
            remote: remote.filter(|_| kind == Kind::Sidebar),
            target,
            watch: None,
            dragging: false,
        };
        self.uis.insert(id, ui);
        self.watch(id);
        self.draw(id);
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
        }
        if self.picker == Some(id) {
            self.picker = None;
        }
        self.stale.remove(&id);
    }

    fn toggle_sidebar(&mut self) {
        if let Some(id) = self.sidebar {
            host::close_surface(id);
            return self.close_ui(id);
        }
        let opts = SurfaceOptions {
            kind: SurfaceKind::SidebarLeft,
            title: "Files".into(),
            cols: SIDEBAR_COLS,
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
            title: "Pick a path".into(),
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
            title: "Files".into(),
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
        let Some(pane) = self.target_pane(id) else { return self.say(id, "no pane to insert into", true) };
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
                self.say(
                    id,
                    format!("inserted {} path{}", paths.len(), if paths.len() == 1 { "" } else { "s" }),
                    false,
                );
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
        let Some(pane) = self.target_pane(id) else { return self.say(id, "no pane", true) };
        if self.blocks.get(&pane) != Some(&BlockState::Prompt) {
            return self.say(id, "the pane is not at a prompt", true);
        }
        let cmd = format!("cd -- {}\r", fsys::shell_quote(&dir.display().to_string()));
        match host::send_text(pane, &cmd, false) {
            Ok(()) => self.say(id, format!("cd {}", fsys::tilde(&dir, fsys::home().as_deref())), false),
            Err(e) => self.say(id, e, true),
        }
    }

    fn copy_paths(&mut self, id: SurfaceId) {
        let Some(paths) = self.uis.get(&id).map(|u| u.view.targets()).filter(|p| !p.is_empty()) else { return };
        let text: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        match host::clipboard_write(&text.join("\n")) {
            Ok(()) => self.say(id, format!("copied {}", fit(&text.join(" "), 60)), false),
            Err(e) => self.say(id, e, true),
        }
    }

    fn bookmark(&mut self, id: SurfaceId, key: &str) {
        let Some(dir) = self.uis.get(&id).map(|u| u.view.dir.clone()) else { return };
        match host::storage_set(&format!("mark:{key}"), &dir.display().to_string()) {
            Ok(()) => self.say(id, format!("'{key} → {}", fsys::tilde(&dir, fsys::home().as_deref())), false),
            Err(e) => self.say(id, e, true),
        }
    }

    fn jump(&mut self, id: SurfaceId, key: &str) {
        match host::storage_get(&format!("mark:{key}")).map(PathBuf::from) {
            Some(d) if d.is_dir() => self.go(id, d),
            Some(d) => self.say(id, format!("{}: gone", d.display()), true),
            None => self.say(id, format!("no mark '{key}"), true),
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
        match host::prompt("zoxide:", "") {
            Ok(r) => {
                self.asks.insert(r, id);
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
            Ok(_) => self.say(id, format!("zoxide: no match for {query}"), true),
            Err(e) => self.say(id, format!("zoxide: {e}"), true),
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
        let rows = self.uis.get(&id).map_or(1, |u| u.size.1.saturating_sub(2) as usize);
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
                (Prefix::G, "g") => ui.view.list.selected = 0,
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
                self.say(id, format!("view: {label}"), false);
            }
            (_, " ") => ui.view.toggle_selected(),
            (_, "*") => ui.view.invert_selected(),
            (_, "c") => self.cd_pane(id),
            (_, "i") => self.insert(id),
            (_, "y") => self.copy_paths(id),
            (_, "F") if ui.view.kind == Kind::Sidebar => {
                ui.follow = !ui.follow;
                let f = ui.follow;
                self.say(id, if f { "following the focused pane" } else { "pinned" }, false);
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
                if !ui.view.filter.is_empty() {
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
                ui.view.list.key(k, t, rows);
            }
        }
    }

    fn mouse(&mut self, id: SurfaceId, m: proteus_plugin::types::MouseEvent) {
        let Some(ui) = self.ui(id) else { return };
        let body = ui.size.1.saturating_sub(2) as usize;
        match (m.button, m.action) {
            (MouseButton::WheelUp, _) => ui.view.list.offset = ui.view.list.offset.saturating_sub(3),
            (MouseButton::WheelDown, _) => {
                ui.view.list.offset = (ui.view.list.offset + 3).min(ui.view.list.len.saturating_sub(body));
                // Keep the cursor in view so `visible` does not scroll back.
                let o = ui.view.list.offset;
                ui.view.list.selected = ui.view.list.selected.max(o);
            }
            (MouseButton::Left, MouseAction::Press) => {
                ui.dragging = false;
                let row = m.row as usize;
                if row == 0 || row > body {
                    return;
                }
                let Some(i) = ui.view.list.at_row(row - 1) else { return };
                if ui.view.list.selected == i {
                    self.activate(id);
                } else {
                    ui.view.list.selected = i;
                }
            }
            (MouseButton::Left, MouseAction::Drag) if !ui.dragging => {
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

struct Files;

impl Guest for Files {
    fn init() -> Result<(), String> {
        host::register_action(SIDEBAR, Some("alt+e"))?;
        host::register_action(PANE, Some("pane:E"))?;
        host::register_action(PICKER, Some("alt+p"))?;
        STATE.with_borrow_mut(|s| s.last_pane = host::focused_pane().ok().flatten());
        Ok(())
    }

    fn on_event(event: Event) {
        STATE.with_borrow_mut(|s| match event {
            Event::Action(a) if a == SIDEBAR => s.toggle_sidebar(),
            Event::Action(a) if a == PANE => s.open_pane(),
            Event::Action(a) if a == PICKER => s.open_picker(),
            Event::ThemeChanged(t) => {
                s.theme = Some(t);
                s.draw_all();
            }
            Event::SurfaceOpened((id, args)) => {
                let dir = Some(args.trim()).filter(|a| !a.is_empty()).map(|a| match a.strip_prefix("~/") {
                    Some(rest) => fsys::home().unwrap_or_default().join(rest),
                    None => PathBuf::from(a),
                });
                let size = host::surface_size(id).unwrap_or((80, 24));
                let ui = Ui {
                    view: View::new(Kind::Pane, dir.or_else(fsys::home).unwrap_or_else(|| PathBuf::from("/"))),
                    size,
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
            Event::Answered((r, answer)) => {
                if let Some(id) = s.asks.remove(&r) {
                    if let Some(q) = answer.filter(|q| !q.trim().is_empty()) {
                        s.zoxide_answer(id, q.trim());
                    }
                    s.draw(id);
                }
            }
            _ => {}
        })
    }

    fn on_ui(surface: SurfaceId, event: UiEvent) {
        STATE.with_borrow_mut(|s| {
            if !s.uis.contains_key(&surface) {
                return;
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
        })
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
