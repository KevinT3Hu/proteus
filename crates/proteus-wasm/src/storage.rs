//! A plugin's small key-value store: a TOML file next to its data
//! directory, outside the sandbox, written whole on every change.

use std::collections::BTreeMap;
use std::path::PathBuf;

pub const MAX_BYTES: usize = 1 << 20;
pub const MAX_KEY: usize = 256;

pub struct Storage {
    path: PathBuf,
    map: Option<BTreeMap<String, String>>,
}

impl Storage {
    pub fn new(path: PathBuf) -> Storage {
        Storage { path, map: None }
    }

    fn map(&mut self) -> &mut BTreeMap<String, String> {
        let path = &self.path;
        self.map.get_or_insert_with(|| {
            std::fs::read_to_string(path).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
        })
    }

    fn save(&mut self) -> Result<(), String> {
        let text = toml::to_string(self.map()).map_err(|e| e.to_string())?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &self.path)).map_err(|e| format!("storage: {e}"))
    }

    pub fn get(&mut self, key: &str) -> Option<String> {
        self.map().get(key).cloned()
    }

    pub fn keys(&mut self) -> Vec<String> {
        self.map().keys().cloned().collect()
    }

    pub fn set(&mut self, key: String, value: String) -> Result<(), String> {
        if key.is_empty() || key.len() > MAX_KEY {
            return Err(format!("storage keys are 1 to {MAX_KEY} bytes"));
        }
        let map = self.map();
        let used: usize = map.iter().filter(|(k, _)| **k != key).map(|(k, v)| k.len() + v.len()).sum();
        if used + key.len() + value.len() > MAX_BYTES {
            return Err(format!("storage is full ({} KiB); keep files in /data", MAX_BYTES >> 10));
        }
        if map.get(&key) == Some(&value) {
            return Ok(());
        }
        map.insert(key, value);
        self.save()
    }

    pub fn delete(&mut self, key: &str) {
        if self.map().remove(key).is_some() {
            let _ = self.save();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_and_limits() {
        let path = std::env::temp_dir().join(format!("proteus-storage-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut s = Storage::new(path.clone());
        s.set("a".into(), "1".into()).unwrap();
        s.set("b \"q\"".into(), "x\ny".into()).unwrap();
        assert!(s.set(String::new(), "v".into()).is_err());
        assert!(s.set("big".into(), "x".repeat(MAX_BYTES)).is_err());
        let mut again = Storage::new(path.clone());
        assert_eq!(again.get("b \"q\"").as_deref(), Some("x\ny"));
        assert_eq!(again.keys(), ["a", "b \"q\""]);
        again.delete("a");
        assert_eq!(Storage::new(path.clone()).keys(), ["b \"q\""]);
        let _ = std::fs::remove_file(&path);
    }
}
