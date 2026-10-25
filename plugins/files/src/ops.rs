//! File operations that run in steps (so one plugin call stays short).

use crate::i18n::tr;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Overwrite,
    Skip,
    Rename,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Working,
    Conflict(PathBuf),
    Done,
}

struct Item {
    src: PathBuf,
    dst: PathBuf,
    top: bool,
}
enum Task {
    Item(Item),
    Directory { item: Item, mode: u32, created: bool, failures: usize },
}
struct Open {
    from: File,
    to: File,
    item: Item,
    temp: PathBuf,
    mode: u32,
    replacing: bool,
}
impl Drop for Open {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temp);
    }
}
const CHUNK: usize = 1 << 20;

/// A copy or move. Files are staged privately and committed by rename.
/// Source files are removed only after their copies commit; source directories
/// use remove_dir, never recursive deletion, so failures and new files survive.
pub struct Transfer {
    pub kind: OpKind,
    queue: VecDeque<Task>,
    open: Option<Open>,
    all: Option<Choice>,
    once: Option<Choice>,
    pub total: u64,
    pub done: u64,
    pub errors: Vec<String>,
    /// Successfully completed top-level pairs.
    pub finished: Vec<(PathBuf, PathBuf)>,
    /// Merges/replacements and partial operations cannot be reversed as one
    /// directory rename. The UI must not offer that destructive approximation.
    pub undoable: bool,
    failures: usize,
}

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

