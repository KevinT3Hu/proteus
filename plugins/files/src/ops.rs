//! File operations that run in steps (so one plugin call stays short):
//! copies and moves with conflicts, bulk rename plans, trash restore.
//! Plain `std::fs`, no host calls (testable).

use std::collections::{HashSet, VecDeque};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Copy,
    Move,
}

/// What to do when the destination exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Overwrite,
    Skip,
    Rename,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Working,
    /// Waiting for `resolve`: this destination exists.
    Conflict(PathBuf),
    Done,
}

struct Item {
    src: PathBuf,
    dst: PathBuf,
    /// Given by the user (not found inside a copied directory).
    top: bool,
}

struct Open {
    from: File,
    to: File,
    dst: PathBuf,
}

const CHUNK: usize = 1 << 20;

/// A copy or move of (source, destination) pairs.
pub struct Transfer {
    pub kind: OpKind,
    queue: VecDeque<Item>,
    open: Option<Open>,
    /// The choice for all later conflicts.
    all: Option<Choice>,
    /// The choice for the conflict now waiting.
    once: Option<Choice>,
    pub total: u64,
    pub done: u64,
    pub errors: Vec<String>,
    /// Top-level pairs that were carried out (for undo).
    pub finished: Vec<(PathBuf, PathBuf)>,
    /// Moves that had to copy: sources removed at the end.
    remove: Vec<PathBuf>,
}

/// Bytes below `p` (not following links), stopping at a large count.
fn size_of(p: &Path, budget: &mut usize) -> u64 {
    let Ok(m) = std::fs::symlink_metadata(p) else { return 0 };
    if !m.is_dir() {
        return m.len();
    }
    let mut n = 0;
    for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        n += size_of(&e.path(), budget);
    }
    n
}

/// `dst` or the first free `name (n).ext` next to it.
pub fn unique(dst: &Path) -> PathBuf {
    if std::fs::symlink_metadata(dst).is_err() {
        return dst.to_path_buf();
    }
    let name = dst.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && !dst.is_dir() => (s.to_string(), format!(".{e}")),
        _ => (name.clone(), String::new()),
    };
    (1..)
        .map(|n| dst.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|p| std::fs::symlink_metadata(p).is_err())
        .expect("a free name")
}

fn remove_any(p: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
        Ok(_) => std::fs::remove_file(p),
        Err(e) => Err(e),
    }
}

impl Transfer {
    pub fn new(kind: OpKind, pairs: Vec<(PathBuf, PathBuf)>) -> Transfer {
        let mut budget = 200_000;
        let total = if kind == OpKind::Copy { pairs.iter().map(|(s, _)| size_of(s, &mut budget)).sum() } else { 0 };
        Transfer {
            kind,
            queue: pairs.into_iter().map(|(src, dst)| Item { src, dst, top: true }).collect(),
            open: None,
            all: None,
            once: None,
            total,
            done: 0,
            errors: Vec::new(),
            finished: Vec::new(),
            remove: Vec::new(),
        }
    }

    /// Answer the waiting conflict, for it or for all.
    pub fn resolve(&mut self, c: Choice, all: bool) {
        if all {
            self.all = Some(c);
        }
        self.once = Some(c);
    }

    /// Stop: the file being written is removed.
    pub fn cancel(&mut self) {
        if let Some(o) = self.open.take() {
            drop(o.to);
            let _ = std::fs::remove_file(&o.dst);
        }
        self.queue.clear();
        self.remove.clear();
    }

    /// Work for about `budget`.
    pub fn step(&mut self, budget: Duration) -> Step {
        let start = Instant::now();
        while start.elapsed() < budget {
            if let Some(o) = self.open.as_mut() {
                let mut buf = vec![0; CHUNK];
                match o.from.read(&mut buf) {
                    Ok(0) => self.open = None,
                    Ok(n) => {
                        if let Err(e) = o.to.write_all(&buf[..n]) {
                            self.errors.push(format!("{}: {e}", o.dst.display()));
                            let dst = o.dst.clone();
                            self.open = None;
                            let _ = std::fs::remove_file(dst);
                        } else {
                            self.done += n as u64;
                        }
                    }
                    Err(e) => {
                        self.errors.push(format!("{}: {e}", o.dst.display()));
                        self.open = None;
                    }
                }
                continue;
            }
            let Some(item) = self.queue.pop_front() else {
                for p in std::mem::take(&mut self.remove).into_iter().rev() {
                    if let Err(e) = remove_any(&p) {
                        self.errors.push(format!("{}: {e}", p.display()));
                    }
                }
                return Step::Done;
            };
            if let Some(s) = self.item(item) {
                return s;
            }
        }
        Step::Working
    }

