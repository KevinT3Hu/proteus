//! File calls beyond WASI: which host paths a plugin may name, watching
//! directories, the desktop trash and MIME types.

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::manifest::FsGrant;

/// The plugin's view of the file system: `/data` and its `fs` grants.
#[derive(Clone)]
pub struct Access {
    pub data_dir: PathBuf,
    roots: Vec<(PathBuf, Arc<std::fs::File>, bool)>,
}

/// `..` and `.` removed without touching the disk.
fn lexical(p: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::RootDir => out.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(n) => out.push(n),
            Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

/// The real path: symlinks resolved, except in the last component unless
/// `follow`. Missing files resolve through their parent.
fn real(p: &Path, follow: bool) -> Option<PathBuf> {
    if follow && let Ok(c) = p.canonicalize() {
        return Some(c);
    }
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => Some(parent.canonicalize().ok()?.join(name)),
        _ => p.canonicalize().ok(),
    }
}

impl Access {
    pub fn new(data_dir: PathBuf, fs: &[FsGrant]) -> Result<Self, String> {
        use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, openat2};
        let mut roots = Vec::new();
        for (path, write) in
            std::iter::once((data_dir.clone(), true)).chain(fs.iter().map(|g| (g.host_path(), g.write)))
        {
            if !path.exists() {
                continue;
            }
            let path = path.canonicalize().map_err(|e| e.to_string())?;
            let fd = openat2(
                CWD,
                &path,
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
                ResolveFlags::NO_SYMLINKS,
            )
            .map_err(|e| e.to_string())?;
            roots.push((path, Arc::new(std::fs::File::from(fd)), write));
        }
        Ok(Self { data_dir: data_dir.canonicalize().map_err(|e| e.to_string())?, roots })
    }

    /// Host path for `path` as the plugin names it; `write` needs an `rw`
    /// grant. `follow`: the last component may be a symlink to follow.
    pub fn resolve(&self, path: &str, write: bool, follow: bool) -> Result<PathBuf, String> {
        let denied = || format!("{path}: not inside the plugin's directories (permission `fs`)");
        if !path.starts_with('/') {
            return Err(format!("{path}: the path must be absolute"));
        }
        let p = lexical(Path::new(path)).ok_or_else(denied)?;
        if let Ok(rest) = p.strip_prefix("/data") {
            let data = self.data_dir.clone();
            let host = real(&data.join(rest), follow).ok_or_else(|| format!("{path}: not found"))?;
            return if host.starts_with(&data) { Ok(host) } else { Err(denied()) };
        }
        let host = real(&p, follow).ok_or_else(|| format!("{path}: not found"))?;
        let ok = self.roots.iter().any(|(root, _, writable)| host.starts_with(root) && (*writable || !write));
        if ok { Ok(host) } else { Err(denied()) }
    }
}

