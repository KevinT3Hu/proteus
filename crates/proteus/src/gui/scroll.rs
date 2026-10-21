//! Scroll mode (keyboard cursor, selection, copy), search and the input
//! line, as methods of [`App`].

use proteus_core::Point;
use proteus_core::event::ClipboardKind;
use proteus_core::input::{Key, Mods};
use proteus_core::search::{self, Match, SearchOptions, Searcher};
use proteus_core::selection::SelectionKind;

use super::textinput::{Edit, Input, Purpose};
use super::{App, copy};
use crate::layout::PaneId;
use crate::modes::{Mode, Motion, Select};

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
    pub pane: PaneId,
    pub query: String,
    pub regex: bool,
    pub matches: Vec<Match>,
    pub current: Option<usize>,
    /// Where incremental search measures "nearest" from.
    pub origin: Point,
    pub error: Option<String>,
}

impl SearchState {
    /// `3/17`, `no matches`, or the regex error.
    pub fn status(&self) -> String {
        if let Some(e) = &self.error {
            return e.clone();
        }
        match (self.current, self.matches.len()) {
            (_, 0) if self.query.is_empty() => String::new(),
            (_, 0) => "no matches".into(),
            (Some(i), n) => format!("{}/{n}", i + 1),
            (None, n) => format!("{n} matches"),
        }
    }
}

impl App {
    /// Switch mode, entering or leaving Scroll mode's cursor as needed.
    pub(super) fn set_mode(&mut self, m: Mode) {
        let old = self.mode;
        if old == m {
            return;
        }
        self.mode = m;
        if old == Mode::Scroll {
            self.leave_scroll();
        }
        if m == Mode::Scroll {
            self.enter_scroll();
        }
        self.dirty = true;
    }

    fn enter_scroll(&mut self) {
        let id = self.focused_id();
        let t = &self.panes[&id].term;
        let g = t.grid();
        let top = g.viewport_line(0);
        let lines = t.lines() as u64;
        let at = Point::new(g.abs_line(t.cursor().y), t.cursor().x);
        // Scrolled back (mouse wheel): start at the bottom of what is shown.
        let cursor = if at.line >= top && at.line < top + lines { at } else { Point::new(top + lines - 1, 0) };
        self.copy = Some(CopyState { pane: id, cursor, select: None });
    }

    fn leave_scroll(&mut self) {
        if let Some(c) = self.copy.take()
            && let Some(p) = self.panes.get_mut(&c.pane)
        {
            if c.select.is_some() {
                p.term.clear_selection();
            }
            p.term.scroll_to_bottom();
        }
        self.search = None;
        if self.input.as_ref().is_some_and(|i| i.purpose == Purpose::Search) {
            self.input = None;
        }
    }

    /// Move the cursor to `to`, scrolling it into view and extending the
    /// keyboard selection.
    fn copy_goto(&mut self, to: Point) {
        let Some(c) = self.copy.as_mut() else { return };
        let Some(p) = self.panes.get_mut(&c.pane) else { return };
        let to = copy::clamp(p.term.grid(), to);
        p.term.reveal_line(to.line);
        if c.select.is_some() {
            p.term.update_selection(to);
        }
        c.cursor = to;
        self.dirty = true;
    }

    pub(super) fn copy_move(&mut self, m: Motion) {
        let Some(c) = self.copy.as_ref() else { return };
        let Some(p) = self.panes.get_mut(&c.pane) else { return };
        let t = &mut p.term;
        let (top, lines) = (t.grid().viewport_line(0), t.lines());
        let blocks = (!t.is_alt_screen()).then(|| t.blocks());
        let to = copy::apply(t.grid(), blocks, c.cursor, m, top, lines);
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
        let Some(c) = self.copy.as_mut() else { return };
        let Some(p) = self.panes.get_mut(&c.pane) else { return };
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
        self.dirty = true;
    }

    /// Copy the selection, or the cursor's line without one.
    pub(super) fn copy_yank(&mut self) {
        let Some(c) = self.copy.as_ref() else { return };
        let Some(p) = self.panes.get(&c.pane) else { return };
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
        let Some(c) = self.copy.as_mut() else { return self.set_mode(Mode::Normal) };
        if c.select.take().is_some() {
            if let Some(p) = self.panes.get_mut(&c.pane) {
                p.term.clear_selection();
            }
            self.dirty = true;
        } else if self.search.take().is_some() {
            self.dirty = true;
        } else {
            self.set_mode(Mode::Normal);
        }
    }

    /// Scroll-mode cursor position in viewport coordinates, if visible.
    pub(super) fn copy_cursor_cell(&self, id: PaneId) -> Option<(usize, usize)> {
        let c = self.copy.as_ref().filter(|c| c.pane == id)?;
        let t = &self.panes.get(&id)?.term;
        let top = t.grid().viewport_line(0);
        let y = c.cursor.line.checked_sub(top)? as usize;
        (y < t.lines()).then_some((c.cursor.col.min(t.cols() - 1), y))
    }

    // --- search ----------------------------------------------------------------

