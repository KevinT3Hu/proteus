//! Scroll mode (keyboard cursor, selection, copy), search, the input line
//! and the palette, as methods of [`App`].

use std::path::PathBuf;

use proteus_core::Point;
use proteus_core::event::ClipboardKind;
use proteus_core::input::{Key, Mods};
use proteus_core::search::{Match, SearchOptions, Searcher};
use proteus_core::selection::SelectionKind;

use super::rows::{self, Rows};
use super::textinput::{Edit, Input, Purpose, palette_rank};
use super::{App, copy};
use crate::layout::PaneId;
use crate::modes::{Action, Mode, Motion, Select};

/// Palette entries remembered as recently used.
const RECENT_MAX: usize = 30;

/// The Scroll-mode cursor of the focused pane.
#[derive(Debug, Clone)]
pub struct CopyState {
    pub pane: PaneId,
    /// Absolute grid point.
    pub cursor: Point,
    /// A keyboard selection is being extended by the cursor.
    pub select: Option<Select>,
}

#[derive(Debug, Clone)]
pub struct SearchState {
    /// Pane the search started in.
    pub pane: PaneId,
    pub query: String,
    pub regex: bool,
    /// Search every visible pane of the tab.
    pub tab: bool,
    /// Only command output (shell-integration blocks).
    pub output: bool,
    /// In pane order (see [`App::search_panes`]), then position.
    pub matches: Vec<(PaneId, Match)>,
    pub current: Option<usize>,
    /// Where incremental search measures "nearest" from.
    pub origin: (PaneId, Point),
    pub error: Option<String>,
}

impl SearchState {
    /// `3/17`, `no matches`, or the regex error.
    pub fn status(&self, catalog: &crate::i18n::Catalog) -> String {
        if let Some(e) = &self.error {
            return catalog.t(e, &[]);
        }
        match (self.current, self.matches.len()) {
            (_, 0) if self.query.is_empty() => String::new(),
            (_, 0) => catalog.t("ui-no-matches", &[]),
            (Some(i), n) => format!("{}/{n}", i + 1),
            (None, n) => catalog.t("ui-count-matches", &[("count", &n.to_string())]),
        }
    }

    /// `[regex] [tab] [output] ` for the options that are on.
    fn flags(&self, catalog: &crate::i18n::Catalog) -> String {
        [(self.regex, "ui-regex"), (self.tab, "ui-tab-title"), (self.output, "ui-output")]
            .iter()
            .filter(|f| f.0)
            .map(|f| format!("{} ", catalog.t(f.1.trim_end(), &[])))
            .collect()
    }
}

/// Index of the nearest match after (`forward`) or before `from`, wrapping.
/// `order` ranks panes; matches are sorted by (rank, start).
fn nearest(matches: &[(PaneId, Match)], order: &[PaneId], from: (PaneId, Point), forward: bool) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    let rank = |p: PaneId| order.iter().position(|&x| x == p).unwrap_or(usize::MAX);
    let key = |m: &(PaneId, Match)| (rank(m.0), m.1.start);
    let at = (rank(from.0), from.1);
    if forward {
        Some(matches.iter().position(|m| key(m) > at).unwrap_or(0))
    } else {
        Some(matches.iter().rposition(|m| key(m) < at).unwrap_or(matches.len() - 1))
    }
}

/// What a palette line stands for.
#[derive(Debug, Clone, PartialEq)]
enum Item {
    Action(Action),
    Tab(usize),
    Pane(PaneId),
    Layout(String),
    /// `p.palette.add` entry.
    Lua(crate::script::FnId),
    /// A plugin's action (`Some`) or managing a plugin: plugin id.
    Plugin(String, super::plugins::Pick),
}

fn recent_path() -> Option<PathBuf> {
    Some(super::workspace::state_dir()?.join("palette_recent"))
}

/// Recently used palette entries, most recent first.
pub(super) fn load_recent() -> Vec<String> {
    let Some(text) = recent_path().and_then(|p| std::fs::read_to_string(p).ok()) else {
        return Vec::new();
    };
    text.lines().filter(|l| !l.is_empty()).take(RECENT_MAX).map(String::from).collect()
}

impl App {
    /// Switch mode, entering or leaving Scroll mode's cursor as needed.
    pub(super) fn set_mode(&mut self, m: Mode) {
        let old = self.w.mode;
        if old == m {
            return;
        }
        self.w.mode = m;
        if old == Mode::Scroll {
            self.leave_scroll();
        }
        if m == Mode::Scroll {
            self.enter_scroll();
        }
        self.w.dirty = true;
    }