/// An authorized object (or parent plus final name), retained until the host
/// operation finishes. /proc only names our already-open descriptor here.
#[derive(Debug, Clone)]
pub struct Pinned {
    pub file: Arc<std::fs::File>,
    path: PathBuf,
    pub original: PathBuf,
}
impl std::ops::Deref for Pinned {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}
impl AsRef<Path> for Pinned {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Access {
    pub fn pin(&self, path: &str, write: bool, follow: bool) -> Result<Pinned, String> {
        use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
        use std::os::fd::AsRawFd;
        let host = self.resolve(path, write, follow)?;
        let (root, base, _) = self
            .roots
            .iter()
            .filter(|(_, _, writable)| !write || *writable)
            .find(|(r, _, _)| host.starts_with(r))
            .ok_or("filesystem grant disappeared")?;
        let relative = host.strip_prefix(root).map_err(|e| e.to_string())?;
        let target = if follow { relative } else { relative.parent().unwrap_or(Path::new("")) };
        let target = if target.as_os_str().is_empty() { Path::new(".") } else { target };
        let flags = OFlags::PATH | OFlags::CLOEXEC | if follow { OFlags::empty() } else { OFlags::DIRECTORY };
        let fd = openat2(base, target, flags, Mode::empty(), ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS)
            .map_err(|e| e.to_string())?;
        let file = Arc::new(std::fs::File::from(fd));
        let mut pinned = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        if !follow {
            let name = relative.file_name().ok_or("cannot modify a grant root")?;
            pinned.push(name);
        }

        Ok(Pinned { file, path: pinned, original: host })
    }
}

// --- listing -------------------------------------------------------------------

const MAX_LIST: usize = 200_000;

/// `list-dir`: `host` is the resolved directory, `guest` the path the
/// plugin asked for (links are followed only into its directories).
pub fn list_dir(host: &Path, guest: &Path, access: &Access) -> Result<Vec<crate::runtime::types::DirEntry>, String> {
    let io = |e: std::io::Error| format!("{}: {e}", guest.display());
    let secs = |m: &std::fs::Metadata| m.mtime();
    let mut out = Vec::new();
    for e in std::fs::read_dir(host).map_err(io)?.flatten().take(MAX_LIST) {
        let Ok(meta) = e.metadata() else { continue };
        let link = meta.file_type().is_symlink();
        let name = e.file_name().to_string_lossy().into_owned();
        let target = if link {
            let child = guest.join(&name).display().to_string();
            access.pin(&child, false, true).ok().and_then(|p| std::fs::metadata(&p).ok())
        } else {
            None
        };
        let m = target.as_ref().unwrap_or(&meta);
        out.push(crate::runtime::types::DirEntry { name, dir: m.is_dir(), link, size: m.len(), mtime: secs(m) });
    }
    Ok(out)
}

// --- watching ------------------------------------------------------------------

pub enum Change {
    Created,
    Removed,
    Modified,
    Moved,
}

/// What the watcher reports: (watch id, path as the plugin named it, change).
pub type OnChange = Box<dyn Fn(u32, String, Change) -> bool + Send>;

/// Directory watches of one plugin (one inotify instance, one thread).
pub struct Watcher {
    fd: Arc<OwnedFd>,
    /// inotify watch descriptor → (watch id, directory as the plugin named it).
    dirs: Arc<Mutex<HashMap<i32, (u32, String)>>>,
    next: u32,
    stop: Arc<AtomicBool>,
}

pub const MAX_WATCHES: usize = 256;

impl Watcher {
    pub fn new(on_change: OnChange, stop: Arc<AtomicBool>) -> Result<Watcher, String> {
        use rustix::fs::inotify;
        let fd = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)
            .map_err(|e| format!("watch: {e}"))?;
        let fd = Arc::new(fd);
        let dirs: Arc<Mutex<HashMap<i32, (u32, String)>>> = Arc::default();
        let (f, d, s) = (fd.clone(), dirs.clone(), stop.clone());
        std::thread::Builder::new()
            .name("plugin-watch".into())
            .spawn(move || {
                let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 8192];
                while !s.load(Ordering::Relaxed) {
                    let mut reader = inotify::Reader::new(f.as_fd(), &mut buf);
                    let mut any = false;
                    while let Ok(ev) = reader.next() {
                        any = true;
                        let Some((id, dir)) = d.lock().ok().and_then(|m| m.get(&ev.wd()).cloned()) else { continue };
                        let m = ev.events();
                        let name = ev.file_name().map(|n| n.to_string_lossy().into_owned());
                        let path = match &name {
                            Some(n) if !n.is_empty() => format!("{}/{n}", dir.trim_end_matches('/')),
                            _ => dir.clone(),
                        };
                        use inotify::ReadFlags as R;
                        let change = if m.intersects(R::CREATE | R::MOVED_TO) {
                            Change::Created
                        } else if m.intersects(R::DELETE | R::DELETE_SELF | R::MOVE_SELF) {
                            Change::Removed
                        } else if m.contains(R::MOVED_FROM) {
                            Change::Moved
                        } else if m.intersects(R::CLOSE_WRITE | R::ATTRIB) {
                            Change::Modified
                        } else {
                            continue;
                        };
                        if !on_change(id, path, change) {
                            return;
                        }
                    }
                    if !any {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Watcher { fd, dirs, next: 0, stop })
    }

    pub fn add(&mut self, host: &Path, guest: &str) -> Result<u32, String> {
        use rustix::fs::inotify::{WatchFlags as W, add_watch};
        if !host.is_dir() {
            return Err(format!("{guest}: not a directory"));
        }
        let mut dirs = self.dirs.lock().map_err(|e| e.to_string())?;
        if dirs.len() >= MAX_WATCHES {
            return Err(format!("at most {MAX_WATCHES} watches"));
        }
        let flags = W::CREATE
            | W::DELETE
            | W::CLOSE_WRITE
            | W::ATTRIB
            | W::MOVED_FROM
            | W::MOVED_TO
            | W::DELETE_SELF
            | W::MOVE_SELF
            | W::ONLYDIR;
        let wd = add_watch(&*self.fd, host, flags).map_err(|e| format!("{guest}: {e}"))?;
        // Watching the same directory twice gives the same descriptor.
        if let Some((id, _)) = dirs.get(&wd) {
            return Ok(*id);
        }
        self.next += 1;
        dirs.insert(wd, (self.next, guest.trim_end_matches('/').to_string()));
        Ok(self.next)
    }

    pub fn remove(&mut self, id: u32) {
        let Ok(mut dirs) = self.dirs.lock() else { return };
        if let Some(&wd) = dirs.iter().find(|(_, (i, _))| *i == id).map(|(wd, _)| wd) {
            dirs.remove(&wd);
            let _ = rustix::fs::inotify::remove_watch(&*self.fd, wd);
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// --- trash ---------------------------------------------------------------------

fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
}

/// `2026-10-21T13:05:09` (UTC: the spec wants local time without a zone;
/// file managers only show it).
fn deletion_date(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Days to civil date (Howard Hinnant).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}", rem / 3600, rem / 60 % 60, rem % 60)
}

fn percent_encode(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut s = String::new();
    for &b in p.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

/// The trash directory for `path` (freedesktop.org trash spec): the home
/// trash on the same file system, else the file system's own.
fn trash_dir(path: &Path, dev: u64) -> Result<PathBuf, String> {
    let home = data_home().ok_or("no home directory")?.join("Trash");
    proteus_pty::secure_fs::private_dir(&home.join("files")).map_err(|e| e.to_string())?;
    proteus_pty::secure_fs::private_dir(&home.join("info")).map_err(|e| e.to_string())?;
    if std::fs::metadata(&home).is_ok_and(|m| m.dev() == dev) {
        return Ok(home);
    }
    let mut top = path.parent().unwrap_or(Path::new("/"));
    while let Some(up) = top.parent() {
        if std::fs::metadata(up).map_or(true, |m| m.dev() != dev) {
            break;
        }
        top = up;
    }
    let uid = rustix::process::getuid().as_raw();
    let shared = top.join(".Trash");
    if let Ok(m) = std::fs::symlink_metadata(&shared)
        && m.is_dir()
        && m.mode() & 0o1000 != 0
    {
        let mine = shared.join(uid.to_string());
        if proteus_pty::secure_fs::private_dir(&mine.join("files")).is_ok()
            && proteus_pty::secure_fs::private_dir(&mine.join("info")).is_ok()
        {
            return Ok(mine);
        }
    }
    let own = top.join(format!(".Trash-{uid}"));
    for d in [own.join("files"), own.join("info")] {
        proteus_pty::secure_fs::private_dir(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    }

    Ok(own)
}

/// Move `path` (resolved, not followed) to the trash.
pub fn trash(path: &Path) -> Result<(), String> {
    trash_named(path, path)
}

pub fn trash_named(path: &Path, original: &Path) -> Result<(), String> {
    let io = |e: std::io::Error| format!("{}: {e}", path.display());
    let meta = std::fs::symlink_metadata(path).map_err(io)?;
    let dir = trash_dir(original, meta.dev())?;
    let files = proteus_pty::secure_fs::private_dir(&dir.join("files")).map_err(|e| e.to_string())?;
    let info_dir = proteus_pty::secure_fs::private_dir(&dir.join("info")).map_err(|e| e.to_string())?;
    use std::os::fd::AsRawFd;
    let files_path = PathBuf::from(format!("/proc/self/fd/{}", files.as_raw_fd()));
    let info_dir_path = PathBuf::from(format!("/proc/self/fd/{}", info_dir.as_raw_fd()));
    let name = original.file_name().ok_or_else(|| format!("{}: cannot trash this", path.display()))?;
    let name = name.to_string_lossy();
    let info =
        format!("[Trash Info]\nPath={}\nDeletionDate={}\n", percent_encode(original), deletion_date(SystemTime::now()));
    for n in 1..10_000 {
        let candidate = if n == 1 { name.to_string() } else { format!("{name}.{n}") };
        let info_path = info_dir_path.join(format!("{candidate}.trashinfo"));
        match std::fs::File::create_new(&info_path) {
            Ok(mut f) => {
                use std::io::Write;
                f.write_all(info.as_bytes()).map_err(|e| format!("{}: {e}", info_path.display()))?;
                let target = files_path.join(&candidate);
                return match rustix::fs::renameat_with(
                    rustix::fs::CWD,
                    path,
                    rustix::fs::CWD,
                    &target,
                    rustix::fs::RenameFlags::NOREPLACE,
                )
                .map_err(std::io::Error::from)
                {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        let _ = std::fs::remove_file(&info_path);
                        Err(io(e))
                    }
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{}: {e}", info_path.display())),
        }
    }
    Err(format!("{}: the trash is full of files with this name", path.display()))
}

// --- MIME types ----------------------------------------------------------------

struct Globs {
    /// Whole file names (lowercase) → type.
    names: HashMap<String, String>,
    /// Extensions without the dot (lowercase) → (weight, type).
    exts: HashMap<String, (u32, String)>,
}

fn globs() -> &'static Globs {
    static G: OnceLock<Globs> = OnceLock::new();
    G.get_or_init(|| {
        let mut g = Globs { names: HashMap::new(), exts: HashMap::new() };
        let mut dirs: Vec<PathBuf> = std::env::var("XDG_DATA_DIRS")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into())
            .split(':')
            .map(PathBuf::from)
            .collect();
        dirs.reverse();
        dirs.extend(data_home());
        // Later directories win (the user's own last).
        for d in dirs {
            let Ok(text) = std::fs::read_to_string(d.join("mime/globs2")) else { continue };
            parse_globs(&text, &mut g);
        }
        g
    })
}

fn parse_globs(text: &str, g: &mut Globs) {
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let mut parts = line.split(':');
        let (Some(w), Some(mime), Some(glob)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let w: u32 = w.parse().unwrap_or(50);
        let glob = glob.to_lowercase();
        if let Some(ext) = glob.strip_prefix("*.") {
            if !ext.contains(['*', '?', '[']) {
                let e = g.exts.entry(ext.to_string()).or_insert((0, String::new()));
                if w >= e.0 {
                    *e = (w, mime.to_string());
                }
            }
        } else if !glob.contains(['*', '?', '[']) {
            g.names.insert(glob, mime.to_string());
        }
    }
}

fn by_name(name: &str, g: &Globs) -> Option<String> {
    let lower = name.to_lowercase();
    if let Some(m) = g.names.get(&lower) {
        return Some(m.clone());
    }
    // The longest matching extension: `x.tar.gz` before `gz`.
    let mut rest = lower.as_str();
    while let Some(i) = rest.find('.') {
        rest = &rest[i + 1..];
        if let Some((_, m)) = g.exts.get(rest) {
            return Some(m.clone());
        }
    }
    None
}

fn sniff(head: &[u8]) -> &'static str {
    const MAGIC: [(&[u8], &str); 12] = [
        (b"\x89PNG\r\n\x1a\n", "image/png"),
        (b"\xff\xd8\xff", "image/jpeg"),
        (b"GIF8", "image/gif"),
        (b"%PDF-", "application/pdf"),
        (b"PK\x03\x04", "application/zip"),
        (b"\x1f\x8b", "application/gzip"),
        (b"\x7fELF", "application/x-executable"),
        (b"\x28\xb5\x2f\xfd", "application/zstd"),
        (b"\xfd7zXZ\0", "application/x-xz"),
        (b"BZh", "application/x-bzip2"),
        (b"7z\xbc\xaf\x27\x1c", "application/x-7z-compressed"),
        (b"#!", "application/x-executable"),
    ];
    if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return "image/webp";
    }
    if let Some((_, m)) = MAGIC.iter().find(|(magic, _)| head.starts_with(magic)) {
        return m;
    }
    // Text: valid UTF-8 (a cut-off character at the end is fine), no NUL.
    let valid = match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    };
    if valid && !head.contains(&0) { "text/plain" } else { "application/octet-stream" }
}

pub fn mime_type(path: &Path) -> Result<String, String> {
    mime_type_named(path, path)
}

pub fn mime_type_named(path: &Path, original: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.is_dir() {
        return Ok("inode/directory".into());
    }
    if !meta.is_file() {
        use std::os::unix::fs::FileTypeExt;
        let t = meta.file_type();
        let kind = if t.is_fifo() {
            "inode/fifo"
        } else if t.is_socket() {
            "inode/socket"
        } else if t.is_block_device() {
            "inode/blockdevice"
        } else {
            "inode/chardevice"
        };
        return Ok(kind.into());
    }
    if let Some(m) = original.file_name().and_then(|n| by_name(&n.to_string_lossy(), globs())) {
        return Ok(m);
    }
    if meta.len() == 0 {
        return Ok("application/x-zerosize".into());
    }
    let mut head = vec![0; 4096];
    let n = {
        use std::io::Read;
        let mut f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        f.read(&mut head).map_err(|e| format!("{}: {e}", path.display()))?
    };
    Ok(sniff(&head[..n]).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_descriptors_resist_directory_and_grant_root_swaps() {
        let root = std::env::temp_dir().join(format!("proteus-fs-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data = root.join("data");
        let outside = root.join("outside");
        std::fs::create_dir_all(data.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(data.join("sub/file"), "inside").unwrap();
        std::fs::write(outside.join("file"), "outside").unwrap();
        let access = Access::new(data.clone(), &[]).unwrap();
        let parent = access.pin("/data/sub/new", true, false).unwrap();
        let object = access.pin("/data/sub/file", false, true).unwrap();
        std::fs::rename(data.join("sub"), data.join("old")).unwrap();
        std::os::unix::fs::symlink(&outside, data.join("sub")).unwrap();
        std::fs::write(&parent, "safe").unwrap();
        assert_eq!(std::fs::read_to_string(&object).unwrap(), "inside");
        assert!(data.join("old/new").exists());
        assert!(!outside.join("new").exists());
        assert!(access.pin("/data/sub/file", false, true).is_err());
        std::fs::rename(&data, root.join("old-data")).unwrap();
        std::os::unix::fs::symlink(&outside, &data).unwrap();
        assert!(access.pin("/data/file", false, true).is_err());
        assert_eq!(std::fs::read_to_string(outside.join("file")).unwrap(), "outside");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mime_globs_and_magic() {
        let mut g = Globs { names: HashMap::new(), exts: HashMap::new() };
        parse_globs(
            "# c\n50:text/x-rust:*.rs\n50:application/gzip:*.gz\n50:application/x-compressed-tar:*.tar.gz\n50:text/x-makefile:makefile\n",
            &mut g,
        );
        assert_eq!(by_name("main.RS", &g).as_deref(), Some("text/x-rust"));
        assert_eq!(by_name("a.tar.gz", &g).as_deref(), Some("application/x-compressed-tar"));
        assert_eq!(by_name("a.gz", &g).as_deref(), Some("application/gzip"));
        assert_eq!(by_name("Makefile", &g).as_deref(), Some("text/x-makefile"));
        assert_eq!(by_name("README", &g), None);
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nxxxx"), "image/png");
        assert_eq!(sniff("héllo\n".as_bytes()), "text/plain");
        assert_eq!(sniff(&"中".as_bytes()[..2]), "text/plain");
        assert_eq!(sniff(b"a\0b"), "application/octet-stream");
    }

    #[test]
    fn dates_and_names() {
        assert_eq!(deletion_date(UNIX_EPOCH), "1970-01-01T00:00:00");
        assert_eq!(deletion_date(UNIX_EPOCH + Duration::from_secs(1_792_587_909)), "2026-10-21T13:05:09");
        assert_eq!(percent_encode(Path::new("/a b/ü")), "/a%20b/%C3%BC");
    }

    #[test]
    fn access_and_trash() {
        let root = std::env::temp_dir().join(format!("proteus-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let (data, src, other) = (root.join("data"), root.join("src"), root.join("other"));
        for d in [&data, &src, &other] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::os::unix::fs::symlink(&other, src.join("escape")).unwrap();
        let a = Access::new(data.clone(), &[FsGrant { path: src.display().to_string(), write: false }]).unwrap();
        let s = src.display().to_string();
        assert_eq!(a.resolve("/data/x", true, true).unwrap(), data.canonicalize().unwrap().join("x"));
        assert!(a.resolve("/data/../src", false, true).is_err());
        assert!(a.resolve(&format!("{s}/new.txt"), false, true).is_ok());
        assert!(a.resolve(&format!("{s}/new.txt"), true, true).is_err(), "read-only grant");
        assert!(a.resolve(&format!("{s}/escape"), false, true).is_err(), "symlink out of the grant");
        assert!(a.resolve(&format!("{s}/escape"), false, false).is_ok(), "the link itself");
        assert!(a.resolve(&format!("{s}/../other"), false, true).is_err());
        assert!(a.resolve("relative", false, true).is_err());

        // The trash: XDG_DATA_HOME points into the temp dir (same file system).
        let file = data.join("doomed.txt");
        std::fs::write(&file, "x").unwrap();
        // SAFETY: tests in this module do not read the environment concurrently.
        unsafe { std::env::set_var("XDG_DATA_HOME", root.join("share")) };
        trash(&file).unwrap();
        assert!(!file.exists());
        assert!(root.join("share/Trash/files/doomed.txt").exists());
        let info = std::fs::read_to_string(root.join("share/Trash/info/doomed.txt.trashinfo")).unwrap();
        assert!(info.contains("Path=") && info.contains("doomed.txt"), "{info}");
        std::fs::write(&file, "y").unwrap();
        trash(&file).unwrap();
        assert!(root.join("share/Trash/files/doomed.txt.2").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
