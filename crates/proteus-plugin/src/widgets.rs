//! Widgets beyond [`List`](crate::List): they keep state and turn it into
//! [`Line`]s; keys are fed in from `on_ui`.

use crate::types::{Attrs, Line, Span, Style};
use crate::{fit, pad, text, width};
use unicode_width::UnicodeWidthChar;

/// A one-line text field with a cursor.
#[derive(Debug, Clone, Default)]
pub struct TextInput {
    pub text: String,
    /// Cursor position in characters.
    pub cursor: usize,
}

/// What a key did to a [`TextInput`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    Changed,
    Moved,
    Submit,
    Cancel,
    /// Not a key for the field.
    Ignored,
}

impl TextInput {
    pub fn new(s: &str) -> Self {
        TextInput { text: s.into(), cursor: s.chars().count() }
    }

    fn byte(&self, c: usize) -> usize {
        self.text.char_indices().nth(c).map_or(self.text.len(), |(i, _)| i)
    }

    /// Insert typed text (also input-method text from `ui-event::text`).
    pub fn insert(&mut self, s: &str) {
        let s: String = s.chars().filter(|c| !c.is_control()).collect();
        let at = self.byte(self.cursor);
        self.text.insert_str(at, &s);
        self.cursor += s.chars().count();
    }

    /// Handle a key (`key` and `text` of a `key-event`; `ctrl` held).
    pub fn key(&mut self, key: &str, text: &str, ctrl: bool) -> Edit {
        let len = self.text.chars().count();
        match (key, ctrl) {
            ("enter", _) => Edit::Submit,
            ("escape", _) => Edit::Cancel,
            ("left", _) | ("b", true) => {
                self.cursor = self.cursor.saturating_sub(1);
                Edit::Moved
            }
            ("right", _) | ("f", true) => {
                self.cursor = (self.cursor + 1).min(len);
                Edit::Moved
            }
            ("home", _) | ("a", true) => {
                self.cursor = 0;
                Edit::Moved
            }
            ("end", _) | ("e", true) => {
                self.cursor = len;
                Edit::Moved
            }
            ("backspace", _) if self.cursor > 0 => {
                let (a, b) = (self.byte(self.cursor - 1), self.byte(self.cursor));
                self.text.replace_range(a..b, "");
                self.cursor -= 1;
                Edit::Changed
            }
            ("delete", _) | ("d", true) if self.cursor < len => {
                let (a, b) = (self.byte(self.cursor), self.byte(self.cursor + 1));
                self.text.replace_range(a..b, "");
                Edit::Changed
            }
            ("u", true) => {
                let b = self.byte(self.cursor);
                self.text.replace_range(..b, "");
                self.cursor = 0;
                Edit::Changed
            }
            ("k", true) => {
                let b = self.byte(self.cursor);
                self.text.truncate(b);
                Edit::Changed
            }
            ("backspace" | "delete", _) => Edit::Moved,
            _ if !ctrl && !text.is_empty() && !text.chars().any(char::is_control) => {
                self.insert(text);
                Edit::Changed
            }
            _ => Edit::Ignored,
        }
    }

    /// The field in `cols` cells (scrolled so the cursor shows) and the
    /// cursor's column, for `frame.cursor`.
    pub fn render(&self, cols: usize, style: Style) -> (Line, u32) {
        let chars: Vec<char> = self.text.chars().collect();
        let before: usize = chars[..self.cursor].iter().map(|c| c.width().unwrap_or(0)).sum();
        // Skip characters until the cursor fits.
        let mut skip = 0;
        let mut shown = before;
        while shown + 1 > cols && skip < self.cursor {
            shown -= chars[skip].width().unwrap_or(0);
            skip += 1;
        }
        let mut out = String::new();
        let mut w = 0;
        for c in &chars[skip..] {
            let cw = c.width().unwrap_or(0);
            if w + cw > cols {
                break;
            }
            out.push(*c);
            w += cw;
        }
        (pad(vec![Span { text: out, style, link: None }], cols, style), shown as u32)
    }
}

/// A node of a [`Tree`].
#[derive(Debug, Clone, Default)]
pub struct Node {
    pub label: String,
    pub children: Vec<Node>,
    pub open: bool,
}