    fn enter_scroll(&mut self) {
        let id = self.focused_id();
        let t = &self.panes[&id].term;
        let g = t.grid();
        let at = Point::new(g.abs_line(t.cursor().y), t.cursor().x);
        let rows = Rows::of(t);
        // Scrolled back (mouse wheel): start at the bottom of what is shown.
        let cursor =
            if rows.row_of(at.line).is_some() { at } else { Point::new(rows.last_line().unwrap_or(at.line), 0) };
        self.w.copy = Some(CopyState { pane: id, cursor, select: None });
    }

    fn leave_scroll(&mut self) {
        if let Some(c) = self.w.copy.take()
            && let Some(p) = self.panes.get_mut(&c.pane)
        {
            if c.select.is_some() {
                p.term.clear_selection();
            }
            p.term.scroll_to_bottom();
        }
        // Panes a tab-wide search scrolled.
        if let Some(s) = self.w.search.take() {
            for (id, _) in s.matches {
                if let Some(p) = self.panes.get_mut(&id) {
                    p.term.scroll_to_bottom();
                }
            }
        }
        if self.w.input.as_ref().is_some_and(|i| i.purpose == Purpose::Search) {
            self.w.input = None;
        }
    }

    /// Move the cursor to `to`, scrolling it into view and extending the
    /// keyboard selection.
    fn copy_goto(&mut self, to: Point) {
        let Some(c) = self.w.copy.as_mut() else {
            return;
        };
        let Some(p) = self.panes.get_mut(&c.pane) else {
            return;
        };
        let to = copy::clamp(p.term.grid(), to);
        p.term.reveal_line(to.line);
        if c.select.is_some() {
            p.term.update_selection(to);
        }
        c.cursor = to;
        self.w.dirty = true;
    }

    pub(super) fn copy_move(&mut self, m: Motion) {
        let Some(c) = self.w.copy.as_ref() else {
            return;
        };
        let Some(p) = self.panes.get_mut(&c.pane) else {
            return;
        };
        let t = &mut p.term;
        let (top, lines) = (t.grid().viewport_line(0), t.lines());
        let blocks = (!t.is_alt_screen()).then(|| t.blocks());
        let mut to = copy::apply(t.grid(), blocks, c.cursor, m, top, lines);
        // A folded output is one row: the cursor rests on its first line.
        if let Some((_, r)) = rows::folds(t).into_iter().find(|f| f.1.contains(&to.line)) {
            let from_inside = r.contains(&c.cursor.line);
            to.line = match m {
                Motion::Down if from_inside => r.end.min(t.grid().end_line() - 1),
                Motion::Up if from_inside => r.start.saturating_sub(1).max(t.grid().first_line()),
                _ => r.start,
            };
        }
        // Paging moves the view along with the cursor, as in vi.
        if matches!(m, Motion::HalfPageUp | Motion::HalfPageDown | Motion::PageUp | Motion::PageDown) {
            t.scroll_display(c.cursor.line as isize - to.line as isize);
        }
        // Prompts go to the top of the view, like `scroll.prev_prompt`.
        if matches!(m, Motion::PrevPrompt | Motion::NextPrompt) && to != c.cursor {
            t.scroll_to_line(to.line);
        }
        self.copy_goto(to);
    }

    pub(super) fn copy_select(&mut self, kind: Select) {
        let Some(c) = self.w.copy.as_mut() else {
            return;
        };
        let Some(p) = self.panes.get_mut(&c.pane) else {
            return;
        };
        if c.select == Some(kind) {
            p.term.clear_selection();
            c.select = None;
        } else {
            // Switching kind keeps the anchor.
            let anchor = match c.select {
                Some(_) => p.term.selection().map_or(c.cursor, |s| s.anchor),
                None => c.cursor,
            };
            let k = match kind {
                Select::Char => SelectionKind::Simple,
                Select::Line => SelectionKind::Line,
                Select::Block => SelectionKind::Block,
            };
            p.term.start_selection(k, anchor);
            p.term.update_selection(c.cursor);
            c.select = Some(kind);
        }
        self.w.dirty = true;
    }

