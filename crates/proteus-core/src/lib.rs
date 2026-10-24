//! Proteus terminal emulation core.
//!
//! This crate has no GUI dependency; Linux file-backed graphics use rustix. Feed it PTY bytes with
//! [`Terminal::advance`], read back the grid for rendering, drain
//! [`Terminal::take_pty_output`] (replies the terminal sends to the app) and
//! [`Terminal::take_events`] (things the host must act on).

pub mod bidi;
pub mod blocks;
pub mod cell;
pub mod color;
pub mod event;
pub mod graphics;
pub mod grid;
pub mod hints;
pub mod input;
pub mod modes;
pub mod search;
pub mod selection;
pub mod style;
mod term;
pub mod text;

pub use blocks::{Block, BlockId, BlockState, Point};
pub use event::Event;
pub use graphics::{GraphicsConfig, ImageBudget, ImageView};
pub use grid::{Grid, Line};
pub use modes::{CursorShape, KeyboardFlags, TermMode};
pub use term::{BlockPart, Cursor, Damage, Terminal};

/// Policy for OSC 52 clipboard reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardRead {
    /// Raise [`Event::ClipboardLoad`]; the host asks the user (design default).
    Ask,
    /// Raise [`Event::ClipboardLoad`]; the host answers without asking.
    Allow,
    Deny,
}

/// Terminal configuration relevant to emulation.
#[derive(Debug, Clone)]
pub struct Config {
    /// Scrollback lines per pane.
    pub scrollback: usize,
    /// Treat East Asian ambiguous-width characters as wide (`text.ambiguous_width = 2`).
    pub ambiguous_wide: bool,
    pub bidi: bidi::Mode,
    pub osc52_write: bool,
    pub osc52_read: ClipboardRead,
    /// At most this many OSC 9/777 notifications per `notify_window`.
    pub notify_burst: usize,
    pub notify_window: std::time::Duration,
    /// Characters that end a word for word selection.
    pub word_separators: String,
    /// Inline images (kitty graphics, Sixel, iTerm2).
    pub graphics: GraphicsConfig,
    /// Mode 2027 (grapheme clustering) is on after power-on and RIS.
    pub grapheme_clustering: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scrollback: 10_000,
            ambiguous_wide: false,
            bidi: bidi::Mode::default(),
            osc52_write: true,
            osc52_read: ClipboardRead::Ask,
            notify_burst: 5,
            notify_window: std::time::Duration::from_secs(10),
            word_separators: " \t,│`|:\"'()[]{}<>".to_string(),
            graphics: GraphicsConfig::default(),
            grapheme_clustering: false,
        }
    }
}