    /// Start one item; `Some` to stop stepping.
    fn item(&mut self, mut it: Item) -> Option<Step> {
        let err = |e: std::io::Error, p: &Path| format!("{}: {e}", p.display());
        let Ok(meta) = std::fs::symlink_metadata(&it.src) else {
            self.errors.push(format!("{}: gone", it.src.display()));
            return None;
        };
        if it.top {
            if meta.is_dir() && it.dst.starts_with(&it.src) {
                self.errors.push(format!("{}: cannot go into itself", it.src.display()));
                return None;
            }
            if it.src == it.dst {
                // Copying into its own directory: a new name.
                if self.kind == OpKind::Move {
                    return None;
                }
                it.dst = unique(&it.dst);
            } else if let Ok(dm) = std::fs::symlink_metadata(&it.dst) {
                let Some(c) = self.once.take().or(self.all) else {
                    let dst = it.dst.clone();
                    self.queue.push_front(it);
                    return Some(Step::Conflict(dst));
                };
                match c {
                    Choice::Skip => return None,
                    Choice::Rename => it.dst = unique(&it.dst),
                    // Directories merge; anything else is replaced.
                    Choice::Overwrite if dm.is_dir() && meta.is_dir() => {}
                    Choice::Overwrite => {
                        if let Err(e) = remove_any(&it.dst) {
                            self.errors.push(err(e, &it.dst));
                            return None;
                        }
                    }
                }
            }
            if self.kind == OpKind::Move && std::fs::symlink_metadata(&it.dst).is_err() {
                match std::fs::rename(&it.src, &it.dst) {
                    Ok(()) => {
                        self.finished.push((it.src, it.dst));
                        return None;
                    }
                    // Another file system: copy, then remove.
                    Err(_) => {
                        self.total += size_of(&it.src, &mut 200_000);
                        self.remove.push(it.src.clone());
                    }
                }
            } else if self.kind == OpKind::Move {
                self.remove.push(it.src.clone());
                self.total += size_of(&it.src, &mut 200_000);
            }
            self.finished.push((it.src.clone(), it.dst.clone()));
        }
        if meta.is_dir() {
            if let Err(e) = std::fs::create_dir(&it.dst)
                && e.kind() != std::io::ErrorKind::AlreadyExists
            {
                self.errors.push(err(e, &it.dst));
                return None;
            }
            let mut kids: Vec<_> = std::fs::read_dir(&it.src).into_iter().flatten().flatten().collect();
            kids.sort_by_key(|e| e.file_name());
            for k in kids.into_iter().rev() {
                let n = k.file_name();
                self.queue.push_front(Item { src: it.src.join(&n), dst: it.dst.join(&n), top: false });
            }
        } else if meta.file_type().is_symlink() {
            self.errors.push(format!("{}: links are not copied", it.src.display()));
            // A move must not remove what it could not copy.
            self.remove.retain(|r| !it.src.starts_with(r));
        } else {
            let from = match File::open(&it.src) {
                Ok(f) => f,
                Err(e) => {
                    self.errors.push(err(e, &it.src));
                    return None;
                }
            };
            match File::create(&it.dst) {
                Ok(to) => self.open = Some(Open { from, to, dst: it.dst }),
                Err(e) => self.errors.push(err(e, &it.dst)),
            }
        }
        None
    }

    /// 0..1, if the size is known.
    pub fn progress(&self) -> Option<f32> {
        (self.total > 0).then(|| (self.done as f64 / self.total as f64).min(1.0) as f32)
    }
}