    /// Copy the selection, or the cursor's line without one.
    pub(super) fn copy_yank(&mut self) {
        let Some(c) = self.w.copy.as_ref() else {
            return;
        };
        let Some(p) = self.panes.get(&c.pane) else {
            return;
        };
        let t = &p.term;
        let text = t.selection_text().filter(|s| !s.is_empty() && c.select.is_some()).unwrap_or_else(|| {
            let l = c.cursor.line;
            t.text_range(Point::new(l, 0), Point::new(l, t.cols() - 1)).trim_end().to_string()
        });
        if !text.is_empty() {
            self.set_selection(ClipboardKind::Clipboard, text.clone());
            self.set_selection(ClipboardKind::Primary, text);
        }
    }

    pub(super) fn copy_escape(&mut self) {
        let Some(c) = self.w.copy.as_mut() else {
            return self.set_mode(Mode::Normal);
        };
        if c.select.take().is_some() {
            if let Some(p) = self.panes.get_mut(&c.pane) {
                p.term.clear_selection();
            }
            self.w.dirty = true;
        } else if self.w.search.take().is_some() {
            self.w.dirty = true;
        } else {
            self.set_mode(Mode::Normal);
        }
    }

    /// Scroll-mode cursor position in pane rows, if visible.
    pub(super) fn copy_cursor_cell(&self, id: PaneId) -> Option<(usize, usize)> {
        let c = self.w.copy.as_ref().filter(|c| c.pane == id)?;
        let t = &self.panes.get(&id)?.term;
        let y = Rows::of(t).row_of(c.cursor.line)?;
        Some((c.cursor.col.min(t.cols() - 1), y))
    }

    // --- search ----------------------------------------------------------------

    pub(super) fn search_start(&mut self) {
        if self.w.mode != Mode::Scroll {
            self.set_mode(Mode::Scroll);
        }
        let Some(c) = self.w.copy.as_ref() else {
            return;
        };
        let (pane, origin) = (c.pane, c.cursor);
        let (query, regex, tab, output) = self
            .w
            .search
            .as_ref()
            .map_or((String::new(), false, false, false), |s| (s.query.clone(), s.regex, s.tab, s.output));
        let mut input = Input::new(Purpose::Search, &query);
        input.regex = regex;
        input.tab = tab;
        input.output = output;
        self.w.input = Some(input);
        self.w.search = Some(SearchState {
            pane,
            query,
            regex,
            tab,
            output,
            matches: Vec::new(),
            current: None,
            origin: (pane, origin),
            error: None,
        });
        self.search_update();
    }

    /// Panes a search looks in, in order: the visible panes of the tab, or
    /// only the one it started in.
    fn search_panes(&self, s: &SearchState) -> Vec<PaneId> {
        if !s.tab {
            return vec![s.pane];
        }
        let view = self.view();
        view.panes.iter().map(|p| p.0).chain(view.floats.iter().map(|f| f.id)).collect()
    }

    /// Recompute matches for the current query.
    fn search_refresh(&mut self) {
        let Some(s) = self.w.search.as_ref() else {
            return;
        };
        let panes = self.search_panes(s);
        let opts = SearchOptions { regex: s.regex, ..SearchOptions::default() };
        let (mut matches, mut error) = (Vec::new(), None);
        if !s.query.is_empty() {
            match Searcher::new(&s.query, &opts) {
                Ok(re) => {
                    for id in panes {
                        let Some(p) = self.panes.get(&id) else {
                            continue;
                        };
                        let t = &p.term;
                        let found = match (s.output, t.is_alt_screen()) {
                            (false, _) => re.find_all(t.grid()),
                            (true, false) => re.find_in_ranges(t.grid(), &t.output_ranges()),
                            (true, true) => Vec::new(),
                        };
                        matches.extend(found.into_iter().map(|m| (id, m)));
                    }
                }
                Err(_) => error = Some("ui-invalid-regex".to_string()),
            }
        }
        if let Some(s) = self.w.search.as_mut() {
            s.matches = matches;
            s.error = error;
        }
    }

