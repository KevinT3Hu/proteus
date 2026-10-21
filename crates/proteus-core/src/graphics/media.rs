//! Kitty file, temp-file and shared-memory transmission media.
//!
//! Every failure is reported to the caller as the same opaque error so that a
//! (possibly remote) client cannot probe the filesystem through responses.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Medium {
    File,
    TempFile,
    SharedMemory,
}

/// Opaque read failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReadFailed;

const SENSITIVE: &[&str] = &["/proc", "/sys", "/dev"];
const TEMP_MARKER: &str = "tty-graphics-protocol";
const MAX_SHM_NAME: usize = 255;

fn temp_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/tmp"), PathBuf::from("/dev/shm"), std::env::temp_dir()];
    if let Some(t) = std::env::var_os("TMPDIR") {
        dirs.push(PathBuf::from(t));
    }
    dirs.into_iter().filter_map(|d| fs::canonicalize(d).ok()).collect()
}

/// Validate the path for `medium` and return what to open (and unlink afterwards).
fn resolve(medium: Medium, name: &str) -> Result<(PathBuf, bool), ReadFailed> {
    match medium {
        Medium::SharedMemory => {
            let rest = name.strip_prefix('/').ok_or(ReadFailed)?;
            if rest.is_empty() || rest.contains('/') || name.len() > MAX_SHM_NAME || rest == "." || rest == ".." {
                return Err(ReadFailed);
            }
            let path = Path::new("/dev/shm").join(rest);
            // Never follow symlinks out of the shm directory.
            let meta = fs::symlink_metadata(&path).map_err(|_| ReadFailed)?;
            if !meta.file_type().is_file() {
                return Err(ReadFailed);
            }
            Ok((path, true))
        }
        Medium::File | Medium::TempFile => {
            let path = Path::new(name);
            if !path.is_absolute() {
                return Err(ReadFailed);
            }
            // Checked before opening: opening some files has side effects.
            let canon = fs::canonicalize(path).map_err(|_| ReadFailed)?;
            let temp = medium == Medium::TempFile;
            let in_shm = canon.starts_with("/dev/shm");
            if SENSITIVE.iter().any(|s| canon.starts_with(s)) && !(temp && in_shm) {
                return Err(ReadFailed);
            }
            if !fs::metadata(&canon).map_err(|_| ReadFailed)?.is_file() {
                return Err(ReadFailed);
            }
            if temp {
                let in_temp = temp_dirs().iter().any(|d| canon.starts_with(d));
                let marked = name.contains(TEMP_MARKER) || canon.to_string_lossy().contains(TEMP_MARKER);
                if !in_temp || !marked {
                    return Err(ReadFailed);
                }
            }
            Ok((canon, temp))
        }
    }
}

/// Read `size` bytes (0 = to end of file) at `offset` from the object named by
/// `name`. Temp files and shm objects are removed after reading.
pub(crate) fn read(medium: Medium, name: &[u8], size: u64, offset: u64, max: usize) -> Result<Vec<u8>, ReadFailed> {
    let name = std::str::from_utf8(name).map_err(|_| ReadFailed)?;
    if name.contains('\0') {
        return Err(ReadFailed);
    }
    let (path, unlink) = resolve(medium, name)?;
    let result = read_at(&path, size, offset, max);
    if unlink {
        let _ = fs::remove_file(&path);
    }
    result
}

fn read_at(path: &Path, size: u64, offset: u64, max: usize) -> Result<Vec<u8>, ReadFailed> {
    let mut f = File::open(path).map_err(|_| ReadFailed)?;
    // Re-check on the open descriptor in case the path was swapped.
    let meta = f.metadata().map_err(|_| ReadFailed)?;
    if !meta.is_file() {
        return Err(ReadFailed);
    }
    let len = meta.len();
    let avail = len.checked_sub(offset).ok_or(ReadFailed)?;
    let want = if size > 0 { size } else { avail };
    if want > avail || want > max as u64 {
        return Err(ReadFailed);
    }
    f.seek(SeekFrom::Start(offset)).map_err(|_| ReadFailed)?;
    let mut buf = vec![0u8; want as usize];
    f.read_exact(&mut buf).map_err(|_| ReadFailed)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_sensitive_and_relative_paths() {
        assert_eq!(read(Medium::File, b"/proc/self/status", 0, 0, 1 << 20), Err(ReadFailed));
        assert_eq!(read(Medium::File, b"/dev/zero", 0, 0, 1 << 20), Err(ReadFailed));
        assert_eq!(read(Medium::File, b"relative.png", 0, 0, 1 << 20), Err(ReadFailed));
        assert_eq!(read(Medium::SharedMemory, b"/../etc/passwd", 0, 0, 1 << 20), Err(ReadFailed));
        assert_eq!(read(Medium::File, b"/tmp", 0, 0, 1 << 20), Err(ReadFailed));
    }

    #[test]
    fn reads_files_and_deletes_temp_files() {
        let dir = std::env::temp_dir();
        let plain = dir.join(format!("proteus-media-{}.bin", std::process::id()));
        fs::write(&plain, b"0123456789").unwrap();
        let name = plain.to_str().unwrap().as_bytes();
        assert_eq!(read(Medium::File, name, 4, 2, 100).unwrap(), b"2345");
        assert_eq!(read(Medium::File, name, 20, 0, 100), Err(ReadFailed));
        assert_eq!(read(Medium::File, name, 0, 0, 5), Err(ReadFailed));
        // Not marked as a protocol temp file: refused and kept.
        assert_eq!(read(Medium::TempFile, name, 0, 0, 100), Err(ReadFailed));
        assert!(plain.exists());
        fs::remove_file(&plain).unwrap();

        let temp = dir.join(format!("tty-graphics-protocol-{}.bin", std::process::id()));
        fs::write(&temp, b"abc").unwrap();
        assert_eq!(read(Medium::TempFile, temp.to_str().unwrap().as_bytes(), 0, 0, 100).unwrap(), b"abc");
        assert!(!temp.exists());
    }
}
