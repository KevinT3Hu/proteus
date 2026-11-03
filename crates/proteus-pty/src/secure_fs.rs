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

/// A user private group (as created with umask 002 setups): named after the
/// effective user, with no other members and no other user's primary group.
fn is_private_group(gid: u32) -> bool {
    static PRIVATE: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *PRIVATE.get_or_init(|| {
        let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
        let group = std::fs::read_to_string("/etc/group").ok()?;
        private_group_of(rustix::process::geteuid().as_raw(), &passwd, &group)
    }) == Some(gid)
}

fn private_group_of(uid: u32, passwd: &str, group: &str) -> Option<u32> {
    let users: Vec<Vec<&str>> =
        passwd.lines().map(|l| l.split(':').collect()).filter(|f: &Vec<_>| f.len() >= 4).collect();
    let me = users.iter().find(|f| f[2].parse() == Ok(uid))?;
    let (name, gid) = (me[0], me[3].parse::<u32>().ok()?);
    if users.iter().any(|f| f[0] != name && f[3].parse() == Ok(gid)) {
        return None;
    }
    let entries: Vec<Vec<&str>> =
        group.lines().map(|l| l.split(':').collect()).filter(|f: &Vec<_>| f.len() >= 4).collect();
    let mut matching = entries.iter().filter(|f| f[2].parse() == Ok(gid));
    let entry = matching.next()?;
    let alone = entry[3].split(',').all(|m| m.is_empty() || m == name);
    (entry[0] == name && alone && matching.next().is_none()).then_some(gid)
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
        let uid = rustix::process::geteuid().as_raw();
        let group_ok = |bits: u32| m.st_mode & bits == 0 || (m.st_uid == uid && is_private_group(m.st_gid));
        let private = m.st_uid == uid && m.st_mode & 0o007 == 0 && group_ok(0o070);
        let safe_ancestor = (m.st_uid == 0 || m.st_uid == uid)
            && ((m.st_mode & 0o002 == 0 && group_ok(0o020)) || m.st_mode & 0o1000 != 0);
        if (final_dir && private_final && !private) || ((!final_dir || !private_final) && !safe_ancestor) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "untrusted application directory"));
        }
    }
    if parts.is_empty() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(fd.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_groups_are_named_after_the_user_and_have_no_other_members() {
        let passwd =
            "root:x:0:0::/root:/bin/sh\nann:x:1000:1000::/home/ann:/bin/sh\nbob:x:1001:100::/home/bob:/bin/sh\n";
        assert_eq!(private_group_of(1000, passwd, "ann:x:1000:\nusers:x:100:bob\n"), Some(1000));
        assert_eq!(private_group_of(1000, passwd, "ann:x:1000:ann\n"), Some(1000));
        assert_eq!(private_group_of(1000, passwd, "ann:x:1000:bob\n"), None, "other member");
        assert_eq!(private_group_of(1000, passwd, "staff:x:1000:\n"), None, "not named after the user");
        assert_eq!(private_group_of(1001, passwd, "users:x:100:\n"), None, "shared primary group");
        let shared = format!("{passwd}eve:x:1002:1000::/home/eve:/bin/sh\n");
        assert_eq!(private_group_of(1000, &shared, "ann:x:1000:\n"), None, "another user's primary group");
    }
}
