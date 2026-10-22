//! Git status markers from `git status --porcelain=v1 -z --ignored`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A file's state, in rising priority for directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mark {
    Ignored,
    Untracked,
    Staged,
    Modified,
}

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub root: PathBuf,
    /// Listed paths, and directories holding changed files.
    marks: HashMap<PathBuf, Mark>,
}

impl Status {
    /// Parse the output of `git status --porcelain=v1 -z` run in `root`.
    pub fn parse(root: &Path, out: &[u8]) -> Status {
        let mut marks: HashMap<PathBuf, Mark> = HashMap::new();
        let mut it = out.split(|&b| b == 0);
        while let Some(rec) = it.next() {
            if rec.len() < 4 {
                continue;
            }
            let (x, y) = (rec[0], rec[1]);
            let path = String::from_utf8_lossy(&rec[3..]);
            // Renames and copies are followed by the old path.
            if matches!(x, b'R' | b'C') {
                it.next();
            }
            let mark = match (x, y) {
                (b'!', b'!') => Mark::Ignored,
                (b'?', b'?') => Mark::Untracked,
                (_, b' ') => Mark::Staged,
                _ => Mark::Modified,
            };
            let p = root.join(path.trim_end_matches('/'));
            if mark != Mark::Ignored {
                // Parents show the strongest change below them.
                for a in p.ancestors().skip(1).take_while(|a| a.starts_with(root) && *a != root) {
                    let m = marks.entry(a.to_path_buf()).or_insert(mark);
                    *m = (*m).max(mark);
                }
            }
            let m = marks.entry(p).or_insert(mark);
            *m = (*m).max(mark);
        }
        Status { root: root.to_path_buf(), marks }
    }

    pub fn mark(&self, path: &Path) -> Option<Mark> {
        if let Some(m) = self.marks.get(path) {
            return Some(*m);
        }
        // Inside an ignored or untracked directory (listed once).
        path.ancestors()
            .skip(1)
            .take_while(|a| a.starts_with(&self.root) && *a != self.root)
            .find_map(|a| self.marks.get(a).filter(|m| matches!(m, Mark::Ignored | Mark::Untracked)))
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain() {
        let out = b" M src/a.rs\0A  src/b.rs\0?? new/\0!! target/\0R  c.rs\0old.rs\0";
        let s = Status::parse(Path::new("/r"), out);
        let m = |p: &str| s.mark(Path::new(p));
        assert_eq!(m("/r/src/a.rs"), Some(Mark::Modified));
        assert_eq!(m("/r/src/b.rs"), Some(Mark::Staged));
        assert_eq!(m("/r/src"), Some(Mark::Modified));
        assert_eq!(m("/r/new"), Some(Mark::Untracked));
        assert_eq!(m("/r/new/x/y"), Some(Mark::Untracked));
        assert_eq!(m("/r/target/debug"), Some(Mark::Ignored));
        assert_eq!(m("/r/c.rs"), Some(Mark::Staged));
        assert_eq!(m("/r/old.rs"), None);
        assert_eq!(m("/r/clean.rs"), None);
        assert_eq!(m("/r"), None);
    }
}