#[cfg(not(target_arch = "wasm32"))]
fn file_mode(path: &Path) -> std::io::Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    Ok(std::fs::metadata(path)?.permissions().mode() & 0o777)
}
#[cfg(target_arch = "wasm32")]
fn file_mode(path: &Path) -> std::io::Result<u32> {
    proteus_plugin::host::file_mode(&path.display().to_string()).map_err(std::io::Error::other)
}
#[cfg(not(target_arch = "wasm32"))]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::fs::PermissionsExt;
    File::options()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)?
        .set_permissions(std::fs::Permissions::from_mode(mode & 0o777))
}
#[cfg(target_arch = "wasm32")]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    proteus_plugin::host::set_file_mode(&path.display().to_string(), mode).map_err(std::io::Error::other)
}
#[cfg(not(target_arch = "wasm32"))]
fn create_private(path: &Path, directory: bool) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if directory {
        std::fs::DirBuilder::new().mode(0o700).create(path)
    } else {
        File::options().write(true).create_new(true).mode(0o600).open(path).map(|_| ())
    }
}
#[cfg(target_arch = "wasm32")]
fn create_private(path: &Path, directory: bool) -> std::io::Result<()> {
    proteus_plugin::host::create_private(&path.display().to_string(), directory).map_err(|e| {
        if std::fs::symlink_metadata(path).is_ok() {
            std::io::Error::new(std::io::ErrorKind::AlreadyExists, e)
        } else {
            std::io::Error::other(e)
        }
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn create_staging(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    // Keep the original O_EXCL descriptor: never reopen a predictable staging
    // pathname, which another process could have replaced with a symlink.
    File::options().write(true).create_new(true).mode(0o600).open(path)
}
#[cfg(target_arch = "wasm32")]
fn create_staging(path: &Path) -> std::io::Result<File> {
    let file = File::options().write(true).create_new(true).open(path)?;
    // WASI does not expose creation modes. The file is still empty here;
    // restrict its mode through the host before writing any source bytes.
    if let Err(e) = set_mode(path, 0o600) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    Ok(file)
}

#[cfg(not(target_arch = "wasm32"))]
fn publish_file(temp: &Path, dst: &Path, replacing: bool) -> std::io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    let flags = if replacing { RenameFlags::empty() } else { RenameFlags::NOREPLACE };
    renameat_with(CWD, temp, CWD, dst, flags).map_err(Into::into)
}
#[cfg(target_arch = "wasm32")]
fn publish_file(temp: &Path, dst: &Path, replacing: bool) -> std::io::Result<()> {
    proteus_plugin::host::publish_file(&temp.display().to_string(), &dst.display().to_string(), replacing)
        .map_err(std::io::Error::other)
}

/// Resolve existing ancestors too, so a symlink alias cannot turn a copy
/// into an unbounded recursive copy into its own source.
fn destination_path(path: &Path) -> std::io::Result<PathBuf> {
    if let Ok(p) = path.canonicalize() {
        return Ok(p);
    }
    let parent = path.parent().ok_or_else(|| std::io::Error::other(tr!("no-parent")))?;
    Ok(destination_path(parent)?.join(path.file_name().ok_or_else(|| std::io::Error::other(tr!("no-file-name")))?))
}

impl Transfer {
    pub fn new(kind: OpKind, pairs: Vec<(PathBuf, PathBuf)>) -> Transfer {
        let mut budget = 200_000;
        let total = pairs.iter().map(|(s, _)| size_of(s, &mut budget)).sum();
        Transfer {
            kind,
            queue: pairs.into_iter().map(|(src, dst)| Task::Item(Item { src, dst, top: true })).collect(),
            open: None,
            all: None,
            once: None,
            total,
            done: 0,
            errors: Vec::new(),
            finished: Vec::new(),
            undoable: true,
            failures: 0,
        }
    }
    fn error(&mut self, path: &Path, e: impl std::fmt::Display) {
        self.errors.push(format!("{}: {e}", path.display()));
        self.failures += 1;
        self.undoable = false;
    }
    pub fn resolve(&mut self, c: Choice, all: bool) {
        if all {
            self.all = Some(c);
        }
        self.once = Some(c);
    }
    pub fn cancel(&mut self) {
        self.open = None;
        self.queue.clear();
        self.undoable = false;
    }
    fn completed(&mut self, item: Item) {
        if item.top {
            self.finished.push((item.src, item.dst));
        }
    }
    pub fn step(&mut self, budget: Duration) -> Step {
        let start = Instant::now();
        while start.elapsed() < budget {
            if let Some(mut o) = self.open.take() {
                let mut buf = vec![0; CHUNK];
                match o.from.read(&mut buf) {
                    Ok(0) => {
                        // Flush staging writes before publishing the file.
                        let result =
                            o.to.flush()
                                .and_then(|_| set_mode(&o.temp, o.mode))
                                .and_then(|_| publish_file(&o.temp, &o.item.dst, o.replacing));
                        if let Err(e) = result {
                            self.error(&o.item.dst, e);
                            continue;
                        }
                        if self.kind == OpKind::Move
                            && let Err(e) = std::fs::remove_file(&o.item.src)
                        {
                            self.error(&o.item.src, e);
                            continue;
                        }
                        let item = Item { src: o.item.src.clone(), dst: o.item.dst.clone(), top: o.item.top };
                        self.completed(item);
                    }
                    Ok(n) => match o.to.write_all(&buf[..n]) {
                        Ok(()) => {
                            self.done += n as u64;
                            self.open = Some(o);
                        }
                        Err(e) => self.error(&o.item.dst, e),
                    },
                    Err(e) => self.error(&o.item.src, e),
                }
                continue;
            }
            match self.queue.pop_front() {
                None => return Step::Done,
                Some(Task::Item(item)) => {
                    if let Some(s) = self.item(item) {
                        return s;
                    }
                }
                Some(Task::Directory { item, mode, created, failures }) => {
                    if created && let Err(e) = set_mode(&item.dst, mode) {
                        self.error(&item.dst, e);
                    }
                    if failures != self.failures {
                        continue;
                    }
                    if self.kind == OpKind::Move
                        && let Err(e) = std::fs::remove_dir(&item.src)
                    {
                        self.error(&item.src, e);
                        continue;
                    }
                    self.completed(item);
                }
            }
        }
        Step::Working
    }
    fn item(&mut self, mut it: Item) -> Option<Step> {
        let meta = match std::fs::symlink_metadata(&it.src) {
            Ok(m) => m,
            Err(e) => {
                self.error(&it.src, e);
                return None;
            }
        };
        if it.src == it.dst {
            if self.kind == OpKind::Move {
                return None;
            }
            it.dst = unique(&it.dst);
        }
        if meta.is_dir() {
            match (it.src.canonicalize(), destination_path(&it.dst)) {
                (Ok(src), Ok(dst)) if dst.starts_with(&src) => {
                    self.error(&it.src, tr!("copy-self"));
                    return None;
                }
                (Err(e), _) | (_, Err(e)) => {
                    self.error(&it.dst, e);
                    return None;
                }
                _ => {}
            }
        }
        let existing = match std::fs::symlink_metadata(&it.dst) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                self.error(&it.dst, e);
                return None;
            }
        };
        let mut replacing = existing.is_some();
        if existing.is_some() {
            let Some(c) = self.once.take().or(self.all) else {
                let dst = it.dst.clone();
                self.queue.push_front(Task::Item(it));
                return Some(Step::Conflict(dst));
            };
            match c {
                Choice::Skip => {
                    self.failures += 1;
                    self.undoable = false;
                    return None;
                }
                Choice::Rename => {
                    it.dst = unique(&it.dst);
                    replacing = false;
                }
                Choice::Overwrite => self.undoable = false,
            }
        }
        let merging = replacing && meta.is_dir() && existing.as_ref().is_some_and(|m| m.is_dir());
        if replacing && existing.as_ref().is_some_and(|m| !m.file_type().is_symlink() && m.is_dir() != meta.is_dir()) {
            self.error(&it.dst, tr!("copy-type-conflict"));
            return None;
        }
        if self.kind == OpKind::Move && !merging && publish_file(&it.src, &it.dst, replacing).is_ok() {
            self.completed(it);
            return None;
        }
        // Cross-device moves (and directory replacements of symlinks) copy
        // through private staging instead.
        if meta.file_type().is_symlink() || !(meta.is_dir() || meta.is_file()) {
            self.error(&it.src, tr!("copy-unsupported"));
            return None;
        }
        let mode = match file_mode(&it.src) {
            Ok(m) => m,
            Err(e) => {
                self.error(&it.src, e);
                return None;
            }
        };
        if meta.is_dir() {
            let kids: std::io::Result<Vec<_>> = std::fs::read_dir(&it.src).and_then(|r| r.collect());
            let mut kids = match kids {
                Ok(k) => k,
                Err(e) => {
                    self.error(&it.src, e);
                    return None;
                }
            };
            if replacing
                && !merging
                && let Err(e) = remove_any(&it.dst)
            {
                self.error(&it.dst, e);
                return None;
            }
            if !merging && let Err(e) = create_private(&it.dst, true) {
                self.error(&it.dst, e);
                return None;
            }
            kids.sort_by_key(|e| e.file_name());
            for k in kids.iter().rev() {
                let n = k.file_name();
                self.queue.push_front(Task::Item(Item { src: it.src.join(&n), dst: it.dst.join(&n), top: false }));
            }
            // The completion marker must run after this subtree.
            self.queue
                .insert(kids.len(), Task::Directory { item: it, mode, created: !merging, failures: self.failures });
        } else {
            let from = match File::open(&it.src) {
                Ok(f) => f,
                Err(e) => {
                    self.error(&it.src, e);
                    return None;
                }
            };
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let (temp, to) = loop {
                let p = it.dst.with_file_name(format!(
                    ".proteus-copy-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match create_staging(&p) {
                    Ok(to) => break (p, to),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => {
                        self.error(&it.dst, e);
                        return None;
                    }
                }
            };
            self.open = Some(Open { from, to, item: it, temp, mode, replacing });
        }
        None
    }
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
        return Err(tr!("rename-count-mismatch", names = lines.len(), count = names.len()));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (old, new) in names.iter().zip(lines) {
        if new.is_empty() || new.contains('/') || new == "." || new == ".." || new.chars().any(char::is_control) {
            return Err(tr!("rename-bad-name", name = new));
        }
        if !seen.insert(new) {
            return Err(tr!("rename-duplicate", name = new));
        }
        if new != old {
            out.push((dir.join(old), dir.join(new)));
        }
    }
    let sources: HashSet<&PathBuf> = out.iter().map(|p| &p.0).collect();
    for (_, to) in &out {
        if !sources.contains(to) && std::fs::symlink_metadata(to).is_ok() {
            return Err(tr!("rename-exists", path = to.display().to_string()));
        }
    }
    Ok(out)
}

/// Carry out renames (also swaps and cycles: through temporary names).
/// Returns those done.
pub fn rename_all(pairs: &[(PathBuf, PathBuf)]) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    rename_all_with(pairs, |_, _| {})
}

