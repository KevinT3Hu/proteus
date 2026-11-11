//! Hint mode: find URLs, paths, hashes... on screen and label them.

use std::ops::Range;

use regex::Regex;

use crate::blocks::Point;
use crate::grid::{Grid, Line};
use crate::text;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintKind {
    Url,
    Path,
    GitHash,
    Ipv4,
    Ipv6,
    Uuid,
    HexColor,
    /// User-defined pattern (from Lua).
    Custom(String),
}

#[derive(Debug, Clone)]
pub struct HintPattern {
    pub kind: HintKind,
    /// If the regex has a capture group 1, that group is the hint text.
    pub regex: Regex,
}

impl HintPattern {
    pub fn new(kind: HintKind, pattern: &str) -> Result<Self, regex::Error> {
        Ok(Self { kind, regex: Regex::new(pattern)? })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub kind: HintKind,
    pub text: String,
    pub start: Point,
    /// Inclusive.
    pub end: Point,
    pub label: String,
}

pub const DEFAULT_ALPHABET: &str = "asdfghjkl";

/// Hosts recognized as URLs without a scheme: `www.` names, names ending
/// in a common top-level domain, `localhost[:port]`, `IPv4:port`. TLDs
/// that are also common file extensions (`.rs`, `.sh`, `.py`, `.md`,
/// `.cc`, `.in`, `.so`, `.app`, `.zip`...) are left out so file names stay
/// paths.
const BARE_HOST: &str = concat!(
    r"www\.[\w-]+(?:\.[\w-]+)+(?::\d{1,5})?",
    r"|(?:[A-Za-z0-9](?:[\w-]*[A-Za-z0-9])?\.)+(?i:com|org|net|edu|gov|io|dev|cn|co|me|info|xyz|tech|cloud|site|top|uk|de|jp|fr|ru|eu|us|ca|au|kr|tw|hk|nl|ch|se|it|es|br)\b(?::\d{1,5})?",
    r"|localhost(?::\d{1,5})?\b",
    r"|\d{1,3}(?:\.\d{1,3}){3}:\d{1,5}\b",
);

/// URL to open for a URL hint: scheme-less ones get `https://`
/// (`http://` for localhost and IP addresses).
pub fn url_with_scheme(text: &str) -> String {
    if text.contains("://") {
        return text.to_string();
    }
    let host = text.split([':', '/', '?', '#']).next().unwrap_or("");
    let local = host == "localhost" || host.parse::<std::net::Ipv4Addr>().is_ok();
    format!("{}://{text}", if local { "http" } else { "https" })
}

/// Built-in patterns in priority order: earlier patterns win on overlap.
pub fn builtin_patterns() -> Vec<HintPattern> {
    const OCTET: &str = r"(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)";
    let ipv4 = format!(r"\b(?:{OCTET}\.){{3}}{OCTET}(?::\d{{1,5}})?\b");
    let bare = format!(r#"(?:^|[^\w@./:~-])((?:{BARE_HOST})(?:[/?#][^\s<>"'`]*)?)"#);
    let table: [(HintKind, &str); 8] = [
        (HintKind::Url, r#"\b(?:https?|ftp|file|ssh|git)://[^\s<>"'`]+"#),
        (HintKind::Url, &bare),
        (HintKind::Uuid, r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b"),
        (HintKind::Ipv4, &ipv4),
        (
            HintKind::Ipv6,
            r"(?:\b(?:[0-9a-fA-F]{1,4}:){7}[0-9a-fA-F]{1,4}\b|(?:\b(?:[0-9a-fA-F]{1,4}:){1,7}|::)(?::|(?::?[0-9a-fA-F]{1,4}){1,7})\b)",
        ),
        (HintKind::HexColor, r"(?:^|[^\w#&])(#(?:[0-9a-fA-F]{6}|[0-9a-fA-F]{3}))\b"),
        (
            HintKind::Path,
            r"(?:(?:~|\.{1,2})?/)?(?:[\w.@+-]+/)+[\w.@+-]*(?::\d+){0,2}|[\w.@+-]+\.[A-Za-z]\w*:\d+(?::\d+)?",
        ),
        (HintKind::GitHash, r"\b[0-9a-f]{7,40}\b"),
    ];
    table.into_iter().map(|(k, p)| HintPattern::new(k, p).expect("valid builtin pattern")).collect()
}

/// Post-filter and trim a raw match. Returns the (possibly shortened) byte range.
fn refine(kind: &HintKind, s: &str, range: Range<usize>) -> Option<Range<usize>> {
    let t = &s[range.clone()];
    match kind {
        HintKind::Url => {
            // Open minus close brackets in `t[..end]`; only closers are trimmed.
            let balance =
                |open: u8, close: u8| t.bytes().fold(0isize, |n, b| n + (b == open) as isize - (b == close) as isize);
            let (mut parens, mut brackets) = (balance(b'(', b')'), balance(b'[', b']'));
            let mut end = t.len();
            while let Some(last) = t[..end].chars().next_back() {
                let trim = match last {
                    ')' => parens < 0,
                    ']' => brackets < 0,
                    _ => ".,:;!?'\"".contains(last),
                };
                if !trim {
                    break;
                }
                match last {
                    ')' => parens += 1,
                    ']' => brackets += 1,
                    _ => {}
                }
                end -= last.len_utf8();
            }
            let min = t.find("://").map_or(0, |i| i + 3);
            (end > min).then(|| range.start..range.start + end)
        }
        HintKind::GitHash => {
            let ok = t.bytes().any(|b| b.is_ascii_digit()) && t.bytes().any(|b| b.is_ascii_alphabetic());
            ok.then_some(range)
        }
        HintKind::Ipv6 => {
            let ok = t.matches(':').count() >= 2 && t.bytes().any(|b| b.is_ascii_digit());
            ok.then_some(range)
        }
        HintKind::Path => {
            (t.trim_end_matches(['.', ':']).len() > 1).then(|| range.start..range.start + t.trim_end_matches('.').len())
        }
        _ => Some(range),
    }
}

/// Find non-overlapping hints in the logical lines intersecting `lines` and
/// assign labels from `alphabet`.
pub fn find_hints(grid: &Grid, lines: Range<Line>, patterns: &[HintPattern], alphabet: &str) -> Vec<Hint> {
    let mut found: Vec<(Point, Point, HintKind, String)> = Vec::new();
    let end = lines.end.min(grid.end_line());
    let mut l = text::logical_start(grid, lines.start.max(grid.first_line()));
    while l < end {
        let ll = text::logical_line(grid, l);
        let mut taken: Vec<Range<usize>> = Vec::new();
        for pat in patterns {
            for caps in pat.regex.captures_iter(&ll.text) {
                let Some(m) = caps.get(1).or_else(|| caps.get(0)) else { continue };
                let Some(r) = refine(&pat.kind, &ll.text, m.range()) else { continue };
                if r.is_empty() || taken.iter().any(|t| t.start < r.end && r.start < t.end) {
                    continue;
                }
                let (Some(s), Some(e)) = (ll.point_at(r.start), ll.point_before(r.end)) else { continue };
                // Keep only hints that start inside the requested lines.
                if s.line < lines.start || s.line >= end {
                    continue;
                }
                found.push((s, e, pat.kind.clone(), ll.text[r.clone()].to_string()));
                taken.push(r);
            }
        }
        l = ll.end.max(l + 1);
    }
    found.sort_by_key(|h| h.0);
    let labels = labels(found.len(), alphabet);
    found
        .into_iter()
        .zip(labels)
        .map(|((start, end, kind, text), label)| Hint { kind, text, start, end, label })
        .collect()
}

/// `n` labels of equal length over `alphabet` (so none is a prefix of another).
pub fn labels(n: usize, alphabet: &str) -> Vec<String> {
    let chars: Vec<char> = alphabet.chars().collect();
    if n == 0 || chars.len() < 2 {
        return Vec::new();
    }
    let mut len = 1;
    while chars.len().pow(len as u32) < n {
        len += 1;
    }
    (0..n)
        .map(|mut i| {
            let mut s = vec![' '; len];
            for slot in s.iter_mut().rev() {
                *slot = chars[i % chars.len()];
                i /= chars.len();
            }
            s.into_iter().collect()
        })
        .collect()
}

/// A file path with an optional `:line[:col]` suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: String,
    pub line: Option<u32>,
    pub col: Option<u32>,
}

pub fn parse_location(s: &str) -> Location {
    let mut parts = s.rsplitn(3, ':').collect::<Vec<_>>();
    parts.reverse();
    let nums: Vec<Option<u32>> = parts.iter().map(|p| p.parse().ok()).collect();
    match (parts.len(), nums.as_slice()) {
        (3, [_, Some(l), Some(c)]) => Location { path: parts[0].into(), line: Some(*l), col: Some(*c) },
        (3, [_, _, Some(l)]) => Location { path: format!("{}:{}", parts[0], parts[1]), line: Some(*l), col: None },
        (2, [_, Some(l)]) => Location { path: parts[0].into(), line: Some(*l), col: None },
        _ => Location { path: s.into(), line: None, col: None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location() {
        assert_eq!(parse_location("src/a.rs:12:5"), Location { path: "src/a.rs".into(), line: Some(12), col: Some(5) });
        assert_eq!(parse_location("src/a.rs:12"), Location { path: "src/a.rs".into(), line: Some(12), col: None });
        assert_eq!(parse_location("/tmp/x"), Location { path: "/tmp/x".into(), line: None, col: None });
    }

    #[test]
    fn bare_urls() {
        let pats = builtin_patterns();
        let bare = &pats[1].regex;
        let find = |s: &str| {
            bare.captures(s).and_then(|c| {
                let m = c.get(1)?;
                refine(&HintKind::Url, s, m.range()).map(|r| s[r].to_string())
            })
        };
        assert_eq!(find("see example.com."), Some("example.com".into()));
        assert_eq!(find("go to www.rust-lang.org/learn, now"), Some("www.rust-lang.org/learn".into()));
        assert_eq!(find("(github.com/a/b)"), Some("github.com/a/b".into()));
        assert_eq!(find("at localhost:8080/api?x=1"), Some("localhost:8080/api?x=1".into()));
        assert_eq!(find("on 10.0.0.1:3000/"), Some("10.0.0.1:3000/".into()));
        assert_eq!(find("baidu.COM/s"), Some("baidu.COM/s".into()));
        // File names, paths, mail addresses, plain IPs are not URLs.
        for s in [
            "src/main.rs",
            "build.sh",
            "a/example.com/x",
            "./example.com",
            "me@example.com",
            "10.0.0.1",
            "foo.company",
            "lib.so",
        ] {
            assert_eq!(find(s), None, "{s}");
        }
        assert_eq!(url_with_scheme("example.com/x"), "https://example.com/x");
        assert_eq!(url_with_scheme("localhost:8080"), "http://localhost:8080");
        assert_eq!(url_with_scheme("10.0.0.1:3000/a"), "http://10.0.0.1:3000/a");
        assert_eq!(url_with_scheme("ftp://x"), "ftp://x");
    }

    #[test]
    fn label_gen() {
        assert_eq!(labels(3, "ab"), vec!["aa", "ab", "ba"]);
        assert_eq!(labels(2, "asd"), vec!["a", "s"]);
    }
}
