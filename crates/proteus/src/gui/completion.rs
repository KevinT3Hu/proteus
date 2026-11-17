//! Geometry shared by completion rendering and pointer hit testing.
use std::ops::Range;

use super::{
    App,
    render::{Label, Theme},
};

#[derive(Debug)]
struct Popup {
    x: f32,
    y: f32,
    width: f32,
    row: f32,
    visible: Range<usize>,
    cols: usize,
}

impl Popup {
    fn new(size: (f32, f32), cell: (f32, f32), bottom: f32, count: usize, selected: usize) -> Option<Self> {
        let (cw, ch) = cell;
        let capacity = ((bottom / ch).floor() as usize).saturating_sub(1).min(12);
        if capacity == 0 || size.0 < cw {
            return None;
        }
        let n = count.max(1).min(capacity);
        let start = selected.min(count.saturating_sub(1)).saturating_sub(n - 1);
        let cols = (size.0 / cw).floor().max(1.) as usize;
        Some(Self {
            x: 0.,
            y: bottom - (n + 1) as f32 * ch,
            width: cols as f32 * cw,
            row: ch,
            visible: start..(start + n).min(count),
            cols,
        })
    }

    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x
            && x < self.x + self.width
            && y >= self.y
            && y < self.y + (self.visible.len().max(1) + 1) as f32 * self.row
    }

    fn at(&self, x: f32, y: f32) -> Option<usize> {
        if !self.contains(x, y) || y < self.y + self.row {
            return None;
        }
        let index = self.visible.start + ((y - self.y) / self.row) as usize - 1;
        self.visible.contains(&index).then_some(index)
    }
}

impl App {
    fn completion_geometry(&self, count: usize) -> Option<Popup> {
        let (w, h) = self.physical();
        let (cw, ch) = self.cell();
        let bottom = h as f32 - self.pad_y() as f32 - ch as f32;
        Popup::new((w as f32, h as f32), (cw as f32, ch as f32), bottom, count, self.w.input.as_ref()?.choice)
    }

    pub(super) fn completion_labels(&self, theme: &Theme) -> Vec<Label> {
        let Some(items) = self.completion_items() else {
            return Vec::new();
        };
        let Some(popup) = self.completion_geometry(items.len()) else {
            return Vec::new();
        };
        let selected = self.w.input.as_ref().map_or(0, |i| i.choice);
        let mut labels = vec![Label {
            x: popup.x,
            y: popup.y,
            cols: popup.cols,
            text: format!(
                " {} · {}/{} · {}",
                self.cfg.i18n.t("completion-title", &[]),
                if items.is_empty() { 0 } else { selected + 1 },
                items.len(),
                self.cfg.i18n.t("completion-open-help", &[])
            ),
            fg: theme.dim,
            bg: Some(theme.bar),
            bold: false,
        }];
        if items.is_empty() {
            labels.push(Label {
                x: popup.x,
                y: popup.y + popup.row,
                cols: popup.cols,
                text: self.cfg.i18n.t("ui-no-match", &[]),
                fg: theme.dim,
                bg: Some(theme.bar),
                bold: false,
            });
        }
        let cw = self.cell().0 as f32;
        for (offset, index) in popup.visible.enumerate() {
            let on = index == selected;
            let y = popup.y + (offset + 1) as f32 * popup.row;
            let (title, key) = &items[index];
            let fg = if on { theme.bg } else { theme.fg };
            // Paint the whole row before its independent text columns.
            labels.push(Label {
                x: popup.x,
                y,
                cols: popup.cols,
                text: String::new(),
                fg,
                bg: Some(if on { theme.accent } else { theme.bar }),
                bold: false,
            });
            let key_width = key.as_deref().map(super::chrome::text_width).filter(|width| *width + 5 < popup.cols);
            let title_cols = key_width.map_or(popup.cols, |width| popup.cols - width - 2);
            labels.push(Label {
                x: popup.x,
                y,
                cols: title_cols,
                text: format!(" {} {title}", if on { "›" } else { " " }),
                fg,
                bg: None,
                bold: on,
            });
            if let (Some(key), Some(width)) = (key, key_width) {
                labels.push(Label {
                    x: popup.x + (popup.cols - width - 1) as f32 * cw,
                    y,
                    cols: width,
                    text: key.clone(),
                    fg: if on { theme.bg } else { theme.dim },
                    bg: None,
                    bold: false,
                });
            }
        }
        labels
    }

    /// Consume pointer events over the list so they never reach a terminal.
    pub(super) fn completion_pointer(&mut self, ev: &super::PointerEvent) -> bool {
        let Some(items) = self.completion_items() else {
            return false;
        };
        let Some(popup) = self.completion_geometry(items.len()) else {
            return false;
        };
        let (x, y) = ((ev.position.0 * self.w.scale) as f32, (ev.position.1 * self.w.scale) as f32);
        if !popup.contains(x, y) {
            return false;
        }
        match ev.kind {
            super::PointerEventKind::Press { button: super::BTN_LEFT, .. } => {
                if let Some(index) = popup.at(x, y) {
                    if let Some(input) = self.w.input.as_mut() {
                        input.choice = index;
                    }
                    self.palette_run();
                }
            }
            super::PointerEventKind::Axis { vertical, .. } => {
                if vertical.absolute == 0. && vertical.discrete == 0 && vertical.value120 == 0 {
                    return true;
                }
                let up = vertical.absolute < 0. || vertical.discrete < 0 || vertical.value120 < 0;
                if let Some(input) = self.w.input.as_mut() {
                    input.choice = if up { input.choice.saturating_sub(1) } else { input.choice.saturating_add(1) };
                }
                self.palette_update();
            }
            _ => {}
        }
        self.w.dirty = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn popup_scrolls_to_selection_and_hit_tests_visible_rows() {
        let popup = Popup::new((800., 600.), (10., 20.), 560., 200, 150).unwrap();
        assert_eq!(popup.visible, 139..151);
        assert_eq!(popup.at(40., 550.), Some(150));
        assert_eq!(popup.at(40., popup.y + 1.), None);
        assert_eq!(popup.at(40., 565.), None);
        assert_eq!(popup.at(801., 550.), None);
        let small = Popup::new((80., 80.), (10., 20.), 60., 200, 199).unwrap();
        assert_eq!(small.visible, 198..200);
        assert!(small.y >= 0.);
        assert!(Popup::new((80., 20.), (10., 20.), 0., 200, 0).is_none());
    }
}
