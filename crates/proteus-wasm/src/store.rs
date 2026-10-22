//! Installed plugins: `$XDG_DATA_HOME/proteus/plugins/ID/` (the manifest,
//! `plugin.wasm`, `assets/`), each plugin's own data directory
//! `plugin-data/ID/`, and `plugins.lock` recording the source, the hash
//! of the component, whether it is enabled and the permissions granted.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::manifest::{Manifest, Permissions};

/// One installed plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub version: String,
    /// Where it was installed from (a path, git URL or archive).
    pub source: String,
    /// `sha256:HEX` of `plugin.wasm`; checked before every load.
    pub hash: String,
    pub enabled: bool,
    /// What the user approved (a subset of what the manifest asks for).
    pub granted: Permissions,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    #[serde(default, rename = "plugin")]
    pub plugins: Vec<Entry>,
}

impl Lock {
    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.plugins.iter().find(|e| e.id == id)
    }

    fn get_mut(&mut self, id: &str) -> Result<&mut Entry, String> {
        self.plugins.iter_mut().find(|e| e.id == id).ok_or_else(|| format!("no plugin `{id}` is installed"))
    }
}

/// The user's answer to "install with these permissions?".
pub type Approve<'a> = &'a mut dyn FnMut(&Manifest, &Permissions) -> bool;

pub struct Store {
    root: PathBuf,
}

/// `$XDG_DATA_HOME/proteus` (or `~/.local/share/proteus`).
pub fn default_root() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("proteus"))
}

