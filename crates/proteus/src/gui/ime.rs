//! Input methods via `zwp_text_input_v3`: preedit (composition) text is
//! shown at the cursor, committed text is written to the PTY as if typed,
//! and the cursor rectangle is reported so the candidate window follows it.

use smithay_client_toolkit::dispatch2::Dispatch2;
use smithay_client_toolkit::reexports::client::{Connection, Proxy, QueueHandle};
use smithay_client_toolkit::reexports::protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use smithay_client_toolkit::reexports::protocols::wp::text_input::zv3::client::zwp_text_input_v3::{
    self, ContentHint, ContentPurpose, ZwpTextInputV3,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::App;

/// Composition text from the input method.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preedit {
    pub text: String,
    /// Cursor / selected segment as byte offsets into `text`; `None` hides it.
    pub cursor: Option<(usize, usize)>,
}

/// One grapheme of the preedit laid out on the grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreeditCell<'a> {
    /// Column offset from the start of the preedit.
    pub col: usize,
    pub width: usize,
    pub text: &'a str,
    /// Byte range in the preedit text.
    pub bytes: (usize, usize),
}

impl Preedit {
    /// Graphemes with their columns (zero-width graphemes take one column).
    pub fn cells(&self) -> Vec<PreeditCell<'_>> {
        let mut col = 0;
        self.text
            .grapheme_indices(true)
            .map(|(b, g)| {
                let width = g.width().clamp(1, 2);
                let cell = PreeditCell { col, width, text: g, bytes: (b, b + g.len()) };
                col += width;
                cell
            })
            .collect()
    }

    pub fn width(&self) -> usize {
        self.cells().last().map_or(0, |c| c.col + c.width)
    }

    /// Column offset of the cursor (start of the selected segment).
    pub fn cursor_col(&self) -> Option<usize> {
        let (begin, _) = self.cursor?;
        let cells = self.cells();
        Some(cells.iter().find(|c| c.bytes.0 >= begin).map_or_else(|| self.width(), |c| c.col))
    }
}

/// Double-buffered state applied on `done`.
#[derive(Default)]
struct Pending {
    preedit: Option<Preedit>,
    commit: Option<String>,
}

pub struct Ime {
    input: ZwpTextInputV3,
    /// The text input is enabled for our surface.
    active: bool,
    pending: Pending,
    pub preedit: Option<Preedit>,
    /// Last cursor rectangle sent, surface-local logical pixels.
    rect: Option<(i32, i32, i32, i32)>,
}

impl Ime {
    pub fn new(input: ZwpTextInputV3) -> Self {
        Self { input, active: false, pending: Pending::default(), preedit: None, rect: None }
    }

    pub fn destroy(self) {
        self.input.destroy();
    }

    /// Report where the cursor is so the candidate window can follow it.
    pub fn set_cursor_rect(&mut self, rect: (i32, i32, i32, i32)) {
        if !self.active || self.rect == Some(rect) {
            return;
        }
        self.rect = Some(rect);
        self.input.set_cursor_rectangle(rect.0, rect.1, rect.2, rect.3);
        self.input.commit();
    }
}

/// User data for text-input objects.
pub struct ImeData;

impl Dispatch2<ZwpTextInputManagerV3, App> for ImeData {
    fn event(
        &self,
        _: &mut App,
        _: &ZwpTextInputManagerV3,
        _: <ZwpTextInputManagerV3 as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
    }
}

impl Dispatch2<ZwpTextInputV3, App> for ImeData {
    fn event(&self, app: &mut App, _: &ZwpTextInputV3, ev: zwp_text_input_v3::Event, _: &Connection, _: &QueueHandle<App>) {
        use zwp_text_input_v3::Event;
        // Enter/leave name a window; everything else goes to the keyboard's.
        let ours = match &ev {
            Event::Enter { surface } | Event::Leave { surface } => app.activate_surface(surface),
            _ => {
                app.activate_kbd();
                true
            }
        };
        let Some(ime) = app.ime.as_mut() else { return };
        match ev {
            Event::Enter { .. } if ours => {
                ime.active = true;
                ime.rect = None;
                ime.input.enable();
                ime.input.set_content_type(ContentHint::None, ContentPurpose::Terminal);
                ime.input.commit();
                app.update_ime_rect();
            }
            Event::Leave { .. } if ours => {
                ime.active = false;
                ime.pending = Pending::default();
                ime.input.disable();
                ime.input.commit();
                if ime.preedit.take().is_some() {
                    app.mark_dirty();
                }
            }
            Event::PreeditString { text, cursor_begin, cursor_end } => {
                let text = text.unwrap_or_default();
                let valid = |i: i32| usize::try_from(i).ok().filter(|&i| i <= text.len() && text.is_char_boundary(i));
                let cursor = valid(cursor_begin).zip(valid(cursor_end)).map(|(b, e)| (b.min(e), b.max(e)));
                ime.pending.preedit = (!text.is_empty()).then_some(Preedit { text, cursor });
            }
            Event::CommitString { text } => ime.pending.commit = text,
            // We never send surrounding text, so there is nothing to delete.
            Event::DeleteSurroundingText { .. } => {}
            Event::Done { .. } => {
                // Preedit not re-sent in this batch is cleared.
                let preedit = ime.pending.preedit.take();
                let commit = ime.pending.commit.take();
                let changed = preedit != ime.preedit;
                ime.preedit = preedit;
                if let Some(text) = commit.filter(|t| !t.is_empty()) {
                    app.commit_text(&text);
                }
                if changed {
                    app.mark_dirty();
                    app.scroll_focused_to_bottom();
                }
                app.update_ime_rect();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preedit_layout() {
        let p = Preedit { text: "ni你好".into(), cursor: Some((4, 4)) };
        let cells = p.cells();
        assert_eq!(cells.iter().map(|c| (c.col, c.width)).collect::<Vec<_>>(), [(0, 1), (1, 1), (2, 2), (4, 2)]);
        assert_eq!(p.width(), 6);
        // Byte 4 is inside 你 (bytes 2..5): the cursor lands after it.
        assert_eq!(p.cursor_col(), Some(4));
        let end = Preedit { text: "ab".into(), cursor: Some((2, 2)) };
        assert_eq!(end.cursor_col(), Some(2));
        let hidden = Preedit { text: "ab".into(), cursor: None };
        assert_eq!(hidden.cursor_col(), None);
        let combining = Preedit { text: "e\u{301}x".into(), cursor: Some((0, 0)) };
        assert_eq!(combining.cells().len(), 2);
    }
}