    /// The query was edited: search again from the origin, upwards.
    fn search_update(&mut self) {
        if let (Some(s), Some(i)) = (self.w.search.as_mut(), self.w.input.as_ref()) {
            s.query = i.text.clone();
            s.regex = i.regex;
            s.tab = i.tab;
            s.output = i.output;
        }
        self.search_refresh();
        let Some(s) = self.w.search.as_ref() else {
            return;
        };
        let order = self.search_panes(s);
        // A match starting at the origin counts.
        let (op, o) = s.origin;
        let current = nearest(&s.matches, &order, (op, Point::new(o.line, o.col + 1)), false);
        let target = current.map(|i| s.matches[i]);
        let Some(s) = self.w.search.as_mut() else { return };
        s.current = current;
        let status = format!("{}{}", s.flags(&self.cfg.i18n), s.status(&self.cfg.i18n));
        if let Some(i) = self.w.input.as_mut() {
            i.status = status;
        }
        if let Some((pane, m)) = target {
            self.unfold_at(pane, m.start.line);
            if let Some(p) = self.panes.get_mut(&pane) {
                p.term.reveal_line(m.start.line);
            }
        }
        self.w.dirty = true;
    }

    /// Put the Scroll cursor on match `m` of `pane` (focusing that pane);
    /// with `select`, the match becomes the selection.
    fn goto_match(&mut self, pane: PaneId, m: Match, select: bool) {
        self.unfold_at(pane, m.start.line);
        let Some(c) = self.w.copy.as_ref() else {
            return;
        };
        if c.select.is_some()
            && let Some(p) = self.panes.get_mut(&c.pane)
        {
            p.term.clear_selection();
        }
        if c.pane != pane {
            if let Some(p) = self.panes.get_mut(&c.pane) {
                p.term.scroll_to_bottom();
            }
            let old = self.focused_id();
            self.w.copy = Some(CopyState { pane, cursor: m.start, select: None });
            self.w.tabs[self.w.active].set_focus(pane);
            self.after_focus(old);
        } else if let Some(c) = self.w.copy.as_mut() {
            c.select = None;
        }
        self.copy_goto(m.start);
        if select && let (Some(c), Some(p)) = (self.w.copy.as_mut(), self.panes.get_mut(&pane)) {
            p.term.start_selection(SelectionKind::Simple, m.start);
            p.term.update_selection(m.end);
            c.cursor = m.end;
            c.select = Some(Select::Char);
            p.term.reveal_line(m.end.line);
        }
        self.w.dirty = true;
    }

    /// Enter in the query: the match is selected (`y` copies it).
    fn search_submit(&mut self) {
        self.w.input = None;
        match self.w.search.as_ref().and_then(|s| Some(s.matches[s.current?])) {
            Some((pane, m)) => self.goto_match(pane, m, true),
            None => self.w.search = None,
        }
        self.w.dirty = true;
    }

    /// `n` (`up`) / `N`: next match from the cursor.
    pub(super) fn search_step(&mut self, up: bool) {
        self.search_refresh();
        let Some(c) = self.w.copy.as_ref() else {
            return;
        };
        let Some(s) = self.w.search.as_ref() else {
            return;
        };
        let order = self.search_panes(s);
        let current = nearest(&s.matches, &order, (c.pane, c.cursor), !up);
        let target = current.map(|i| s.matches[i]);
        if let Some(s) = self.w.search.as_mut() {
            s.current = current;
        }
        if let Some((pane, m)) = target {
            self.goto_match(pane, m, false);
        }
        self.w.dirty = true;
    }

    pub(super) fn search_marks(&self, id: PaneId) -> Vec<super::render::Mark> {
        use super::render::{Mark, MarkKind};
        let Some(s) = self.w.search.as_ref() else {
            return Vec::new();
        };
        s.matches
            .iter()
            .enumerate()
            .filter(|(_, (p, _))| *p == id)
            .map(|(i, (_, m))| Mark {
                start: m.start,
                end: m.end,
                kind: if Some(i) == s.current { MarkKind::Current } else { MarkKind::Match },
            })
            .collect()
    }

    // --- input line --------------------------------------------------------------

    pub(super) fn open_input(&mut self, purpose: Purpose, text: &str, status: String) {
        let mut i = Input::new(purpose, text);
        i.status = status;
        self.w.input = Some(i);
        self.w.dirty = true;
    }

    pub(super) fn open_palette(&mut self) {
        self.open_input(Purpose::Palette, "", String::new());
        self.palette_update();
    }