pub fn sha256(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    let hex: String = d.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

impl Store {
    pub fn new(root: PathBuf) -> Store {
        Store { root }
    }

    pub fn open_default() -> Option<Store> {
        default_root().map(Store::new)
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root.join("plugins.lock")
    }

    pub fn plugin_dir(&self, id: &str) -> PathBuf {
        self.root.join("plugins").join(id)
    }

    /// The plugin's own files, `/data` inside the sandbox.
    pub fn data_dir(&self, id: &str) -> PathBuf {
        self.root.join("plugin-data").join(id)
    }

    /// The plugin's key-value store (outside its sandbox).
    pub fn storage_path(&self, id: &str) -> PathBuf {
        self.root.join("plugin-data").join(format!("{id}.storage.toml"))
    }

    /// Compiled components (native code), named by the wasm's hash.
    pub fn compiled_dir(&self) -> PathBuf {
        self.root.join("compiled")
    }

    pub fn component_path(&self, id: &str) -> PathBuf {
        self.plugin_dir(id).join("plugin.wasm")
    }

    /// The lock file (empty if there is none).
    pub fn read(&self) -> Result<Lock, String> {
        let path = self.lock_path();
        match std::fs::read_to_string(&path) {
            Ok(t) => toml::from_str(&t).map_err(|e| format!("{}: {}", path.display(), e.message())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Lock::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn write(&self, lock: &Lock) -> Result<(), String> {
        let path = self.lock_path();
        let text = format!(
            "# Installed Proteus plugins. Managed by `proteus +plugin`; edits are picked up.\n{}",
            toml::to_string(lock).map_err(|e| e.to_string())?
        );
        std::fs::create_dir_all(&self.root).map_err(|e| format!("{}: {e}", self.root.display()))?;
        let tmp = path.with_extension("lock.tmp");
        std::fs::write(&tmp, text)
            .and_then(|_| std::fs::rename(&tmp, &path))
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The component's bytes, if they still match the lock.
    pub fn load_component(&self, e: &Entry) -> Result<Vec<u8>, String> {
        let path = self.component_path(&e.id);
        let bytes = std::fs::read(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        if sha256(&bytes) != e.hash {
            return Err(format!("{}: changed since it was installed (hash mismatch); reinstall it", path.display()));
        }
        Ok(bytes)
    }

    /// Install (or update) from `source`: a directory, a git URL or an
    /// archive. `approve` is asked about the permissions not granted yet;
    /// declining leaves everything as it was.
    pub fn install(&self, source: &str, approve: Approve<'_>) -> Result<Entry, String> {
        let fetched = Fetched::get(source)?;
        let dir = fetched.dir();
        let manifest = Manifest::load(dir)?;
        let wasm_src = dir.join(&manifest.wasm);
        let wasm = std::fs::read(&wasm_src).map_err(|e| format!("{}: {e}", wasm_src.display()))?;
        if !wasm.starts_with(b"\0asm") {
            return Err(format!("{}: not a WebAssembly file", wasm_src.display()));
        }
        let mut lock = self.read()?;
        let old = lock.get(&manifest.id).cloned();
        let granted = old.as_ref().map(|e| e.granted.clone()).unwrap_or_default();
        let asks = manifest.permissions.beyond(&granted);
        if (!asks.is_empty() || old.is_none()) && !approve(&manifest, &asks) {
            return Err("not installed: permissions declined".into());
        }
        // Copy into place: stage, then swap.
        let target = self.plugin_dir(&manifest.id);
        let stage = self.root.join("plugins").join(format!(".{}.new", manifest.id));
        let io = |e: std::io::Error| format!("{}: {e}", stage.display());
        let _ = std::fs::remove_dir_all(&stage);
        std::fs::create_dir_all(&stage).map_err(io)?;
        std::fs::copy(dir.join("proteus-plugin.toml"), stage.join("proteus-plugin.toml")).map_err(io)?;
        std::fs::write(stage.join("plugin.wasm"), &wasm).map_err(io)?;
        if dir.join("assets").is_dir() {
            copy_dir(&dir.join("assets"), &stage.join("assets")).map_err(io)?;
        }
        let _ = std::fs::remove_dir_all(&target);
        std::fs::rename(&stage, &target).map_err(io)?;
        std::fs::create_dir_all(self.data_dir(&manifest.id)).map_err(io)?;
        let source = match Path::new(source).canonicalize() {
            Ok(p) if p.is_dir() || p.is_file() => p.display().to_string(),
            _ => source.to_string(),
        };
        // What it asks for and was approved; a revoked permission it no
        // longer asks for stays revoked.
        let mut granted = manifest.permissions.clone();
        granted.fs.retain(|f| manifest.permissions.fs.contains(f));
        let entry = Entry {
            id: manifest.id.clone(),
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            source,
            hash: sha256(&wasm),
            enabled: old.as_ref().is_none_or(|e| e.enabled),
            granted,
        };
        lock.plugins.retain(|e| e.id != entry.id);
        lock.plugins.push(entry.clone());
        self.write(&lock)?;
        Ok(entry)
    }

    /// Install again from where it came from.
    pub fn update(&self, id: &str, approve: Approve<'_>) -> Result<Entry, String> {
        let source = self.read()?.get(id).map(|e| e.source.clone()).ok_or_else(|| format!("no plugin `{id}`"))?;
        let e = self.install(&source, approve)?;
        if e.id != id {
            return Err(format!("{source} now holds plugin `{}`, not `{id}`", e.id));
        }
        Ok(e)
    }

    /// Uninstall; `keep_data` leaves its data directory.
    pub fn remove(&self, id: &str, keep_data: bool) -> Result<(), String> {
        let mut lock = self.read()?;
        lock.get_mut(id)?;
        lock.plugins.retain(|e| e.id != id);
        self.write(&lock)?;
        let _ = std::fs::remove_dir_all(self.plugin_dir(id));
        if !keep_data {
            let _ = std::fs::remove_dir_all(self.data_dir(id));
            let _ = std::fs::remove_file(self.storage_path(id));
        }
        Ok(())
    }

    pub fn set_enabled(&self, id: &str, on: bool) -> Result<(), String> {
        let mut lock = self.read()?;
        lock.get_mut(id)?.enabled = on;
        self.write(&lock)
    }

    pub fn revoke(&self, id: &str, permission: &str) -> Result<(), String> {
        let mut lock = self.read()?;
        if !lock.get_mut(id)?.granted.revoke(permission) {
            return Err(format!("`{id}` was not granted `{permission}`"));
        }
        self.write(&lock)
    }
}

/// A plugin source on disk; a temporary checkout is removed on drop.
struct Fetched {
    dir: PathBuf,
    temp: Option<PathBuf>,
}

impl Fetched {
    fn get(source: &str) -> Result<Fetched, String> {
        let path = Path::new(source);
        if path.is_dir() {
            return Ok(Fetched { dir: path.to_path_buf(), temp: None });
        }
        let temp = std::env::temp_dir().join(format!("proteus-plugin-{}-{}", std::process::id(), unique()));
        let f = Fetched { dir: temp.clone(), temp: Some(temp.clone()) };
        let lower = source.to_ascii_lowercase();
        let archive = [".tar", ".tar.gz", ".tgz", ".tar.xz", ".tar.zst", ".tar.bz2", ".zip"]
            .iter()
            .any(|ext| lower.ends_with(ext));
        let status = if path.is_file() && archive {
            std::fs::create_dir_all(&temp).map_err(|e| e.to_string())?;
            let mut c = if lower.ends_with(".zip") {
                let mut c = Command::new("unzip");
                c.arg("-q").arg(path).arg("-d").arg(&temp);
                c
            } else {
                let mut c = Command::new("tar");
                c.arg("-xf").arg(path).arg("-C").arg(&temp);
                c
            };
            c.status()
        } else if ["https://", "http://", "ssh://", "git://", "git@", "file://"].iter().any(|p| lower.starts_with(p))
            || lower.ends_with(".git")
        {
            Command::new("git").args(["clone", "--quiet", "--depth", "1", "--"]).arg(source).arg(&temp).status()
        } else {
            return Err(format!("{source}: not a directory, archive or git URL"));
        };
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => return Err(format!("{source}: fetching failed ({s})")),
            Err(e) => return Err(format!("{source}: {e}")),
        }
        // An archive with one top directory: the plugin is inside it.
        let mut f = f;
        if !f.dir.join("proteus-plugin.toml").exists()
            && let Ok(rd) = std::fs::read_dir(&f.dir)
        {
            let entries: Vec<_> = rd.flatten().collect();
            if let [only] = entries.as_slice()
                && only.path().is_dir()
            {
                f.dir = only.path();
            }
        }
        Ok(f)
    }

    fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Fetched {
    fn drop(&mut self) {
        if let Some(t) = &self.temp {
            let _ = std::fs::remove_dir_all(t);
        }
    }
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let ty = e.file_type()?;
        // Symlinks are not followed: a package cannot pull in other files.
        if ty.is_dir() {
            copy_dir(&e.path(), &to.join(e.file_name()))?;
        } else if ty.is_file() {
            std::fs::copy(e.path(), to.join(e.file_name()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(dir: &Path, perms: &str, wasm: &[u8]) {
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(
            dir.join("proteus-plugin.toml"),
            format!(
                "[plugin]\nid = \"t.demo\"\nname = \"Demo\"\nversion = \"1.0\"\napi = \"1\"\n[permissions]\n{perms}"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("plugin.wasm"), wasm).unwrap();
        std::fs::write(dir.join("assets/a.txt"), "x").unwrap();
    }

    #[test]
    fn install_update_remove() {
        let base = std::env::temp_dir().join(format!("proteus-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (src, store) = (base.join("src"), Store::new(base.join("data")));
        package(&src, "ui = true", b"\0asm1");
        let src_s = src.display().to_string();

        let mut asked = Vec::new();
        let e = store
            .install(&src_s, &mut |m, p| {
                asked.push((m.id.clone(), p.names()));
                true
            })
            .unwrap();
        assert_eq!(asked, [("t.demo".to_string(), vec!["ui".to_string()])]);
        assert!(e.enabled && e.granted.ui && e.hash.starts_with("sha256:"));
        assert!(store.plugin_dir("t.demo").join("assets/a.txt").exists() && store.data_dir("t.demo").is_dir());
        assert_eq!(store.load_component(&e).unwrap(), b"\0asm1");

        // An update asking nothing new is not asked about.
        package(&src, "ui = true", b"\0asm2");
        store.update("t.demo", &mut |_, _| panic!("asked")).unwrap();
        // One asking for more is; declining keeps the old version.
        package(&src, "ui = true\nwrite-pty = true", b"\0asm3");
        assert!(store.update("t.demo", &mut |_, p| p.names() == ["write-pty"] && false).is_err());
        let e = store.read().unwrap().get("t.demo").cloned().unwrap();
        assert_eq!(store.load_component(&e).unwrap(), b"\0asm2");

        store.set_enabled("t.demo", false).unwrap();
        store.revoke("t.demo", "ui").unwrap();
        let e = store.read().unwrap().get("t.demo").cloned().unwrap();
        assert!(!e.enabled && !e.granted.ui);
        assert!(store.revoke("t.demo", "ui").is_err());

        // Tampering is noticed.
        std::fs::write(store.component_path("t.demo"), b"\0asmX").unwrap();
        assert!(store.load_component(&e).unwrap_err().contains("hash mismatch"));

        // Archives work too.
        let tar = base.join("demo.tar.gz");
        let ok = Command::new("tar").arg("-czf").arg(&tar).arg("-C").arg(&base).arg("src").status().unwrap();
        assert!(ok.success());
        store.install(&tar.display().to_string(), &mut |_, _| true).unwrap();

        store.remove("t.demo", false).unwrap();
        assert!(!store.plugin_dir("t.demo").exists() && !store.data_dir("t.demo").exists());
        assert!(store.read().unwrap().plugins.is_empty());
        assert!(store.remove("t.demo", false).is_err());
        assert!(store.install(&base.join("nope").display().to_string(), &mut |_, _| true).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
