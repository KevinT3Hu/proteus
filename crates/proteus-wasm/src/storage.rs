//! A plugin's transactional key-value store. Each operation reloads disk state
//! under a stable sidecar lock, so separate terminal processes cannot overwrite
//! each other's updates. Failed writes never leave a divergent in-memory cache.
use std::collections::BTreeMap;
use std::path::PathBuf;

use proteus_pty::persistence::StateFile;

pub const MAX_BYTES: usize = 1 << 20;
pub const MAX_KEY: usize = 256;
// TOML quoting can expand each logical byte (e.g. control characters) to six.
const MAX_FILE_BYTES: usize = MAX_BYTES * 8;

pub struct Storage {
    path: PathBuf,
}

impl Storage {
    pub fn new(path: PathBuf) -> Storage {
        Storage { path }
    }

    fn load(file: &StateFile) -> Result<BTreeMap<String, String>, String> {
        let Some(bytes) = file.read(MAX_FILE_BYTES).map_err(|e| format!("storage: {e}"))? else {
            return Ok(BTreeMap::new());
        };
        let text = std::str::from_utf8(&bytes).map_err(|e| format!("storage: invalid UTF-8: {e}"))?;
        let map: BTreeMap<String, String> =
            toml::from_str(text).map_err(|e| format!("storage: malformed TOML: {e}"))?;
        if map.keys().any(|k| k.is_empty() || k.len() > MAX_KEY)
            || map.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() > MAX_BYTES
        {
            return Err("storage: persisted data exceeds key or size limits".into());
        }
        Ok(map)
    }

    fn file(&self) -> Result<StateFile, String> {
        StateFile::new(&self.path).map_err(|e| format!("storage {}: {e}", self.path.display()))
    }

    pub fn get(&self, key: &str) -> Result<Option<String>, String> {
        let file = self.file()?;
        let _lock = file.lock(false).map_err(|e| format!("storage lock: {e}"))?;
        Ok(Self::load(&file)?.remove(key))
    }

    pub fn keys(&self) -> Result<Vec<String>, String> {
        let file = self.file()?;
        let _lock = file.lock(false).map_err(|e| format!("storage lock: {e}"))?;
        Ok(Self::load(&file)?.into_keys().collect())
    }

    fn update(&self, edit: impl FnOnce(&mut BTreeMap<String, String>) -> Result<(), String>) -> Result<(), String> {
        self.update_with(edit, |file, text| file.write(text))
    }

    fn update_with(
        &self,
        edit: impl FnOnce(&mut BTreeMap<String, String>) -> Result<(), String>,
        persist: impl FnOnce(&StateFile, &[u8]) -> std::io::Result<()>,
    ) -> Result<(), String> {
        let file = self.file()?;
        let _lock = file.lock(false).map_err(|e| format!("storage lock: {e}"))?;
        let mut next = Self::load(&file)?;
        edit(&mut next)?;
        let text = toml::to_string(&next).map_err(|e| format!("storage: {e}"))?;
        if text.len() > MAX_FILE_BYTES {
            return Err("storage: serialized data exceeds file size limit".into());
        }
        // Always persist, even an identical retry: a prior failure may have
        // occurred AFTER rename but before the directory was synchronized.
        persist(&file, text.as_bytes()).map_err(|e| format!("storage: {e}"))
    }

    pub fn set(&self, key: String, value: String) -> Result<(), String> {
        if key.is_empty() || key.len() > MAX_KEY {
            return Err(format!("storage keys are 1 to {MAX_KEY} bytes"));
        }
        self.update(|map| {
            let used: usize = map.iter().filter(|(k, _)| **k != key).map(|(k, v)| k.len() + v.len()).sum();
            if used + key.len() + value.len() > MAX_BYTES {
                return Err(format!("storage is full ({} KiB); keep files in /data", MAX_BYTES >> 10));
            }
            map.insert(key, value);
            Ok(())
        })
    }