    /// Every palette entry: actions, tabs of this window, panes of all
    /// windows, saved layouts.
    fn palette_items(&self) -> Vec<(String, Item)> {
        let mut v: Vec<(String, Item)> = Action::all()
            .into_iter()
            .map(|a| {
                let id = a.name();
                let label = self.cfg.i18n.t(&crate::i18n::action_id(&id), &[]);
                (if label == id { id } else { format!("{label} ({id})") }, Item::Action(a))
            })
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v.extend(self.sc.palette.iter().map(|(name, f)| (name.clone(), Item::Lua(*f))));
        v.extend(self.plugin_palette().into_iter().map(|(label, id, a)| (label, Item::Plugin(id, a))));
        for (i, t) in self.w.tabs.iter().enumerate() {
            let title =
                t.name.clone().unwrap_or_else(|| self.panes.get(&t.focus).map_or("", |p| p.title()).to_string());
            v.push((
                self.cfg.i18n.t("ui-tab-number-title", &[("number", &(i + 1).to_string()), ("title", &title)]),
                Item::Tab(i),
            ));
        }
        let mut ids: Vec<PaneId> =
            std::iter::once(&self.w).chain(&self.parked).flat_map(|w| w.tabs.iter().flat_map(|t| t.panes())).collect();
        ids.sort();
        for id in ids {
            if let Some(p) = self.panes.get(&id) {
                v.push((
                    self.cfg.i18n.t("ui-pane-id-title", &[("id", &id.to_string()), ("title", p.title())]),
                    Item::Pane(id),
                ));
            }
        }
        for name in Self::layout_names() {
            v.push((self.cfg.i18n.t("ui-layout-name", &[("name", &name)]), Item::Layout(name)));
        }
        v
    }

    /// Matching entries, recently used ones first.
    fn palette_hits(&self, query: &str) -> Vec<(String, Item)> {
        let items = self.palette_items();
        let keys: Vec<String> = items.iter().map(|i| i.0.to_lowercase()).collect();
        let mut hits: Vec<(String, Item)> = palette_rank(&keys, query).into_iter().map(|i| items[i].clone()).collect();
        let rank = |s: &str| self.recent.iter().position(|r| r == s).unwrap_or(usize::MAX);
        hits.sort_by_key(|h| rank(&h.0)); // stable: the rest keeps its order
        hits
    }

    /// Show the matching entries, the chosen one first in brackets with
    /// its key.
    fn palette_update(&mut self) {
        let Some(query) = self.w.input.as_ref().map(|i| i.text.clone()) else {
            return;
        };
        let hits = self.palette_hits(&query);
        let key = |item: &Item| match item {
            Item::Action(a) => self.keymap.key_path(*a),
            Item::Tab(i) if *i < 9 => self.keymap.key_path(Action::GotoTab(*i as u8)),
            _ => None,
        };
        let chosen_key = self
            .w
            .input
            .as_ref()
            .and_then(|i| hits.get(i.choice.min(hits.len().saturating_sub(1))))
            .and_then(|h| key(&h.1));
        let Some(i) = self.w.input.as_mut() else {
            return;
        };
        if hits.is_empty() {
            i.choice = 0;
            i.status = self.cfg.i18n.t("ui-no-match", &[]);
            return;
        }
        i.choice = i.choice.min(hits.len() - 1);
        let mut shown: Vec<String> = hits.iter().skip(i.choice).take(8).map(|s| s.0.clone()).collect();
        shown[0] = match chosen_key {
            Some(k) => format!("[{} · {k}]", shown[0]),
            None => format!("[{}]", shown[0]),
        };
        i.status = format!("{}  ({}/{})", shown.join("  "), i.choice + 1, hits.len());
    }

    fn remember(&mut self, entry: String) {
        self.recent.retain(|r| *r != entry);
        self.recent.insert(0, entry);
        self.recent.truncate(RECENT_MAX);
        if let Some(path) = recent_path() {
            let _ = path.parent().map(std::fs::create_dir_all);
            let _ = std::fs::write(path, self.recent.join("\n") + "\n");
        }
    }

    fn palette_run(&mut self) {
        let Some((text, choice)) = self.w.input.as_ref().map(|i| (i.text.clone(), i.choice)) else {
            return;
        };
        let hit = self.palette_hits(&text).into_iter().nth(choice);
        self.w.input = None;
        let Some((label, item)) = hit else { return };
        match item {
            Item::Action(a) => {
                self.remember(label);
                self.dispatch(a);
            }
            Item::Tab(i) => self.dispatch(Action::GotoTab(i as u8)),
            Item::Pane(id) => self.reveal_pane(id, None),
            Item::Layout(name) => {
                self.remember(label);
                self.load_layout(&name);
            }
            Item::Lua(f) => {
                self.remember(label);
                self.queue_lua(f);
            }
            Item::Plugin(id, a) => {
                self.remember(label.clone());
                self.plugin_palette_run(&label, &id, a);
            }
        }
    }

