//! Tab bar and hint bar layout: turns state into labels on the cell grid.

use proteus_core::color::Rgb;
use unicode_width::UnicodeWidthStr;

use super::render::{Label, Theme};

/// Widest a tab may get, in cells.
const TAB_MAX: usize = 24;
const TAB_MIN: usize = 6;

/// Display width in cells.
pub fn text_width(s: &str) -> usize {
    width(s)
}

fn width(s: &str) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
    s.graphemes(true).map(|g| g.width().clamp(1, 2)).sum()
}

/// Blend `a` towards `b` by `t` (0..1).
pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Rgb { r: f(a.r, b.r), g: f(a.g, b.g), b: f(a.b, b.b) }
}

/// One label per tab that fits, in order (index = tab index). Titles are
/// truncated so all tabs share the row.
pub fn tab_bar(titles: &[String], active: usize, x0: f32, y: f32, cols: usize, cw: f32, theme: &Theme) -> Vec<Label> {
    let n = titles.len().max(1);
    let cap = (cols / n).clamp(TAB_MIN, TAB_MAX);
    let mut col = 0;
    let mut out = Vec::new();
    for (i, title) in titles.iter().enumerate() {
        let text = format!(" {} {} ", i + 1, title);
        let w = width(&text).min(cap);
        if col + w > cols {
            break;
        }
        let on = i == active;
        out.push(Label {
            x: x0 + col as f32 * cw,
            y,
            cols: w,
            text,
            fg: if on { theme.bg } else { theme.dim },
            bg: Some(if on { theme.accent } else { theme.bar }),
            bold: on,
        });
        col += w + 1;
    }
    out
}

/// Index of the tab label under x, if any.
pub fn tab_at(labels: &[Label], x: f32, cw: f32) -> Option<usize> {
    labels.iter().position(|l| x >= l.x && x < l.x + l.cols as f32 * cw)
}

/// Mode badge followed by `keys label` pairs, cut off at `cols`.
pub fn hint_bar(mode: &str, hints: &[(String, &str)], x0: f32, y: f32, cols: usize, cw: f32, theme: &Theme) -> Vec<Label> {
    let mut out = Vec::new();
    let badge = format!(" {mode} ");
    let mut col = width(&badge).min(cols);
    out.push(Label { x: x0, y, cols: col, text: badge, fg: theme.bg, bg: Some(theme.accent), bold: true });
    col += 1;
    for (keys, label) in hints {
        let (kw, lw) = (width(keys), width(label));
        if col + kw + 1 + lw > cols {
            break;
        }
        out.push(Label { x: x0 + col as f32 * cw, y, cols: kw, text: keys.clone(), fg: theme.accent, bg: None, bold: true });
        col += kw + 1;
        out.push(Label { x: x0 + col as f32 * cw, y, cols: lw, text: label.to_string(), fg: theme.fg, bg: None, bold: false });
        col += lw + 2;
    }
    out
}

/// The input line: badge, text, status. Returns the labels and the caret's
/// x position (`caret_cols` = width of the text before the caret).
#[allow(clippy::too_many_arguments)]
pub fn input_bar(
    badge: &str,
    text: &str,
    caret_cols: usize,
    status: &str,
    x0: f32,
    y: f32,
    cols: usize,
    cw: f32,
    theme: &Theme,
) -> (Vec<Label>, f32) {
    let badge = format!(" {badge} ");
    let bw = width(&badge).min(cols);
    let mut out = vec![Label { x: x0, y, cols: bw, text: badge, fg: theme.bg, bg: Some(theme.accent), bold: true }];
    let start = bw + 1;
    let room = cols.saturating_sub(start);
    // Keep the caret in view: drop text from the left if needed.
    let (shown, caret) = if caret_cols < room {
        (text.to_string(), caret_cols)
    } else {
        use unicode_segmentation::UnicodeSegmentation;
        let mut skip = caret_cols + 1 - room;
        let mut s = String::new();
        for g in text.graphemes(true) {
            if skip > 0 {
                skip = skip.saturating_sub(width(g));
                continue;
            }
            s.push_str(g);
        }
        (s, room.saturating_sub(1))
    };
    out.push(Label { x: x0 + start as f32 * cw, y, cols: room, text: shown.clone(), fg: theme.fg, bg: None, bold: false });
    let after = start + width(&shown) + 2;
    if !status.is_empty() && after + width(status) <= cols {
        out.push(Label {
            x: x0 + after as f32 * cw,
            y,
            cols: width(status),
            text: status.to_string(),
            fg: theme.dim,
            bg: None,
            bold: false,
        });
    }
    (out, x0 + (start + caret) as f32 * cw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        let c = |v| Rgb { r: v, g: v, b: v };
        Theme { fg: c(200), bg: c(0), accent: c(100), bar: c(30), dim: c(150) }
    }

    #[test]
    fn tabs_share_the_row() {
        let titles: Vec<String> = ["zsh", "a very long title for a tab that goes on", "nvim"].map(String::from).to_vec();
        let l = tab_bar(&titles, 1, 0.0, 0.0, 60, 10.0, &theme());
        assert_eq!(l.len(), 3);
        assert_eq!(l[0].text, " 1 zsh ");
        assert_eq!(l[0].cols, 7);
        assert_eq!(l[1].cols, 20); // 60 / 3
        assert!(l[1].bold && !l[0].bold);
        assert_eq!(l[1].x, 80.0);
        assert_eq!(tab_at(&l, 85.0, 10.0), Some(1));
        assert_eq!(tab_at(&l, 75.0, 10.0), None); // gap
        // Too narrow: tabs that do not fit are left out.
        assert_eq!(tab_bar(&titles, 0, 0.0, 0.0, 10, 10.0, &theme()).len(), 1);
    }

    #[test]
    fn hints_truncate() {
        let hints = vec![("hjkl".to_string(), "focus"), ("x".to_string(), "close")];
        let l = hint_bar("PANE", &hints, 0.0, 0.0, 80, 10.0, &theme());
        assert_eq!(l.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), [" PANE ", "hjkl", "focus", "x", "close"]);
        assert_eq!(l[1].x, 70.0);
        let l = hint_bar("PANE", &hints, 0.0, 0.0, 18, 10.0, &theme());
        assert_eq!(l.len(), 3);
        let (l, caret) = input_bar("SEARCH", "foo", 3, "1/2", 0.0, 0.0, 40, 10.0, &theme());
        assert_eq!(l.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), [" SEARCH ", "foo", "1/2"]);
        assert_eq!(caret, 120.0);
        // Long text scrolls so the caret stays visible.
        let (l, caret) = input_bar("S", "abcdefghij", 10, "", 0.0, 0.0, 10, 10.0, &theme());
        assert_eq!((l[1].text.as_str(), caret), ("fghij", 90.0));
        assert_eq!(mix(Rgb { r: 0, g: 0, b: 0 }, Rgb { r: 200, g: 100, b: 0 }, 0.5), Rgb { r: 100, g: 50, b: 0 });
    }
}
