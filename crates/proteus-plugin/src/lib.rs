//! SDK for Proteus plugins: the generated bindings of `wit/proteus.wit`
//! plus styled text, widgets and bounded cell-based UI composition.
//! See [`Component`] and [`ui`] for declarative views, layouts, inputs and images;
//! [`widgets`] provides lists, text input, trees, tables, spinners and scrolling.
//!
//! ```ignore
//! use proteus_plugin::{Guest, host, types::*};
//!
//! struct Hello;
//!
//! impl Guest for Hello {
//!     fn init() -> Result<(), String> {
//!         host::notify("hello", false);
//!         Ok(())
//!     }
//!     fn on_event(_: Event) {}
//!     fn on_ui(_: SurfaceId, _: UiEvent) {}
//! }
//!
//! proteus_plugin::export!(Hello);
//! ```
//!
//! Load the source directory with `p.load_plugin { source = "path/to/plugin" }`;
//! Proteus builds and caches it. Prebuilt components are also supported.

#![allow(clippy::too_many_arguments)]

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "plugin",
        pub_export_macro: true,
        default_bindings_module: "proteus_plugin",
    });
}

pub use bindings::proteus::plugin::types;

/// Terminal host calls. Actions use stable IDs, independent of palette labels.
pub use serde;
pub use serde_json;

pub mod host {
    /// Deserialize plugin-owned settings. An omitted config uses T::default().
    /// Read during init; changing settings in init.lua restarts the instance.
    pub fn config<T: serde::de::DeserializeOwned + Default>() -> Result<T, String> {
        let raw = plugin_config()?;
        if raw == "null" {
            return Ok(T::default());
        }
        serde_json::from_str(&raw).map_err(|e| format!("plugin config: {e}"))
    }

    pub use super::bindings::proteus::plugin::host::*;

    /// Register an action, e.g. ("sidebar.toggle", "Files: sidebar", Some("alt+e")).
    /// Users bind it as `plugin:PLUGIN-ID:sidebar.toggle` in any Lua keymap.
    /// The matching `Event::Action` carries "sidebar.toggle".
    pub fn register_action(id: &str, name: &str, default_key: Option<&str>) -> Result<(), String> {
        super::bindings::proteus::plugin::host::register_action_id(id, name, default_key)
    }
}
#[doc(hidden)]
pub use bindings::*;
pub use bindings::{Guest, export};

/// Fluent resources with typed arguments, select expressions, and CLDR plural rules.
/// Create a catalog with `Catalog::new(&host::locale()?)`; refresh its locale
/// on `Event::ConfigChanged` before redrawing localized UI.
pub mod i18n;

pub mod ui;
pub mod widgets;
pub use ui::{Component, Element};
pub use widgets::{Column, Edit, Node, ScrollView, Spinner, Table, TextInput, Tree};

use types::{Attrs, Color, Line, Span, Style};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

impl Style {
    pub const PLAIN: Style = Style { fg: None, bg: None, attrs: Attrs::empty() };
}

impl Default for Style {
    fn default() -> Self {
        Style::PLAIN
    }
}

/// Styled text: `text("x").fg(Color::Indexed(2)).bold()`.
pub fn text(s: impl Into<String>) -> Span {
    Span { text: s.into(), style: Style::PLAIN, link: None }
}

impl Span {
    pub fn fg(mut self, c: Color) -> Self {
        self.style.fg = Some(c);
        self
    }
    pub fn bg(mut self, c: Color) -> Self {
        self.style.bg = Some(c);
        self
    }
    pub fn bold(mut self) -> Self {
        self.style.attrs |= Attrs::BOLD;
        self
    }
    pub fn dim(mut self) -> Self {
        self.style.attrs |= Attrs::DIM;
        self
    }
    pub fn italic(mut self) -> Self {
        self.style.attrs |= Attrs::ITALIC;
        self
    }
    pub fn reverse(mut self) -> Self {
        self.style.attrs |= Attrs::REVERSE;
        self
    }
    pub fn link(mut self, url: impl Into<String>) -> Self {
        self.link = Some(url.into());
        self
    }
}

/// Display width in cells.
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// `s` cut to `cols` cells, with `…` if it was longer.
pub fn fit(s: &str, cols: usize) -> String {
    if width(s) <= cols {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.graphemes(true) {
        let cw = width(c);
        if w + cw + 1 > cols {
            break;
        }
        out.push_str(c);
        w += cw;
    }
    if cols > 0 {
        out.push('…');
    }
    out
}

/// A line padded with spaces to `cols` (so a background fills the row).
pub fn pad(mut line: Line, cols: usize, style: Style) -> Line {
    let w: usize = line.iter().map(|s| width(&s.text)).sum();
    if w < cols {
        line.push(Span { text: " ".repeat(cols - w), style, link: None });
    }
    line
}

/// A scrolling list with a selection; handles the usual keys.
#[derive(Debug, Clone, Default)]
pub struct List {
    pub len: usize,
    pub selected: usize,
    /// First visible item.
    pub offset: usize,
}

impl List {
    /// Move the selection for `key` (`up`/`k`, `down`/`j`, `home`/`g`,
    /// `end`/`G`, `page-up`, `page-down`). Returns true if the key was used.
    pub fn key(&mut self, key: &str, text: &str, rows: usize) -> bool {
        let last = self.len.saturating_sub(1);
        let page = rows.max(2) - 1;
        self.selected = match (key, text) {
            ("up", _) | (_, "k") => self.selected.saturating_sub(1),
            ("down", _) | (_, "j") => (self.selected + 1).min(last),
            ("home", _) | (_, "g") => 0,
            ("end", _) | (_, "G") => last,
            ("page-up", _) => self.selected.saturating_sub(page),
            ("page-down", _) => (self.selected + page).min(last),
            _ => return false,
        };
        true
    }

    /// Keep the selection in range and in view of `rows` rows.
    pub fn clamp(&mut self, rows: usize) {
        self.selected = self.selected.min(self.len.saturating_sub(1));
        let rows = rows.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + rows {
            self.offset = self.selected + 1 - rows;
        }
        self.offset = self.offset.min(self.len.saturating_sub(rows));
    }

    /// Items visible in `rows` rows: (index, selected).
    pub fn visible(&mut self, rows: usize) -> impl Iterator<Item = (usize, bool)> + use<> {
        self.clamp(rows);
        let sel = self.selected;
        (self.offset..(self.offset + rows).min(self.len)).map(move |i| (i, i == sel))
    }

    /// Item at a visible row (a mouse click), if any.
    pub fn at_row(&self, row: usize) -> Option<usize> {
        Some(self.offset + row).filter(|&i| i < self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_scrolls() {
        let mut l = List { len: 10, ..List::default() };
        assert!(l.key("down", "", 4) && l.selected == 1);
        assert!(l.key("end", "", 4) && l.selected == 9);
        assert_eq!(l.visible(4).map(|v| v.0).collect::<Vec<_>>(), [6, 7, 8, 9]);
        assert!(l.key("page-up", "", 4) && l.selected == 6);
        assert!(l.key("", "g", 4) && l.selected == 0);
        assert_eq!(l.visible(4).next(), Some((0, true)));
        assert_eq!(l.at_row(3), Some(3));
        assert!(!l.key("x", "x", 4));
        assert_eq!(fit("hello world", 6), "hello…");
        assert_eq!(fit("中文字", 4), "中…");
        assert_eq!(fit("ok", 6), "ok");
    }
}