fn rename_all_with(
    pairs: &[(PathBuf, PathBuf)],
    staged: impl FnOnce(&[(PathBuf, PathBuf)], &[PathBuf]),
) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut tmp = Vec::<PathBuf>::new();
    let mut directories = Vec::new();
    for (from, _) in pairs {
        // Reserve a private directory exclusively, then keep its entry until
        // publication or recovery. Existing staging paths are never replaced.
        let result = (|| {
            let dir = loop {
                let dir = from.with_file_name(format!(
                    ".proteus-rename-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match create_private(&dir, true) {
                    Ok(()) => break dir,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            };
            let t = dir.join("entry");
            directories.push(dir);
            publish_file(from, &t, false)?;
            Ok(t)
        })();
        let t = match result {
            Ok(t) => t,
            Err(e) => {
                // Put back what was moved.
                let mut errors = vec![format!("{}: {e}", from.display())];
                for ((f, _), t) in pairs.iter().zip(&tmp) {
                    if let Err(e) = publish_file(t, f, false) {
                        errors.push(format!("recovery file {}: {e}", t.display()));
                    }
                }
                for dir in directories {
                    let _ = std::fs::remove_dir(dir);
                }
                return Err(errors.join("\n"));
            }
        };
        tmp.push(t);
    }
    staged(pairs, &tmp);
    let mut done = Vec::new();
    let mut errors = Vec::new();
    for ((from, to), t) in pairs.iter().zip(&tmp) {
        match publish_file(t, to, false) {
            Ok(()) => done.push((from.clone(), to.clone())),
            Err(e) => {
                let recovery = match publish_file(t, from, false) {
                    Ok(()) => String::new(),
                    Err(e) => format!("; recovery file {}: {e}", t.display()),
                };
                errors.push(format!("{}: {e}{recovery}", to.display()));
            }
        }
    }
    for dir in directories {
        let _ = std::fs::remove_dir(dir);
    }
    if errors.is_empty() { Ok(done) } else { Err(format!("{} renames completed; {}", done.len(), errors.join("\n"))) }
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
    let (file, info) = trashed(trash, path).ok_or_else(|| tr!("not-in-trash", path = path.display().to_string()))?;
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(tr!("restore-exists", path = path.display().to_string()));
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

    fn finish(t: &mut Transfer) {
        loop {
            match run(t) {
                Step::Done => return,
                Step::Conflict(_) => t.resolve(Choice::Overwrite, true),
                Step::Working => unreachable!(),
            }
        }
    }

    #[test]
    fn failed_staged_move_preserves_source_and_replaced_destination() {
        let d = tmp("failure");
        let src = d.join("a/x.txt");
        let dst = d.join("b/x.txt");
        std::fs::write(&dst, "previous destination").unwrap();
        for read_error in [false, true] {
            let mut t = Transfer::new(OpKind::Copy, vec![]);
            t.resolve(Choice::Overwrite, true);
            assert_eq!(t.item(Item { src: src.clone(), dst: dst.clone(), top: true }), None);
            t.kind = OpKind::Move; // Exercise the copy fallback of a move.
            let o = t.open.as_mut().unwrap();
            let stage = o.temp.clone();
            if read_error {
                o.from = File::open(d.join("a")).unwrap();
            } else {
                o.to = File::options().write(true).open("/dev/full").unwrap();
            }
            finish(&mut t);
            assert!(!t.errors.is_empty());
            assert!(src.exists());
            assert_eq!(std::fs::read_to_string(&dst).unwrap(), "previous destination");
            assert!(!stage.exists());
            assert!(t.finished.is_empty());
            assert!(!t.undoable);
        }
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn merges_replace_links_without_following_them() {
        let d = tmp("links");
        let outside = d.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("file"), "unrelated").unwrap();
        std::os::unix::fs::symlink(outside.join("file"), d.join("b/x.txt")).unwrap();
        std::os::unix::fs::symlink(&outside, d.join("b/sub")).unwrap();
        let mut t = Transfer::new(OpKind::Copy, vec![(d.join("a"), d.join("b"))]);
        finish(&mut t);
        assert!(t.errors.is_empty(), "{:?}", t.errors);
        assert_eq!(std::fs::read_to_string(outside.join("file")).unwrap(), "unrelated");
        assert!(!outside.join("y").exists());
        assert!(!std::fs::symlink_metadata(d.join("b/x.txt")).unwrap().file_type().is_symlink());
        assert!(!std::fs::symlink_metadata(d.join("b/sub")).unwrap().file_type().is_symlink());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn partial_directory_moves_keep_uncopied_files() {
        let d = tmp("partial");
        std::fs::write(d.join("b/x.txt"), "keep").unwrap();
        let mut t = Transfer::new(OpKind::Move, vec![(d.join("a"), d.join("b"))]);
        assert!(matches!(run(&mut t), Step::Conflict(_)));
        t.resolve(Choice::Overwrite, false);
        // A directory merge does not authorize overwriting its children.
        assert_eq!(run(&mut t), Step::Conflict(d.join("b/x.txt")));
        t.resolve(Choice::Skip, false);
        finish(&mut t);
        assert!(d.join("a/x.txt").exists());
        assert_eq!(std::fs::read_to_string(d.join("b/x.txt")).unwrap(), "keep");
        assert!(!t.undoable);
        assert!(t.finished.is_empty());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn merged_moves_do_not_offer_whole_directory_undo() {
        let d = tmp("undo-merge");
        std::fs::write(d.join("b/existing"), "unrelated").unwrap();
        let mut t = Transfer::new(OpKind::Move, vec![(d.join("a"), d.join("b"))]);
        finish(&mut t);
        assert!(t.errors.is_empty());
        assert_eq!(t.finished.len(), 1);
        assert!(!t.undoable);
        assert_eq!(std::fs::read_to_string(d.join("b/existing")).unwrap(), "unrelated");
        assert!(!d.join("a").exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn staged_copies_preserve_private_modes_and_executable_bits() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("modes");
        std::fs::set_permissions(d.join("a"), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(d.join("a/x.txt"), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(d.join("a/sub/y"), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut t = Transfer::new(OpKind::Copy, vec![(d.join("a"), d.join("b/a"))]);
        finish(&mut t);
        assert!(t.errors.is_empty(), "{:?}", t.errors);
        assert_eq!(file_mode(&d.join("b/a")).unwrap(), 0o700);
        assert_eq!(file_mode(&d.join("b/a/x.txt")).unwrap(), 0o600);
        assert_eq!(file_mode(&d.join("b/a/sub/y")).unwrap(), 0o700);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn rejects_copy_into_source_via_symlink_alias() {
        let d = tmp("alias");
        std::os::unix::fs::symlink(d.join("a"), d.join("alias")).unwrap();
        let mut t = Transfer::new(OpKind::Copy, vec![(d.join("a"), d.join("alias/new"))]);
        finish(&mut t);
        assert_eq!(t.errors.len(), 1);
        assert!(!d.join("a/new").exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn new_destinations_are_not_overwritten_if_created_during_copy() {
        let d = tmp("new-destination");
        let src = d.join("a/x.txt");
        let dst = d.join("b/new.txt");
        let mut t = Transfer::new(OpKind::Copy, vec![]);
        t.item(Item { src: src.clone(), dst: dst.clone(), top: true });
        std::fs::write(&dst, "created concurrently").unwrap();
        finish(&mut t);
        assert!(!t.errors.is_empty());
        assert!(src.exists());
        assert_eq!(std::fs::read_to_string(dst).unwrap(), "created concurrently");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn type_conflicts_do_not_delete_either_tree() {
        let d = tmp("type-conflict");
        let mut t = Transfer::new(OpKind::Move, vec![(d.join("a/x.txt"), d.join("a/sub"))]);
        finish(&mut t);
        assert!(!t.errors.is_empty());
        assert!(d.join("a/x.txt").exists());
        assert_eq!(std::fs::read(d.join("a/sub/y")).unwrap(), b"y");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn bulk_rename_preserves_staging_collisions_and_racing_destinations() {
        let root = tmp("rename-collisions");
        let from = root.join("source");
        let to = root.join("target");
        std::fs::write(&from, "source data").unwrap();
        // Cover both the old staging name and a range of reserved-directory
        // names: all preexisting objects must survive untouched.
        let mut occupied = vec![root.join(format!(".proteus-rename-0-{}", std::process::id()))];
        occupied.extend((0..10).map(|n| root.join(format!(".proteus-rename-{}-{n}", std::process::id()))));
        for p in &occupied {
            std::fs::write(p, "unrelated data").unwrap();
        }
        rename_all(&[(from.clone(), to.clone())]).unwrap();
        for p in occupied {
            assert_eq!(std::fs::read_to_string(p).unwrap(), "unrelated data");
        }
        std::fs::rename(&to, &from).unwrap();
        let result = rename_all_with(&[(from.clone(), to.clone())], |_, _| {
            std::fs::write(&to, "concurrent writer").unwrap();
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "concurrent writer");
        assert_eq!(std::fs::read_to_string(&from).unwrap(), "source data");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bulk_rename_retains_recovery_files_when_rollback_is_obstructed() {
        let root = tmp("rename-recovery");
        let a = root.join("one");
        let b = root.join("two");
        std::fs::write(&a, "one").unwrap();
        std::fs::write(&b, "two").unwrap();
        let mut retained = PathBuf::new();
        let result = rename_all_with(&[(a.clone(), b.clone()), (b.clone(), a.clone())], |_, temps| {
            retained = temps[1].clone();
            std::fs::write(&a, "concurrent writer").unwrap();
        });
        let error = result.unwrap_err();
        assert!(error.contains(&retained.display().to_string()));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "concurrent writer");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(&retained).unwrap(), "two");
        std::fs::remove_dir_all(root).unwrap();
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
