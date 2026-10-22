//! Directory listings, sorting and matching (no host calls: testable).

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    /// A directory, or a symlink to one.
    pub dir: bool,
    pub link: bool,
    pub size: u64,
    /// Seconds since the epoch.
    pub mtime: i64,
}

impl Entry {
    pub fn hidden(&self) -> bool {
        self.name.starts_with('.')
    }

    fn ext(&self) -> &str {
        match self.name.rsplit_once('.') {
            Some((stem, e)) if !stem.is_empty() && !self.dir => e,
            _ => "",
        }
    }
}

/// The entries of `dir`, unsorted. Unreadable entries are skipped.
pub fn list(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let Ok(e) = e else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(meta) = e.metadata() else { continue };
        let link = meta.file_type().is_symlink();
        // Links show what they point at (a broken one stays a file).
        let target = if link { std::fs::metadata(e.path()).unwrap_or(meta) } else { meta };
        let mtime =
            target.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as i64);
        out.push(Entry { name, dir: target.is_dir(), link, size: target.len(), mtime });
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Mtime,
    Size,
    Ext,
}

impl SortKey {
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Mtime => "mtime",
            SortKey::Size => "size",
            SortKey::Ext => "ext",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub dirs_first: bool,
    pub reverse: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Sort { key: SortKey::Name, dirs_first: true, reverse: false }
    }
}

impl Sort {
    pub fn apply(&self, v: &mut [Entry]) {
        v.sort_by(|a, b| {
            if self.dirs_first && a.dir != b.dir {
                return b.dir.cmp(&a.dir);
            }
            let o = match self.key {
                SortKey::Name => Ordering::Equal,
                // Newest and biggest first.
                SortKey::Mtime => b.mtime.cmp(&a.mtime),
                SortKey::Size => b.size.cmp(&a.size),
                SortKey::Ext => natural(a.ext(), b.ext()),
            }
            .then_with(|| natural(&a.name, &b.name));
            if self.reverse { o.reverse() } else { o }
        });
    }
}

/// Natural order: case-insensitive, runs of digits compared as numbers
/// (`a2` < `a10`); ties broken by the exact text.
pub fn natural(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(c) = it.next_if(char::is_ascii_digit) {
                        s.push(c);
                    }
                    s
                };
                let (m, n) = (take(&mut x), take(&mut y));
                let (mt, nt) = (m.trim_start_matches('0'), n.trim_start_matches('0'));
                let o = mt.len().cmp(&nt.len()).then_with(|| mt.cmp(nt));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(c), Some(d)) => {
                let o = c.to_lowercase().cmp(d.to_lowercase());
                if o != Ordering::Equal {
                    return o;
                }
                x.next();
                y.next();
            }
        }
    }
}

/// Fuzzy match: the pattern's characters appear in order. Case-insensitive
/// unless the pattern has an uppercase letter.
pub fn fuzzy(pattern: &str, name: &str) -> bool {
    let exact = pattern.chars().any(char::is_uppercase);
    let fold = |c: char| if exact { c } else { c.to_lowercase().next().unwrap_or(c) };
    let mut it = name.chars().map(fold);
    pattern.chars().map(fold).all(|p| it.any(|c| c == p))
}

/// `1.2K`, `34M`, ...
pub fn human(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n}B")
    } else if v < 10.0 {
        format!("{v:.1}{}", UNITS[u])
    } else {
        format!("{v:.0}{}", UNITS[u])
    }
}

/// `p` with the home directory as `~`.
pub fn tilde(p: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| p.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => p.display().to_string(),
    }
}

pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// `text` quoted for a POSIX shell if it needs it.
pub fn shell_quote(text: &str) -> String {
    let safe = |c: char| c.is_alphanumeric() || "-_./~:@%+=,".contains(c);
    if !text.is_empty() && text.chars().all(safe) && !text.starts_with('~') {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(name: &str, dir: bool, size: u64, mtime: i64) -> Entry {
        Entry { name: name.into(), dir, link: false, size, mtime }
    }

    #[test]
    fn natural_order() {
        let mut v = vec!["a10", "a2", "B", "a", "a02"];
        v.sort_by(|a, b| natural(a, b));
        assert_eq!(v, ["a", "a02", "a2", "a10", "B"]);
    }

    #[test]
    fn sorts() {
        let mut v = vec![e("b.rs", false, 5, 1), e("z", true, 0, 0), e("a.txt", false, 9, 3), e("c.md", false, 1, 2)];
        let names = |v: &[Entry]| v.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        Sort::default().apply(&mut v);
        assert_eq!(names(&v), ["z", "a.txt", "b.rs", "c.md"]);
        Sort { key: SortKey::Size, dirs_first: false, reverse: false }.apply(&mut v);
        assert_eq!(names(&v), ["a.txt", "b.rs", "c.md", "z"]);
        Sort { key: SortKey::Mtime, dirs_first: true, reverse: true }.apply(&mut v);
        // Reversed: oldest first, directories still first.
        assert_eq!(names(&v), ["z", "b.rs", "c.md", "a.txt"]);
        Sort { key: SortKey::Ext, dirs_first: true, reverse: false }.apply(&mut v);
        assert_eq!(names(&v), ["z", "c.md", "b.rs", "a.txt"]);
    }

    #[test]
    fn matching_and_text() {
        assert!(fuzzy("crs", "Cargo.rs"));
        assert!(!fuzzy("C", "cargo"));
        assert!(fuzzy("", "x"));
        assert_eq!(human(512), "512B");
        assert_eq!(human(1536), "1.5K");
        assert_eq!(human(50 << 20), "50M");
        let h = Path::new("/home/u");
        assert_eq!(tilde(Path::new("/home/u/src"), Some(h)), "~/src");
        assert_eq!(tilde(h, Some(h)), "~");
        assert_eq!(tilde(Path::new("/etc"), Some(h)), "/etc");
        assert_eq!(shell_quote("/a b/it's"), r"'/a b/it'\''s'");
        assert_eq!(shell_quote("/usr/bin"), "/usr/bin");
    }
}
