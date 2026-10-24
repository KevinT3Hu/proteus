//! Private, descriptor-relative state files. Successful replacement means the
//! new file AND its containing directory have been synchronized to storage.
use rustix::fs::{AtFlags, Mode, OFlags, openat, renameat, unlinkat};
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

pub struct StateFile {
    dir: File,
    name: OsString,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Written,
    FileSynced,
    Renamed,
}

impl StateFile {
    pub fn new(path: &Path) -> io::Result<Self> {
        Self::open(path, true)
    }

    /// Private file in an explicitly chosen directory; don't change the
    /// permissions of that directory (which may be a shared project).
    pub fn in_directory(path: &Path) -> io::Result<Self> {
        Self::open(path, false)
    }

    fn open(path: &Path, private: bool) -> io::Result<Self> {
        let path = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
        let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?.to_owned();
        let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
        let dir = if private { crate::secure_fs::state_dir(parent)? } else { crate::secure_fs::trusted_dir(parent)? };
        Ok(Self { dir, name })
    }

    fn open_private(&self, name: &std::ffi::OsStr, flags: OFlags) -> io::Result<File> {
        let fd = openat(
            &self.dir,
            name,
            flags | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        let m = rustix::fs::fstat(&fd)?;
        if rustix::fs::FileType::from_raw_mode(m.st_mode) != rustix::fs::FileType::RegularFile
            || m.st_uid != rustix::process::geteuid().as_raw()
            || m.st_nlink != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "state file must be a user-owned regular file with one link",
            ));
        }
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o600))?;
        Ok(fd.into())
    }

    /// Lock the stable sidecar inode, never the file replaced by rename.
    /// Locks are released automatically when the returned File is dropped.
    pub fn lock(&self, nonblocking: bool) -> io::Result<File> {
        let mut name = self.name.clone();
        name.push(".lock");
        let lock = self.open_private(&name, OFlags::RDWR | OFlags::CREATE)?;
        let op = if nonblocking {
            rustix::fs::FlockOperation::NonBlockingLockExclusive
        } else {
            rustix::fs::FlockOperation::LockExclusive
        };
        rustix::fs::flock(&lock, op)?;
        Ok(lock)
    }

    /// Missing is distinct from corrupt/unreadable. Never read an unbounded
    /// file, follow a symlink, or block while opening a named pipe.
    pub fn read(&self, max: usize) -> io::Result<Option<Vec<u8>>> {
        let file = match self.open_private(&self.name, OFlags::RDONLY) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut bytes = Vec::new();
        file.take(max.saturating_add(1) as u64).read_to_end(&mut bytes)?;
        if bytes.len() > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "state file exceeds size limit"));
        }
        Ok(Some(bytes))
    }

    pub fn write(&self, bytes: &[u8]) -> io::Result<()> {
        self.write_with(bytes, |_| Ok(()))
    }

    fn write_with(&self, bytes: &[u8], mut checkpoint: impl FnMut(Stage) -> io::Result<()>) -> io::Result<()> {
        let temp = format!(".state-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let mut file =
            self.open_private(std::ffi::OsStr::new(&temp), OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL)?;
        let result = (|| {
            file.write_all(bytes)?;
            checkpoint(Stage::Written)?;
            file.sync_all()?;
            checkpoint(Stage::FileSynced)?;
            renameat(&self.dir, &temp, &self.dir, &self.name)?;
            checkpoint(Stage::Renamed)?;
            self.dir.sync_all()
        })();
        // After rename this name no longer exists; before rename a failed
        // write leaves the previous snapshot untouched.
        let _ = unlinkat(&self.dir, &temp, AtFlags::empty());
        result
    }

    /// Keep corrupt session bytes available for manual recovery before a new
    /// autosave replaces them. Unique backups never overwrite an older one.
    pub fn backup(&self, bytes: &[u8]) -> io::Result<OsString> {
        let mut name = self.name.clone();
        name.push(format!(".corrupt-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let mut file = self.open_private(&name, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL)?;
        let result = (|| {
            file.write_all(bytes)?;
            file.sync_all()?;
            self.dir.sync_all()
        })();
        if let Err(e) = result {
            let _ = unlinkat(&self.dir, &name, AtFlags::empty());
            return Err(e);
        }
        Ok(name)
    }

    pub fn remove(&self) -> io::Result<()> {
        match unlinkat(&self.dir, &self.name, AtFlags::empty()) {
            Ok(()) => {}
            Err(rustix::io::Errno::NOENT) => {}
            Err(e) => return Err(e.into()),
        }
        self.dir.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn interrupted_writer_child() {
        let Some(path) = std::env::var_os("PROTEUS_TEST_INTERRUPTED_STATE") else { return };
        let file = StateFile::new(Path::new(&path)).unwrap();
        let _ = file.write_with(b"not committed", |stage| {
            if stage == Stage::FileSynced {
                std::process::exit(0);
            }
            Ok(())
        });
        panic!("writer should exit before rename");
    }

    #[test]
    fn process_interruption_preserves_the_last_committed_snapshot() {
        let dir = std::env::temp_dir().join(format!("proteus-state-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("session");
        let file = StateFile::new(&path).unwrap();
        file.write(b"committed").unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "persistence::tests::interrupted_writer_child"])
            .env("PROTEUS_TEST_INTERRUPTED_STATE", &path)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(StateFile::new(&path).unwrap().read(100).unwrap().unwrap(), b"committed");
        file.write(b"recovered").unwrap();
        assert_eq!(file.read(100).unwrap().unwrap(), b"recovered");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn explicit_layout_directory_permissions_are_preserved() {
        let dir = std::env::temp_dir().join(format!("proteus-layout-parent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let file = StateFile::in_directory(&dir.join("layout.lua")).unwrap();
        file.write(b"return {tabs={{}}}").unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o755);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn private_atomic_replacements_survive_failures_and_reject_hostile_files() {
        let dir = std::env::temp_dir().join(format!("proteus-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.join("snapshot");
        let file = StateFile::new(&path).unwrap();
        file.write(b"old").unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        for errno in [28, 13] {
            for fail in [Stage::Written, Stage::FileSynced] {
                let result = file.write_with(b"partial replacement", |at| {
                    if at == fail { Err(io::Error::from_raw_os_error(errno)) } else { Ok(()) }
                });
                assert!(result.is_err());
                assert_eq!(file.read(100).unwrap().unwrap(), b"old");
                assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
            }
        }
        // A directory-fsync error means publication may already have happened.
        assert!(
            file.write_with(b"published", |at| if matches!(at, Stage::Renamed) {
                Err(io::Error::other("sync failure"))
            } else {
                Ok(())
            })
            .is_err()
        );
        assert_eq!(file.read(100).unwrap().unwrap(), b"published");
        file.write(b"published").unwrap(); // Retrying also performs both syncs.
        assert!(file.read(2).is_err());
        let backup = file.backup(b"malformed original").unwrap();
        assert_eq!(std::fs::read(dir.join(backup)).unwrap(), b"malformed original");
        let lock = file.lock(true).unwrap();
        assert!(StateFile::new(&path).unwrap().lock(true).is_err());
        drop(lock);
        assert!(file.lock(true).is_ok());
        file.remove().unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, "safe").unwrap();
        symlink(&victim, &path).unwrap();
        assert!(file.read(100).is_err());
        file.write(b"replacement").unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"safe");
        file.remove().unwrap();
        std::fs::hard_link(&victim, &path).unwrap();
        assert!(file.read(100).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