    pub fn delete(&self, key: &str) -> Result<(), String> {
        self.update(|map| {
            map.remove(key);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            Self(std::env::temp_dir().join(format!(
                "proteus-storage-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )))
        }
        fn store(&self) -> Storage {
            Storage::new(self.0.join("state.toml"))
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn persists_and_limits() {
        let t = Temp::new();
        let s = t.store();
        s.set("a".into(), "1".into()).unwrap();
        s.set("b \"q\"".into(), "x\ny".into()).unwrap();
        assert!(s.set(String::new(), "v".into()).is_err());
        assert!(s.set("big".into(), "x".repeat(MAX_BYTES)).is_err());
        let again = t.store();
        assert_eq!(again.get("b \"q\"").unwrap().as_deref(), Some("x\ny"));
        assert_eq!(again.keys().unwrap(), ["a", "b \"q\""]);
        again.delete("a").unwrap();
        assert_eq!(t.store().keys().unwrap(), ["b \"q\""]);
    }

    #[test]
    fn failed_updates_are_not_committed_and_identical_values_retry_persistence() {
        let t = Temp::new();
        let s = t.store();
        s.set("key".into(), "original".into()).unwrap();
        for delete in [false, true] {
            let failure = s.update_with(
                |map| {
                    if delete {
                        map.remove("key");
                    } else {
                        map.insert("key".into(), "new".into());
                    }
                    Ok(())
                },
                |_, _| Err(std::io::Error::from_raw_os_error(28)),
            );
            assert!(failure.is_err());
            assert_eq!(s.get("key").unwrap().as_deref(), Some("original"));
            assert_eq!(t.store().get("key").unwrap().as_deref(), Some("original"));
        }
        // Simulate a directory sync error after a complete atomic publication.
        assert!(
            s.update_with(
                |map| {
                    map.insert("key".into(), "new".into());
                    Ok(())
                },
                |file, bytes| {
                    file.write(bytes)?;
                    Err(std::io::Error::other("directory sync failed"))
                }
            )
            .is_err()
        );
        assert_eq!(s.get("key").unwrap().as_deref(), Some("new"));
        let mut persisted = false;
        s.update_with(
            |_| Ok(()),
            |file, bytes| {
                persisted = true;
                file.write(bytes)
            },
        )
        .unwrap();
        assert!(persisted);
        s.delete("key").unwrap();
        assert_eq!(s.get("key").unwrap(), None);
    }

    #[test]
    fn malformed_and_oversized_state_is_reported_and_never_overwritten() {
        let t = Temp::new();
        let s = t.store();
        s.set("key".into(), "saved".into()).unwrap();
        for invalid in [b"not = [valid".to_vec(), vec![0xff], vec![b'x'; MAX_FILE_BYTES + 1]] {
            std::fs::write(&s.path, &invalid).unwrap();
            assert!(s.get("key").is_err());
            assert!(s.keys().is_err());
            assert!(s.set("new".into(), "value".into()).is_err());
            assert!(s.delete("key").is_err());
            assert_eq!(std::fs::read(&s.path).unwrap(), invalid);
        }
        std::fs::write(&s.path, "key = 'repaired'\n").unwrap();
        s.set("new".into(), "value".into()).unwrap();
        assert_eq!(s.get("key").unwrap().as_deref(), Some("repaired"));
    }

    #[test]
    fn concurrent_process_worker() {
        let Some(path) = std::env::var_os("PROTEUS_TEST_STORAGE_PATH") else { return };
        let s = Storage::new(path.into());
        let worker = std::env::var("PROTEUS_TEST_STORAGE_WORKER").unwrap();
        for i in 0..10 {
            s.set(format!("{worker}-{i}"), i.to_string()).unwrap();
        }
    }

    #[test]
    fn independent_processes_do_not_lose_updates() {
        let t = Temp::new();
        let executable = std::env::current_exe().unwrap();
        let workers: Vec<_> = (0..4)
            .map(|worker| {
                std::process::Command::new(&executable)
                    .args(["--exact", "storage::tests::concurrent_process_worker"])
                    .env("PROTEUS_TEST_STORAGE_PATH", &t.store().path)
                    .env("PROTEUS_TEST_STORAGE_WORKER", worker.to_string())
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for mut w in workers {
            assert!(w.wait().unwrap().success());
        }
        assert_eq!(t.store().keys().unwrap().len(), 40);
    }

    #[test]
    fn concurrent_instances_merge_updates_under_the_same_lock() {
        let t = Temp::new();
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                let s = t.store();
                std::thread::spawn(move || {
                    for i in 0..10 {
                        s.set(format!("{worker}-{i}"), i.to_string()).unwrap();
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        assert_eq!(t.store().keys().unwrap().len(), 80);
    }
}