impl Node {
    pub fn leaf(label: impl Into<String>) -> Self {
        Node { label: label.into(), ..Node::default() }
    }
    pub fn branch(label: impl Into<String>, children: Vec<Node>) -> Self {
        Node { label: label.into(), children, open: false }
    }
}

/// A collapsible tree with a selection. Rows are the open part of the tree.
#[derive(Debug, Clone, Default)]
pub struct Tree {
    pub roots: Vec<Node>,
    pub list: crate::List,
}

impl Tree {
    /// Path (child indices) of every visible row, and its depth.
    pub fn rows(&self) -> Vec<Vec<usize>> {
        fn walk(nodes: &[Node], path: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
            for (i, n) in nodes.iter().enumerate() {
                path.push(i);
                out.push(path.clone());
                if n.open {
                    walk(&n.children, path, out);
                }
                path.pop();
            }
        }
        let mut out = Vec::new();
        walk(&self.roots, &mut Vec::new(), &mut out);
        out
    }

    pub fn node(&self, path: &[usize]) -> Option<&Node> {
        let (first, rest) = path.split_first()?;
        rest.iter().try_fold(self.roots.get(*first)?, |n, &i| n.children.get(i))
    }

    pub fn node_mut(&mut self, path: &[usize]) -> Option<&mut Node> {
        let (first, rest) = path.split_first()?;
        rest.iter().try_fold(self.roots.get_mut(*first)?, |n, &i| n.children.get_mut(i))
    }

    /// Path of the selected row.
    pub fn selected(&self) -> Option<Vec<usize>> {
        self.rows().into_iter().nth(self.list.selected)
    }

    /// List keys, plus `right`/`l` to open, `left`/`h` to close (or go to
    /// the parent) and `space` to toggle.
    pub fn key(&mut self, key: &str, text: &str, rows: usize) -> bool {
        let all = self.rows();
        self.list.len = all.len();
        let Some(path) = all.get(self.list.selected).cloned() else { return self.list.key(key, text, rows) };
        let has_children = self.node(&path).is_some_and(|n| !n.children.is_empty());
        match (key, text) {
            ("right", _) | (_, "l") if has_children => {
                if let Some(n) = self.node_mut(&path) {
                    n.open = true;
                }
            }
            (_, " ") if has_children => {
                if let Some(n) = self.node_mut(&path) {
                    n.open = !n.open;
                }
            }
            ("left", _) | (_, "h") => {
                if has_children && self.node(&path).is_some_and(|n| n.open) {
                    if let Some(n) = self.node_mut(&path) {
                        n.open = false;
                    }
                } else if path.len() > 1 {
                    let parent = &path[..path.len() - 1];
                    if let Some(i) = all.iter().position(|p| p == parent) {
                        self.list.selected = i;
                    }
                }
            }
            _ => return self.list.key(key, text, rows),
        }
        self.list.len = self.rows().len();
        true
    }

    /// Visible rows in `cols` × `rows` cells; the selected row highlighted
    /// with `selected`.
    pub fn render(&mut self, cols: usize, rows: usize, selected: Style) -> Vec<Line> {
        let all = self.rows();
        self.list.len = all.len();
        self.list
            .visible(rows)
            .map(|(i, sel)| {
                let path = &all[i];
                let n = self.node(path).expect("visible row");
                let mark = match (n.children.is_empty(), n.open) {
                    (true, _) => "  ",
                    (false, true) => "▾ ",
                    (false, false) => "▸ ",
                };
                let indent = "  ".repeat(path.len() - 1);
                let label = fit(&format!("{indent}{mark}{}", n.label), cols);
                let style = if sel { selected } else { Style::PLAIN };
                pad(vec![Span { text: label, style, link: None }], cols, style)
            })
            .collect()
    }
}

/// Columns of a [`Table`]: title and width in cells (0: share what is left).
#[derive(Debug, Clone)]
pub struct Column {
    pub title: String,
    pub width: usize,
}

/// A table with a header row and a selectable body.
#[derive(Debug, Clone, Default)]
pub struct Table {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<String>>,
    pub list: crate::List,
}

