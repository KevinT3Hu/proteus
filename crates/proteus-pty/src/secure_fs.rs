//! Private application directories, opened without following symlinks.
use rustix::fs::{Mode, OFlags, mkdirat, open, openat};
use std::fs::File;
use std::io;
use std::path::{Component, Path};

/// Existing ancestors must not be replaceable by other users. Sticky shared
/// ancestors (such as /tmp) are allowed; the final directory must be private.
pub fn private_dir(path: &Path) -> io::Result<File> {
    directory(path, false, true)
}

/// Migrate an existing user-owned state directory to 0700. Unlike runtime
/// script directories, old state directories may have been created with 0755.
pub fn state_dir(path: &Path) -> io::Result<File> {
    directory(path, true, true)
}

/// A safe parent for an explicitly chosen file. Preserve its permissions;
/// saving a layout in a source checkout must not chmod the checkout itself.
pub fn trusted_dir(path: &Path) -> io::Result<File> {
    directory(path, false, false)
}

fn directory(path: &Path, migrate: bool, private_final: bool) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut fd = open("/", OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
    let parts: Vec<_> = path.components().filter(|c| !matches!(c, Component::RootDir)).collect();
    for (i, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err(io::ErrorKind::InvalidInput.into());
        };
        match mkdirat(&fd, *name, Mode::from_raw_mode(0o700)) {
            Ok(()) => {
                if migrate || !private_final {
                    rustix::fs::fsync(&fd)?;
                }
            }
            Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
        fd =
            openat(&fd, *name, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?;
        let m = rustix::fs::fstat(&fd)?;
        let final_dir = i + 1 == parts.len();
        if final_dir && migrate && m.st_uid == rustix::process::geteuid().as_raw() {
            rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o700))?;
        }
        let m = rustix::fs::fstat(&fd)?;
        let private = m.st_uid == rustix::process::geteuid().as_raw() && m.st_mode & 0o077 == 0;
        let safe_ancestor = (m.st_uid == 0 || m.st_uid == rustix::process::geteuid().as_raw())
            && (m.st_mode & 0o022 == 0 || m.st_mode & 0o1000 != 0);
        if (final_dir && private_final && !private) || ((!final_dir || !private_final) && !safe_ancestor) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "untrusted application directory"));
        }
    }
    if parts.is_empty() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(fd.into())
}
