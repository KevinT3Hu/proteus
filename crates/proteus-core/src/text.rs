//! Text helpers: character width, DEC charsets, grid text extraction, URL decoding.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::blocks::Point;
use crate::event::Cwd;
use crate::grid::{Grid, Line};

/// Display width of a single character (0, 1 or 2).
pub(crate) fn char_width(c: char, ambiguous_wide: bool) -> usize {
    let w = if ambiguous_wide { c.width_cjk() } else { c.width() };
    w.unwrap_or(0).min(2)
}

/// Display width of a grapheme cluster, capped at 2.
pub(crate) fn str_width(s: &str, ambiguous_wide: bool) -> usize {
    let w = if ambiguous_wide { s.width_cjk() } else { s.width() };
    w.min(2)
}

/// DEC Special Graphics (line drawing) translation for `ESC ( 0`.
pub(crate) fn dec_special(c: char) -> char {
    match c {
        '_' => '\u{a0}',
        '`' => '◆',
        'a' => '▒',
        'b' => '␉',
        'c' => '␌',
        'd' => '␍',
        'e' => '␊',
        'f' => '°',
        'g' => '±',
        'h' => '␤',
        'i' => '␋',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        c => c,
    }
}

/// Extract text between two points (both inclusive). Soft-wrapped rows are
/// joined without a newline; trailing blanks of hard lines are trimmed. With
/// `rect`, extracts the rectangle spanned by the two points.
pub fn extract(grid: &Grid, start: Point, end: Point, rect: bool) -> String {
    let mut out = String::new();
    if end.line < start.line || (!rect && end < start) {
        return out;
    }
    let cols = grid.cols();
    let (lc, rc) = (start.col.min(end.col), start.col.max(end.col));
    for line in start.line..=end.line {
        let Some(row) = grid.abs_row(line) else { continue };
        let (a, b) = if rect {
            (lc, rc + 1)
        } else {
            (if line == start.line { start.col } else { 0 }, if line == end.line { end.col + 1 } else { cols })
        };
        let text = row.text(a..b);
        let soft = !rect && row.wrapped && line != end.line && b >= cols;
        if soft {
            out.push_str(&text);
        } else {
            out.push_str(text.trim_end_matches(' '));
            if line != end.line {
                out.push('\n');
            }
        }
    }
    out
}

/// A logical (unwrapped) line with a byte → grid point index.
#[derive(Debug, Clone)]
pub struct LogicalLine {
    pub text: String,
    /// `(byte offset, point)` for every character, in order.
    pub index: Vec<(usize, Point)>,
    pub start: Line,
    /// One past the last row of this logical line.
    pub end: Line,
}

impl LogicalLine {
    /// Point of the character containing byte `byte`.
    pub fn point_at(&self, byte: usize) -> Option<Point> {
        let i = self.index.partition_point(|(b, _)| *b <= byte).checked_sub(1)?;
        Some(self.index[i].1)
    }

    /// Point of the last character starting before byte `end` (for exclusive match ends).
    pub fn point_before(&self, end: usize) -> Option<Point> {
        self.point_at(end.checked_sub(1)?)
    }
}

/// First row of the logical line containing `line`.
pub fn logical_start(grid: &Grid, mut line: Line) -> Line {
    while line > grid.first_line() && grid.abs_row(line - 1).is_some_and(|r| r.wrapped) {
        line -= 1;
    }
    line
}

/// Build the logical line starting at `line`. Trailing blanks of the last row are dropped.
pub fn logical_line(grid: &Grid, line: Line) -> LogicalLine {
    let mut text = String::new();
    let mut index = Vec::new();
    let mut l = line;
    while let Some(row) = grid.abs_row(l) {
        let len = if row.wrapped { row.len() } else { row.content_len() };
        for (col, cell) in row.cells[..len].iter().enumerate() {
            if cell.is_spacer() {
                continue;
            }
            index.push((text.len(), Point::new(l, col)));
            cell.push_text(&mut text);
        }
        l += 1;
        if !row.wrapped {
            break;
        }
    }
    LogicalLine { text, index, start: line, end: l }
}

pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(v) = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse an OSC 7 URL (`file://host/path` or `kitty-shell-cwd://host/path`).
pub(crate) fn parse_cwd_url(s: &str) -> Option<Cwd> {
    let (rest, encoded) = if let Some(r) = s.strip_prefix("file://") {
        (r, true)
    } else {
        (s.strip_prefix("kitty-shell-cwd://")?, false)
    };
    let slash = rest.find('/')?;
    let (host, path) = rest.split_at(slash);
    Some(Cwd {
        host: (!host.is_empty()).then(|| host.to_string()),
        path: if encoded { percent_decode(path) } else { path.to_string() },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode() {
        assert_eq!(percent_decode("/a%20b/%E4%B8%AD"), "/a b/中");
        assert_eq!(percent_decode("100%"), "100%");
        let c = parse_cwd_url("file://box/home/u/my%20dir").unwrap();
        assert_eq!(c.host.as_deref(), Some("box"));
        assert_eq!(c.path, "/home/u/my dir");
        assert_eq!(parse_cwd_url("file:///tmp").unwrap().host, None);
    }

    #[test]
    fn widths() {
        assert_eq!(char_width('a', false), 1);
        assert_eq!(char_width('中', false), 2);
        assert_eq!(char_width('\u{301}', false), 0);
        assert_eq!(char_width('é', false), 1);
        assert_eq!(char_width('±', false), 1);
        assert_eq!(char_width('±', true), 2);
    }
}
