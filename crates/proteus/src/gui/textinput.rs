//! The one-line text input in the bottom bar (search query, renames,
//! layout names).

use proteus_core::input::{Key, Mods};
use unicode_segmentation::UnicodeSegmentation;

use crate::layout::PaneId;

/// What the typed text is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Search,
    /// Rename the tab that contains this pane.
    RenameTab(PaneId),
    RenamePane(PaneId),
    SaveLayout,
    LoadLayout,
}

impl Purpose {
    pub fn badge(self) -> &'static str {
        match self {
            Purpose::Search => "SEARCH",
            Purpose::RenameTab(_) => "RENAME TAB",
            Purpose::RenamePane(_) => "RENAME PANE",
            Purpose::SaveLayout => "SAVE LAYOUT",
            Purpose::LoadLayout => "LOAD LAYOUT",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Input {
    pub purpose: Purpose,
    pub text: String,
    /// Byte offset of the caret.
    pub cursor: usize,
    /// Search: the query is a regular expression.
    pub regex: bool,
    /// Shown after the text (match count, error, choices).
    pub status: String,
}

/// Result of a key press in the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// Text (or options) changed.
    Changed,
    /// Only the caret moved, or nothing happened.
    Moved,
    Submit,
    Cancel,
}

impl Input {
    pub fn new(purpose: Purpose, text: &str) -> Self {
        Self { purpose, text: text.to_string(), cursor: text.len(), regex: false, status: String::new() }
    }

    fn prev_boundary(&self) -> usize {
        self.text[..self.cursor].grapheme_indices(true).next_back().map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self) -> usize {
        self.text[self.cursor..].graphemes(true).next().map_or(self.cursor, |g| self.cursor + g.len())
    }

    pub fn insert(&mut self, s: &str) {
        let s: String = s.chars().filter(|c| !c.is_control()).collect();
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    /// Apply a key. `text` is what the key types.
    pub fn key(&mut self, key: Key, mods: Mods, text: &str) -> Edit {
        let m = mods & (Mods::SHIFT | Mods::CTRL | Mods::ALT | Mods::SUPER);
        let ctrl = m == Mods::CTRL;
        match key {
            Key::Enter => Edit::Submit,
            Key::Escape => Edit::Cancel,
            Key::Char('c' | 'g') if ctrl => Edit::Cancel,
            Key::Backspace if self.cursor > 0 => {
                let p = self.prev_boundary();
                self.text.replace_range(p..self.cursor, "");
                self.cursor = p;
                Edit::Changed
            }
            Key::Delete | Key::Char('d') if (key == Key::Delete || ctrl) && self.cursor < self.text.len() => {
                let n = self.next_boundary();
                self.text.replace_range(self.cursor..n, "");
                Edit::Changed
            }
            Key::Left | Key::Char('b') if key == Key::Left || ctrl => {
                self.cursor = self.prev_boundary();
                Edit::Moved
            }
            Key::Right | Key::Char('f') if key == Key::Right || ctrl => {
                self.cursor = self.next_boundary();
                Edit::Moved
            }
            Key::Home | Key::Char('a') if key == Key::Home || ctrl => {
                self.cursor = 0;
                Edit::Moved
            }
            Key::End | Key::Char('e') if key == Key::End || ctrl => {
                self.cursor = self.text.len();
                Edit::Moved
            }
            Key::Char('u') if ctrl => {
                self.text.replace_range(..self.cursor, "");
                self.cursor = 0;
                Edit::Changed
            }
            Key::Char('w') if ctrl => {
                let before = &self.text[..self.cursor];
                let trimmed = before.trim_end();
                let start = trimmed.rfind(char::is_whitespace).map_or(0, |i| i + 1);
                self.text.replace_range(start..self.cursor, "");
                self.cursor = start;
                Edit::Changed
            }
            Key::Char('r') if ctrl && self.purpose == Purpose::Search => {
                self.regex = !self.regex;
                Edit::Changed
            }
            _ if !text.is_empty() && !m.intersects(Mods::CTRL | Mods::ALT | Mods::SUPER) => {
                self.insert(text);
                Edit::Changed
            }
            _ => Edit::Moved,
        }
    }

    /// Text before the caret (for placing the caret).
    pub fn before_cursor(&self) -> &str {
        &self.text[..self.cursor]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing() {
        let mut i = Input::new(Purpose::Search, "");
        let n = Mods::empty();
        for c in ["f", "o", "o", " ", "b", "a", "r"] {
            assert_eq!(i.key(Key::Char(c.chars().next().unwrap()), n, c), Edit::Changed);
        }
        assert_eq!(i.text, "foo bar");
        i.key(Key::Char('w'), Mods::CTRL, "");
        assert_eq!(i.text, "foo ");
        i.key(Key::Home, n, "");
        i.insert("é");
        assert_eq!((i.text.as_str(), i.cursor), ("éfoo ", 2));
        i.key(Key::Backspace, n, "");
        assert_eq!(i.text, "foo ");
        i.key(Key::Right, n, "");
        i.key(Key::Char('u'), Mods::CTRL, "");
        assert_eq!((i.text.as_str(), i.cursor), ("oo ", 0));
        assert_eq!(i.key(Key::Char('r'), Mods::CTRL, ""), Edit::Changed);
        assert!(i.regex);
        assert_eq!(i.key(Key::Char('x'), Mods::ALT, "x"), Edit::Moved);
        assert_eq!(i.key(Key::Enter, n, "\r"), Edit::Submit);
        assert_eq!(i.key(Key::Escape, n, ""), Edit::Cancel);
        let mut r = Input::new(Purpose::RenameTab(1), "abc");
        assert_eq!(r.key(Key::Char('r'), Mods::CTRL, ""), Edit::Moved);
        r.key(Key::Left, n, "");
        r.key(Key::Delete, n, "");
        assert_eq!(r.text, "ab");
    }
}