impl Table {
    fn widths(&self, cols: usize) -> Vec<usize> {
        let gaps = self.columns.len().saturating_sub(1);
        let fixed: usize = self.columns.iter().map(|c| c.width).sum();
        let flex = self.columns.iter().filter(|c| c.width == 0).count();
        let rest = cols.saturating_sub(fixed + gaps);
        self.columns.iter().map(|c| if c.width == 0 { rest / flex.max(1) } else { c.width }).collect()
    }

    fn line(cells: &[String], widths: &[usize], cols: usize, style: Style) -> Line {
        let mut s = String::new();
        for (i, (c, w)) in cells.iter().zip(widths).enumerate() {
            if i > 0 {
                s.push(' ');
            }
            let c = fit(c, *w);
            let n = width(&c);
            s.push_str(&c);
            s.push_str(&" ".repeat(w.saturating_sub(n)));
        }
        pad(vec![Span { text: fit(&s, cols), style, link: None }], cols, style)
    }

    pub fn key(&mut self, key: &str, text: &str, rows: usize) -> bool {
        self.list.len = self.rows.len();
        self.list.key(key, text, rows.saturating_sub(1))
    }

    /// The header and the visible rows in `cols` × `rows` cells.
    pub fn render(&mut self, cols: usize, rows: usize, selected: Style) -> Vec<Line> {
        let widths = self.widths(cols);
        let head: Vec<String> = self.columns.iter().map(|c| c.title.clone()).collect();
        let bold = Style { fg: None, bg: None, attrs: Attrs::BOLD };
        let mut out = vec![Self::line(&head, &widths, cols, bold)];
        self.list.len = self.rows.len();
        for (i, sel) in self.list.visible(rows.saturating_sub(1)) {
            out.push(Self::line(&self.rows[i], &widths, cols, if sel { selected } else { Style::PLAIN }));
        }
        out
    }

    /// Body row at a visible row of the table (row 0 is the header).
    pub fn at_row(&self, row: usize) -> Option<usize> {
        self.list.at_row(row.checked_sub(1)?)
    }
}

/// A spinner for work in progress: call [`tick`](Spinner::tick) from a
/// timer.
#[derive(Debug, Clone, Default)]
pub struct Spinner {
    frame: usize,
}

impl Spinner {
    const FRAMES: [&'static str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    pub fn tick(&mut self) {
        self.frame = (self.frame + 1) % Self::FRAMES.len();
    }

    pub fn span(&self) -> Span {
        text(Self::FRAMES[self.frame])
    }
}

/// A view of many lines scrolled with the keys and the wheel (for text
/// that is read, not selected from).
#[derive(Debug, Clone, Default)]
pub struct ScrollView {
    pub lines: Vec<Line>,
    pub offset: usize,
}

impl ScrollView {
    fn max(&self, rows: usize) -> usize {
        self.lines.len().saturating_sub(rows)
    }

    pub fn scroll(&mut self, by: isize, rows: usize) {
        self.offset = self.offset.saturating_add_signed(by).min(self.max(rows));
    }

    /// `up`/`k`, `down`/`j`, `page-up`, `page-down`/`space`, `home`/`g`,
    /// `end`/`G`.
    pub fn key(&mut self, key: &str, text: &str, rows: usize) -> bool {
        let page = rows.max(2) as isize - 1;
        match (key, text) {
            ("up", _) | (_, "k") => self.scroll(-1, rows),
            ("down", _) | (_, "j") => self.scroll(1, rows),
            ("page-up", _) => self.scroll(-page, rows),
            ("page-down", _) | (_, " ") => self.scroll(page, rows),
            ("home", _) | (_, "g") => self.offset = 0,
            ("end", _) | (_, "G") => self.offset = self.max(rows),
            _ => return false,
        }
        true
    }

    /// Wheel buttons of a `mouse-event`.
    pub fn wheel(&mut self, up: bool, rows: usize) {
        self.scroll(if up { -3 } else { 3 }, rows);
    }