/// Renames from an edited list of names (one per line, same order as
/// `names`): (old, new) pairs of changed names in `dir`.
pub fn rename_plan(dir: &Path, names: &[String], edited: &str) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let lines: Vec<&str> = edited.lines().collect();
    let lines: Vec<&str> = if lines.last() == Some(&"") { lines[..lines.len() - 1].to_vec() } else { lines };
    if lines.len() != names.len() {
        return Err(format!("{} names for {} files; nothing renamed", lines.len(), names.len()));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (old, new) in names.iter().zip(lines) {
        if new.is_empty() || new.contains('/') || new == "." || new == ".." || new.chars().any(char::is_control) {
            return Err(format!("bad name `{new}`; nothing renamed"));
        }
        if !seen.insert(new) {
            return Err(format!("`{new}` twice; nothing renamed"));
        }
        if new != old {
            out.push((dir.join(old), dir.join(new)));
        }
    }
    let sources: HashSet<&PathBuf> = out.iter().map(|p| &p.0).collect();
    for (_, to) in &out {
        if !sources.contains(to) && std::fs::symlink_metadata(to).is_ok() {
            return Err(format!("{} exists; nothing renamed", to.display()));
        }
    }
    Ok(out)
}

/// Carry out renames (also swaps and cycles: through temporary names).
/// Returns those done.
pub fn rename_all(pairs: &[(PathBuf, PathBuf)]) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let mut tmp = Vec::new();
    for (i, (from, _)) in pairs.iter().enumerate() {
        let t = from.with_file_name(format!(".proteus-rename-{i}-{}", std::process::id()));
        if let Err(e) = std::fs::rename(from, &t) {
            // Put back what was moved.
            for ((f, _), t) in pairs.iter().zip(&tmp) {
                let _ = std::fs::rename(t, f);
            }
            return Err(format!("{}: {e}", from.display()));
        }
        tmp.push(t);
    }
    let mut done = Vec::new();
    let mut err = None;
    for ((from, to), t) in pairs.iter().zip(&tmp) {
        match std::fs::rename(t, to) {
            Ok(()) => done.push((from.clone(), to.clone())),
            Err(e) => {
                let _ = std::fs::rename(t, from);
                err.get_or_insert(format!("{}: {e}", to.display()));
            }
        }
    }
    match err {
        Some(e) if done.is_empty() => Err(e),
        _ => Ok(done),
    }
}

/// Where the home trash put `path` (the newest), if it is there.
pub fn trashed(trash: &Path, path: &Path) -> Option<(PathBuf, PathBuf)> {
    let want = format!("Path={}", crate::preview::file_uri(path).trim_start_matches("file://"));
    let mut best: Option<(String, PathBuf, PathBuf)> = None;
    for e in std::fs::read_dir(trash.join("info")).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".trashinfo") else { continue };
        let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
        if !text.lines().any(|l| l == want) {
            continue;
        }
        let date = text.lines().find_map(|l| l.strip_prefix("DeletionDate=")).unwrap_or("").to_string();
        if best.as_ref().is_none_or(|b| date > b.0) {
            best = Some((date, trash.join("files").join(stem), e.path()));
        }
    }
    best.map(|b| (b.1, b.2))
}