    /// A key while the input line is open.
    pub(super) fn input_key(&mut self, key: Key, mods: Mods, text: &str) {
        let Some(input) = self.w.input.as_mut() else {
            return;
        };
        let purpose = input.purpose;
        match input.key(key, mods, text) {
            Edit::Changed if purpose == Purpose::Search => self.search_update(),
            Edit::Changed if purpose == Purpose::Palette => {
                if let Some(i) = self.w.input.as_mut() {
                    i.choice = 0;
                }
                self.palette_update();
            }
            Edit::Prev | Edit::Next if purpose == Purpose::Palette => {
                if let Some(i) = self.w.input.as_mut() {
                    i.choice = if key == Key::Up || mods.contains(Mods::SHIFT) {
                        i.choice.saturating_sub(1)
                    } else {
                        i.choice + 1
                    };
                }
                self.palette_update();
            }
            Edit::Changed | Edit::Moved | Edit::Prev | Edit::Next => {}
            Edit::Cancel => {
                self.w.input = None;
                if let Purpose::PluginPrompt(n) = purpose {
                    self.plugin_answer(n, None);
                }
                if purpose == Purpose::Search {
                    self.w.search = None;
                }
            }
            Edit::Submit => {
                let text = self.w.input.as_ref().map(|i| i.text.trim().to_string()).unwrap_or_default();
                if purpose == Purpose::Search {
                    return self.search_submit();
                }
                if purpose == Purpose::Palette {
                    return self.palette_run();
                }
                self.w.input = None;
                self.input_submit(purpose, text);
            }
        }
        self.w.dirty = true;
    }

    /// Text typed by the input method goes to the input line.
    pub(super) fn input_text(&mut self, text: &str) {
        let Some(input) = self.w.input.as_mut() else {
            return;
        };
        input.insert(text);
        match input.purpose {
            Purpose::Search => self.search_update(),
            Purpose::Palette => self.palette_update(),
            _ => {}
        }
        self.w.dirty = true;
    }

    fn input_submit(&mut self, purpose: Purpose, text: String) {
        let name = (!text.is_empty()).then_some(text.clone());
        match purpose {
            Purpose::Search => {}
            Purpose::RenameTab(pane) => {
                if let Some(t) = self.w.tabs.iter_mut().find(|t| t.contains(pane)) {
                    t.name = name;
                }
            }
            Purpose::RenamePane(pane) => {
                if let Some(p) = self.panes.get_mut(&pane) {
                    p.name = name;
                }
                self.update_title();
            }
            Purpose::ExportScrollback(pane) => {
                if text.is_empty() {
                    return;
                }
                let Some(p) = self.panes.get(&pane) else {
                    return;
                };
                let output = p.term.scrollback_text();
                let sender = self.sender.clone();
                std::thread::spawn(move || {
                    use std::io::Write;
                    let result = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&text)
                        .and_then(|mut file| {
                            file.write_all(output.as_bytes())?;
                            file.sync_all()
                        })
                        .map_err(|e| format!("{text}: {e}"));
                    let _ = sender.send(super::Msg::Exported(result));
                });
            }
            Purpose::SaveLayout if !text.is_empty() => self.save_layout(&text),
            Purpose::LoadLayout if !text.is_empty() => self.load_layout(&text),
            Purpose::PluginPrompt(n) => self.plugin_answer(n, Some(text)),
            Purpose::SaveLayout | Purpose::LoadLayout | Purpose::Palette => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_across_panes() {
        let m = |l, c| Match { start: Point::new(l, c), end: Point::new(l, c + 1) };
        let ms = vec![(1, m(5, 0)), (1, m(9, 0)), (2, m(1, 0)), (2, m(7, 0))];
        let order = [1, 2];
        assert_eq!(nearest(&ms, &order, (1, Point::new(9, 0)), true), Some(2));
        assert_eq!(nearest(&ms, &order, (2, Point::new(1, 0)), false), Some(1));
        assert_eq!(nearest(&ms, &order, (2, Point::new(9, 0)), true), Some(0)); // wraps
        assert_eq!(nearest(&ms, &order, (1, Point::new(0, 0)), false), Some(3));
        assert_eq!(nearest(&[], &order, (1, Point::new(0, 0)), false), None);
    }
}