    /// The visible lines, with a scroll bar in the last column when the
    /// lines do not fit.
    pub fn render(&mut self, cols: usize, rows: usize) -> Vec<Line> {
        self.offset = self.offset.min(self.max(rows));
        let total = self.lines.len();
        let bar = total > rows && cols > 1;
        let (thumb, size) = if bar {
            let size = (rows * rows / total).max(1);
            (self.offset * (rows - size) / self.max(rows).max(1), size)
        } else {
            (0, 0)
        };
        let w = if bar { cols - 1 } else { cols };
        let dim = Style { fg: None, bg: None, attrs: Attrs::DIM };
        (0..rows)
            .map(|r| {
                let mut line = match self.lines.get(self.offset + r) {
                    Some(l) => clip(l, w),
                    None => Vec::new(),
                };
                if bar {
                    line = pad(line, w, Style::PLAIN);
                    let on = r >= thumb && r < thumb + size;
                    line.push(Span { text: if on { "┃" } else { "│" }.into(), style: dim, link: None });
                }
                line
            })
            .collect()
    }
}

/// `line` cut to `cols` cells.
pub fn clip(line: &Line, cols: usize) -> Line {
    let mut out = Vec::new();
    let mut left = cols;
    for s in line {
        if left == 0 {
            break;
        }
        let w = width(&s.text);
        if w <= left {
            out.push(s.clone());
            left -= w;
        } else {
            let mut t = String::new();
            let mut n = 0;
            for c in s.text.chars() {
                let cw = c.width().unwrap_or(0);
                if n + cw > left {
                    break;
                }
                t.push(c);
                n += cw;
            }
            out.push(Span { text: t, ..s.clone() });
            left = 0;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(l: &Line) -> String {
        l.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn text_input_edits() {
        let mut t = TextInput::new("ab");
        assert_eq!(t.key("left", "", false), Edit::Moved);
        assert_eq!(t.key("x", "中", false), Edit::Changed);
        assert_eq!((t.text.as_str(), t.cursor), ("a中b", 2));
        assert_eq!(t.key("backspace", "", false), Edit::Changed);
        assert_eq!(t.text, "ab");
        assert_eq!(t.key("k", "", true), Edit::Changed);
        assert_eq!(t.text, "a");
        assert_eq!(t.key("enter", "", false), Edit::Submit);
        let t = TextInput::new("abcdefgh");
        let (l, cur) = t.render(4, Style::PLAIN);
        assert_eq!((line_text(&l).as_str(), cur), ("fgh ", 3));
    }

    #[test]
    fn tree_opens_and_closes() {
        let mut t = Tree {
            roots: vec![Node::branch("a", vec![Node::leaf("a1"), Node::leaf("a2")]), Node::leaf("b")],
            ..Tree::default()
        };
        assert_eq!(t.rows().len(), 2);
        assert!(t.key("right", "", 10));
        assert_eq!(t.rows().len(), 4);
        assert!(t.key("down", "", 10));
        assert_eq!(t.selected(), Some(vec![0, 0]));
        assert!(t.key("left", "", 10));
        assert_eq!(t.selected(), Some(vec![0]));
        let lines = t.render(10, 10, Style::PLAIN);
        assert_eq!(line_text(&lines[1]).trim_end(), "    a1");
        assert!(t.key("left", "", 10));
        assert_eq!(t.rows().len(), 2);
    }

    #[test]
    fn table_and_scroll_view() {
        let mut t = Table {
            columns: vec![Column { title: "Name".into(), width: 0 }, Column { title: "N".into(), width: 3 }],
            rows: vec![vec!["alpha".into(), "1".into()], vec!["beta".into(), "22".into()]],
            ..Table::default()
        };
        let lines = t.render(10, 3, Style::PLAIN);
        assert_eq!(line_text(&lines[0]), "Name   N  ");
        assert_eq!(line_text(&lines[2]), "beta   22 ");
        assert_eq!(t.at_row(2), Some(1));
        let mut v = ScrollView { lines: (0..10).map(|i| vec![text(i.to_string())]).collect(), offset: 0 };
        assert!(v.key("end", "", 4));
        let r = v.render(3, 4);
        assert_eq!(line_text(&r[0]), "6 │");
        assert_eq!(line_text(&r[3]), "9 ┃");
    }
}