/// Put a trashed file back.
pub fn restore(trash: &Path, path: &Path) -> Result<(), String> {
    let (file, info) = trashed(trash, path).ok_or_else(|| format!("{}: not in the trash", path.display()))?;
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(format!("{} exists again", path.display()));
    }
    std::fs::rename(&file, path).map_err(|e| format!("{}: {e}", path.display()))?;
    let _ = std::fs::remove_file(info);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("proteus-ops-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("a/sub")).unwrap();
        std::fs::create_dir_all(d.join("b")).unwrap();
        std::fs::write(d.join("a/x.txt"), "x".repeat(3 << 20)).unwrap();
        std::fs::write(d.join("a/sub/y"), "y").unwrap();
        d
    }

    fn run(t: &mut Transfer) -> Step {
        loop {
            match t.step(Duration::from_millis(5)) {
                Step::Working => {}
                s => return s,
            }
        }
    }

    #[test]
    fn copies_and_moves() {
        let d = tmp("cp");
        let mut t = Transfer::new(OpKind::Copy, vec![(d.join("a"), d.join("b/a"))]);
        assert_eq!(t.total, (3 << 20) + 1);
        assert_eq!(run(&mut t), Step::Done);
        assert_eq!(std::fs::read(d.join("b/a/sub/y")).unwrap(), b"y");
        assert_eq!(std::fs::metadata(d.join("b/a/x.txt")).unwrap().len(), 3 << 20);
        assert_eq!(t.progress(), Some(1.0));
        // Again: a conflict; rename gives "a (1)".
        let mut t = Transfer::new(OpKind::Copy, vec![(d.join("a"), d.join("b/a"))]);
        assert_eq!(run(&mut t), Step::Conflict(d.join("b/a")));
        t.resolve(Choice::Rename, false);
        assert_eq!(run(&mut t), Step::Done);
        assert!(d.join("b/a (1)/sub/y").exists());
        // Copy into the same directory makes a new name.
        let mut t = Transfer::new(OpKind::Copy, vec![(d.join("a/x.txt"), d.join("a/x.txt"))]);
        assert_eq!(run(&mut t), Step::Done);
        assert!(d.join("a/x (1).txt").exists());
        // Into itself is refused.
        let mut t = Transfer::new(OpKind::Copy, vec![(d.join("a"), d.join("a/sub/a"))]);
        assert_eq!(run(&mut t), Step::Done);
        assert_eq!(t.errors.len(), 1);
        // Move, overwrite.
        let mut t = Transfer::new(OpKind::Move, vec![(d.join("a/sub/y"), d.join("b/a/sub/y"))]);
        assert!(matches!(run(&mut t), Step::Conflict(_)));
        t.resolve(Choice::Overwrite, true);
        assert_eq!(run(&mut t), Step::Done);
        assert!(!d.join("a/sub/y").exists() && d.join("b/a/sub/y").exists());
        assert_eq!(t.finished, [(d.join("a/sub/y"), d.join("b/a/sub/y"))]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn cancels() {
        let d = tmp("cancel");
        let mut t = Transfer::new(OpKind::Copy, vec![(d.join("a/x.txt"), d.join("b/x.txt"))]);
        t.step(Duration::ZERO);
        t.step(Duration::from_micros(1));
        t.cancel();
        assert!(!d.join("b/x.txt").exists());
        assert_eq!(t.step(Duration::from_millis(5)), Step::Done);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bulk_rename() {
        let d = tmp("bulk");
        std::fs::write(d.join("b/1"), "one").unwrap();
        std::fs::write(d.join("b/2"), "two").unwrap();
        let names = vec!["1".to_string(), "2".to_string()];
        assert!(rename_plan(&d.join("b"), &names, "1\n").is_err());
        assert!(rename_plan(&d.join("b"), &names, "x\nx\n").is_err());
        assert!(rename_plan(&d.join("b"), &names, "a/b\n2\n").is_err());
        // A swap.
        let plan = rename_plan(&d.join("b"), &names, "2\n1\n").unwrap();
        assert_eq!(rename_all(&plan).unwrap().len(), 2);
        assert_eq!(std::fs::read_to_string(d.join("b/1")).unwrap(), "two");
        assert_eq!(rename_plan(&d.join("b"), &names, "1\n2").unwrap(), []);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn restores_from_trash() {
        let d = tmp("trash");
        let trash = d.join("Trash");
        std::fs::create_dir_all(trash.join("info")).unwrap();
        std::fs::create_dir_all(trash.join("files")).unwrap();
        let orig = d.join("b/my file");
        std::fs::write(trash.join("files/my file.2"), "new").unwrap();
        std::fs::write(trash.join("files/my file"), "old").unwrap();
        let info = |date: &str| {
            format!("[Trash Info]\nPath={}\nDeletionDate={date}\n", orig.display().to_string().replace(' ', "%20"))
        };
        std::fs::write(trash.join("info/my file.trashinfo"), info("2026-01-01T00:00:00")).unwrap();
        std::fs::write(trash.join("info/my file.2.trashinfo"), info("2026-02-01T00:00:00")).unwrap();
        restore(&trash, &orig).unwrap();
        assert_eq!(std::fs::read_to_string(&orig).unwrap(), "new");
        assert!(restore(&trash, &orig).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