    pub(super) fn search_start(&mut self) {
        if self.mode != Mode::Scroll {
            self.set_mode(Mode::Scroll);
        }
        let Some(c) = self.copy.as_ref() else { return };
        let (pane, origin) = (c.pane, c.cursor);
        let (query, regex) = self.search.as_ref().map_or((String::new(), false), |s| (s.query.clone(), s.regex));
        let mut input = Input::new(Purpose::Search, &query);
        input.regex = regex;
        self.input = Some(input);
        self.search = Some(SearchState {
            pane,
            query,
            regex,
            matches: Vec::new(),
            current: None,
            origin,
            error: None,
        });
        self.search_update();
    }

    /// Recompute matches for the current query.
    fn search_refresh(&mut self) {
        let Some(s) = self.search.as_mut() else { return };
        let Some(p) = self.panes.get(&s.pane) else { return };
        s.error = None;
        s.matches.clear();
        if s.query.is_empty() {
            return;
        }
        match Searcher::new(&s.query, &SearchOptions { regex: s.regex, ..SearchOptions::default() }) {
            Ok(re) => s.matches = re.find_all(p.term.grid()),
            Err(_) => s.error = Some("invalid regex".into()),
        }
    }

    /// The query was edited: search again from the origin, upwards.
    fn search_update(&mut self) {
        if let (Some(s), Some(i)) = (self.search.as_mut(), self.input.as_ref()) {
            s.query = i.text.clone();
            s.regex = i.regex;
        }
        self.search_refresh();
        let Some(s) = self.search.as_mut() else { return };
        // A match starting at the origin counts.
        let from = Point::new(s.origin.line, s.origin.col + 1);
        s.current = search::nearest(&s.matches, from, false);
        let target = s.current.map(|i| s.matches[i].start);
        let status = s.status();
        let pane = s.pane;
        if let Some(i) = self.input.as_mut() {
            i.status = if i.regex { format!("[regex] {status}") } else { status };
        }
        if let (Some(t), Some(p)) = (target, self.panes.get_mut(&pane)) {
            p.term.reveal_line(t.line);
        }
        self.dirty = true;
    }

    /// Enter in the query: the cursor goes to the current match.
    fn search_submit(&mut self) {
        self.input = None;
        match self.search.as_ref().and_then(|s| Some(s.matches[s.current?].start)) {
            Some(p) => self.copy_goto(p),
            None => self.search = None,
        }
        self.dirty = true;
    }

    /// `n` (`up`) / `N`: next match from the cursor.
    pub(super) fn search_step(&mut self, up: bool) {
        self.search_refresh();
        let Some(c) = self.copy.as_ref() else { return };
        let Some(s) = self.search.as_mut() else { return };
        s.current = search::nearest(&s.matches, c.cursor, !up);
        if let Some(i) = s.current {
            let p = s.matches[i].start;
            self.copy_goto(p);
        }
        self.dirty = true;
    }

    pub(super) fn search_marks(&self, id: PaneId) -> Vec<super::render::Mark> {
        let Some(s) = self.search.as_ref().filter(|s| s.pane == id) else { return Vec::new() };
        s.matches
            .iter()
            .enumerate()
            .map(|(i, m)| super::render::Mark { start: m.start, end: m.end, current: Some(i) == s.current })
            .collect()
    }

    // --- input line --------------------------------------------------------------

    pub(super) fn open_input(&mut self, purpose: Purpose, text: &str, status: String) {
        let mut i = Input::new(purpose, text);
        i.status = status;
        self.input = Some(i);
        self.dirty = true;
    }

    /// A key while the input line is open.
    pub(super) fn input_key(&mut self, key: Key, mods: Mods, text: &str) {
        let Some(input) = self.input.as_mut() else { return };
        let purpose = input.purpose;
        match input.key(key, mods, text) {
            Edit::Changed if purpose == Purpose::Search => self.search_update(),
            Edit::Changed | Edit::Moved => {}
            Edit::Cancel => {
                self.input = None;
                if purpose == Purpose::Search {
                    self.search = None;
                }
            }
            Edit::Submit => {
                let text = self.input.as_ref().map(|i| i.text.trim().to_string()).unwrap_or_default();
                if purpose == Purpose::Search {
                    return self.search_submit();
                }
                self.input = None;
                self.input_submit(purpose, text);
            }
        }
        self.dirty = true;
    }

    /// Text typed by the input method goes to the input line.
    pub(super) fn input_text(&mut self, text: &str) {
        let Some(input) = self.input.as_mut() else { return };
        input.insert(text);
        if input.purpose == Purpose::Search {
            self.search_update();
        }
        self.dirty = true;
    }

    fn input_submit(&mut self, purpose: Purpose, text: String) {
        let name = (!text.is_empty()).then_some(text.clone());
        match purpose {
            Purpose::Search => {}
            Purpose::RenameTab(pane) => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.root.contains(pane)) {
                    t.name = name;
                }
            }
            Purpose::RenamePane(pane) => {
                if let Some(p) = self.panes.get_mut(&pane) {
                    p.name = name;
                }
                self.update_title();
            }
            Purpose::SaveLayout if !text.is_empty() => self.save_layout(&text),
            Purpose::LoadLayout if !text.is_empty() => self.load_layout(&text),
            Purpose::SaveLayout | Purpose::LoadLayout => {}
        }
    }
}
