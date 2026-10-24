//! Explicit plugin sources and a disposable fetch/build cache. No discovery or registry.
use crate::{Manifest, Permissions};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Source {
    /// Opaque plugin-owned settings; never interpreted by the host.
    pub config: serde_json::Value,
    pub source: String,
    pub rev: Option<String>,
    /// Expected SHA-256 of the final WASM artifact.
    pub sha256: Option<String>,
    pub subdir: Option<PathBuf>,
    pub id: Option<String>,
    pub permissions: Option<Permissions>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub id: String,
    pub name: String,
    pub version: String,
    pub source: Source,
    pub hash: String,
    pub granted: Permissions,
    pub wasm: Arc<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct Cache {
    pub root: PathBuf,
    pub data: PathBuf,
}

pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn home_base(var: &str, fallback: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback)))
}
impl Cache {
    pub fn default_paths() -> Option<Self> {
        Some(Self {
            root: std::env::var_os("PROTEUS_CACHE_DIR")
                .map(PathBuf::from)
                .or_else(|| home_base("XDG_CACHE_HOME", ".cache").map(|p| p.join("proteus/plugins")))?,
            data: std::env::var_os("PROTEUS_DATA_DIR")
                .map(PathBuf::from)
                .or_else(|| home_base("XDG_DATA_HOME", ".local/share").map(|p| p.join("proteus")))?,
        })
    }
    pub fn data_dir(&self, id: &str) -> PathBuf {
        self.data.join("plugin-data").join(id)
    }
    pub fn storage_path(&self, id: &str) -> PathBuf {
        self.data.join("plugin-data").join(format!("{id}.storage.toml"))
    }
    pub fn compiled_dir(&self) -> PathBuf {
        self.root.join("compiled")
    }

    /// Materialize exactly this path/URL. Cached remote snapshots are reused until removed.
    pub fn materialize(&self, source: &Source) -> Result<PathBuf, String> {
        let raw = &source.source;
        let remote = ["http://", "https://", "ssh://", "git://", "git@", "file://"].iter().any(|p| raw.starts_with(p));
        if !remote && source.rev.is_some() {
            return Err("rev is only supported for Git URLs".into());
        }
        let mut path = if !remote {
            PathBuf::from(raw)
        } else {
            let key = sha256(format!("{}\0{:?}", raw, source.rev).as_bytes());
            let slot = self.root.join("sources").join(key);
            std::fs::create_dir_all(&slot).map_err(|e| e.to_string())?;
            let _guard = cache_lock(&slot)?;
            let suffix = raw.split(['?', '#']).next().unwrap_or(raw);
            let direct = suffix.ends_with(".wasm");
            if direct {
                if source.rev.is_some() {
                    return Err("rev is only supported for Git sources".into());
                }
                let file = slot.join("plugin.wasm");
                if !file.exists() {
                    let cfg = ureq::Agent::config_builder()
                        .timeout_global(Some(Duration::from_secs(60)))
                        .tls_config(
                            ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build(),
                        )
                        .build();
                    let mut response = cfg.new_agent().get(raw).call().map_err(|e| format!("fetch {raw}: {e}"))?;
                    let bytes = response
                        .body_mut()
                        .with_config()
                        .limit(256 * 1024 * 1024)
                        .read_to_vec()
                        .map_err(|e| e.to_string())?;
                    if !bytes.starts_with(b"\0asm") {
                        return Err(format!("{raw}: not WebAssembly"));
                    }
                    atomic_write(&file, &bytes)?;
                }
                file
            } else {
                let checkout = slot.join("checkout");
                if !checkout.exists() {
                    let stage = slot.join("checkout.tmp");
                    if stage.exists() {
                        std::fs::remove_dir_all(&stage).map_err(|e| e.to_string())?;
                    }
                    command(
                        Command::new("git").args(["clone", "--quiet", "--no-checkout", "--"]).arg(raw).arg(&stage),
                        "git clone",
                    )?;
                    let rev = source.rev.as_deref().unwrap_or("HEAD");
                    if rev.starts_with('-') {
                        return Err("rev must not start with '-'".into());
                    }
                    command(
                        Command::new("git").arg("-C").arg(&stage).args(["checkout", "--quiet", "--detach", rev, "--"]),
                        "git checkout",
                    )?;
                    std::fs::rename(&stage, &checkout).map_err(|e| e.to_string())?;
                }
                checkout
            }
        };
        if let Some(subdir) = &source.subdir {
            if subdir.is_absolute() || subdir.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                return Err("plugin subdir must be relative and cannot contain '..'".into());
            }
            path = path.join(subdir);
        }
        path.canonicalize().map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Rust source is always built through Cargo's incremental cache, never a stale sibling WASM.
    pub fn resolve(&self, source: &Source) -> Result<Resolved, String> {
        if source.sha256.as_ref().is_some_and(|h| h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit())) {
            return Err("plugin sha256 must contain exactly 64 hexadecimal digits".into());
        }
        let path = self.materialize(source)?;
        let direct = path.extension().is_some_and(|s| s == "wasm");
        let dir = if path.is_dir() {
            path.clone()
        } else {
            path.parent().ok_or("plugin has no parent directory")?.to_path_buf()
        };
        let manifest = if dir.join("proteus-plugin.toml").is_file() { Some(Manifest::load(&dir)?) } else { None };
        let (id, name, version, requested) = match manifest.as_ref() {
            Some(m) => {
                if source.id.as_ref().is_some_and(|id| id != &m.id) {
                    return Err(format!("plugin ID does not match manifest `{}`", m.id));
                }
                (m.id.clone(), m.name.clone(), m.version.clone(), m.permissions.clone())
            }
            None if direct => {
                let id =
                    source.id.clone().unwrap_or_else(|| format!("plugin-{}", &sha256(source.source.as_bytes())[..16]));
                (id.clone(), id, "0".into(), Permissions::default())
            }
            None => {
                return Err(format!(
                    "{}: expected proteus-plugin.toml for a WASM source package; Lua modules belong in require()",
                    dir.display()
                ));
            }
        };
        if !crate::manifest::valid_plugin_id(&id) {
            return Err(format!("invalid plugin ID `{id}`"));
        }
        let granted = source.permissions.clone().unwrap_or(requested);
        for grant in &granted.fs {
            grant.validate_directory()?;
        }
        let artifact = if direct {
            path
        } else if dir.join("Cargo.toml").is_file() {
            self.build(&dir)?
        } else {
            dir.join(&manifest.as_ref().ok_or("missing manifest")?.wasm)
        };
        let bytes = std::fs::read(&artifact).map_err(|e| format!("{}: {e}", artifact.display()))?;
        if !bytes.starts_with(b"\0asm") {
            return Err(format!("{}: not WebAssembly", artifact.display()));
        }
        let hash = sha256(&bytes);
        if source.sha256.as_ref().is_some_and(|expected| !expected.eq_ignore_ascii_case(&hash)) {
            return Err(format!(
                "plugin checksum mismatch: expected {}, got {hash}",
                source.sha256.as_deref().unwrap()
            ));
        }
        let cached = self.root.join("artifacts").join(format!("{hash}.wasm"));
        atomic_write(&cached, &bytes)?;
        Ok(Resolved { id, name, version, source: source.clone(), hash, granted, wasm: Arc::new(bytes) })
    }

    fn build(&self, dir: &Path) -> Result<PathBuf, String> {
        self.build_with_cargo(dir, std::ffi::OsStr::new("cargo"))
    }

    fn build_with_cargo(&self, dir: &Path, cargo: &std::ffi::OsStr) -> Result<PathBuf, String> {
        let slot = self.root.join("build").join(sha256(dir.as_os_str().as_encoded_bytes()));
        std::fs::create_dir_all(&slot).map_err(|e| e.to_string())?;
        let _guard = cache_lock(&slot)?;
        let manifest = dir.join("Cargo.toml").canonicalize().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(cargo);
        cmd.current_dir(dir)
            .args([
                "build",
                "--release",
                "--target",
                "wasm32-wasip2",
                "--message-format=json-render-diagnostics",
                "--manifest-path",
            ])
            .arg(&manifest)
            .arg("--target-dir")
            .arg(slot.join("target"));
        let out = cmd.output().map_err(|e| format!("cargo build: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "plugin build in {} failed (requires the wasm32-wasip2 Rust target):\n{}",
                dir.display(),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        let mut artifacts = Vec::new();
        for line in out.stdout.split(|&b| b == b'\n') {
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else { continue };
            if v["reason"] != "compiler-artifact"
                || v["manifest_path"].as_str().map(Path::new) != Some(manifest.as_path())
            {
                continue;
            }
            if let Some(files) = v["filenames"].as_array() {
                for f in files.iter().filter_map(|v| v.as_str()) {
                    let p = PathBuf::from(f);
                    if p.extension().is_some_and(|s| s == "wasm") && !artifacts.contains(&p) {
                        artifacts.push(p);
                    }
                }
            }
        }
        match artifacts.as_slice() {
            [only] => Ok(only.clone()),
            _ => Err("plugin source must produce exactly one WASM artifact; select the package with subdir".into()),
        }
    }
}

fn command(cmd: &mut Command, label: &str) -> Result<(), String> {
    let out = cmd.output().map_err(|e| format!("{label}: {e}"))?;
    if out.status.success() { Ok(()) } else { Err(format!("{label}: {}", String::from_utf8_lossy(&out.stderr))) }
}
fn cache_lock(dir: &Path) -> Result<std::fs::File, String> {
    let f = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(".lock"))
        .map_err(|e| e.to_string())?;
    rustix::fs::flock(&f, rustix::fs::FlockOperation::LockExclusive).map_err(|e| e.to_string())?;
    Ok(f)
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().ok_or("cache path has no parent")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let temp = dir.join(format!(".tmp-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let result = (|| {
        let mut f = std::fs::File::options().write(true).create_new(true).open(&temp)?;
        f.write_all(bytes)?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "proteus-sources-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn cache(&self) -> Cache {
            Cache { root: self.0.join("cache"), data: self.0.join("data") }
        }
        fn package(&self) -> PathBuf {
            let p = self.0.join("package");
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(
                p.join("proteus-plugin.toml"),
                "[plugin]\nid='t.demo'\nname='Demo'\nversion='1'\napi='1'\n[permissions]\nui=true\n",
            )
            .unwrap();
            std::fs::write(p.join("plugin.wasm"), b"\0asm\x0d\0\x01\0").unwrap();
            p
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn pinned_artifacts_reject_changes_and_malformed_checksums() {
        let t = Temp::new();
        let cache = t.cache();
        let path = t.package();
        let mut source = Source { source: path.display().to_string(), ..Default::default() };
        let first = cache.resolve(&source).unwrap();
        source.sha256 = Some(first.hash.to_ascii_uppercase());
        assert_eq!(cache.resolve(&source).unwrap().hash, first.hash);
        std::fs::write(path.join("plugin.wasm"), b"\0asmchanged").unwrap();
        assert!(cache.resolve(&source).unwrap_err().contains("checksum mismatch"));
        source.sha256 = Some("wrong".into());
        assert!(cache.resolve(&source).unwrap_err().contains("64 hexadecimal"));
    }

    #[test]
    fn explicit_packages_and_direct_wasm_use_content_cache() {
        let t = Temp::new();
        let cache = t.cache();
        let p = t.package();
        let source = Source { source: p.display().to_string(), ..Default::default() };
        let first = cache.resolve(&source).unwrap();
        assert_eq!(first.id, "t.demo");
        assert!(first.granted.ui);
        assert!(cache.root.join("artifacts").join(format!("{}.wasm", first.hash)).is_file());
        let configured = Source { config: serde_json::json!({"position":"right"}), ..source.clone() };
        let with_config = cache.resolve(&configured).unwrap();
        assert_eq!(with_config.hash, first.hash);
        assert_eq!(with_config.wasm, first.wasm);
        assert_eq!(with_config.source.config, configured.config);
        assert_ne!(with_config, first);
        // Changing local bytes produces a new artifact; no registry or install operation.
        std::fs::write(p.join("plugin.wasm"), b"\0asm\x0d\0\x01\0changed").unwrap();
        let second = cache.resolve(&source).unwrap();
        assert_ne!(first.hash, second.hash);
        let bare = t.0.join("bare.wasm");
        std::fs::write(&bare, &*first.wasm).unwrap();
        let direct = cache
            .resolve(&Source { source: bare.display().to_string(), id: Some("t.bare".into()), ..Default::default() })
            .unwrap();
        assert_eq!(direct.id, "t.bare");
        assert_eq!(direct.granted, Permissions::default());
        assert!(!cache.data.join("plugins.lock").exists());
        assert!(!cache.data.join("plugins").exists());
    }
    #[test]
    fn config_permissions_replace_manifest_and_validate_before_build() {
        let t = Temp::new();
        let p = t.package();
        let source =
            Source { source: p.display().to_string(), permissions: Some(Permissions::default()), ..Default::default() };
        assert!(!t.cache().resolve(&source).unwrap().granted.ui);
        let mismatch = Source { id: Some("another.id".into()), ..source.clone() };
        assert!(t.cache().resolve(&mismatch).unwrap_err().contains("does not match"));
        let file = t.0.join("file");
        std::fs::write(&file, b"x").unwrap();
        let perms = Permissions {
            fs: vec![crate::FsGrant { path: file.display().to_string(), write: false }],
            ..Default::default()
        };
        assert!(t.cache().resolve(&Source { permissions: Some(perms), ..source }).unwrap_err().contains("single-file"));
    }
    #[test]
    fn no_directory_discovery_or_lua_plugin_sources() {
        let t = Temp::new();
        let p = t.package();
        let unrelated = t.0.join("unrelated");
        std::fs::create_dir_all(&unrelated).unwrap();
        let cache = t.cache();
        assert!(cache.resolve(&Source { source: unrelated.display().to_string(), ..Default::default() }).is_err());
        let lua = t.0.join("init.lua");
        std::fs::write(&lua, "error('not a WASM plugin')").unwrap();
        assert!(cache.resolve(&Source { source: lua.display().to_string(), ..Default::default() }).is_err());
        assert!(!cache.root.exists());
        assert!(
            cache
                .resolve(&Source {
                    source: p.display().to_string(),
                    subdir: Some("../other".into()),
                    ..Default::default()
                })
                .is_err()
        );
    }
    #[test]
    fn git_sources_cache_exact_revision_and_support_subdirectory() {
        let t = Temp::new();
        let repo = t.0.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let p = t.package();
        std::fs::rename(p, repo.join("plugin")).unwrap();
        command(Command::new("git").arg("-C").arg(&repo).args(["init", "--quiet"]), "init").unwrap();
        command(Command::new("git").arg("-C").arg(&repo).args(["add", "."]), "add").unwrap();
        command(
            Command::new("git").arg("-C").arg(&repo).args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--quiet",
                "-m",
                "plugin",
            ]),
            "commit",
        )
        .unwrap();
        let output = Command::new("git").arg("-C").arg(&repo).args(["rev-parse", "HEAD"]).output().unwrap();
        let source = Source {
            source: format!("file://{}", repo.display()),
            rev: Some(String::from_utf8(output.stdout).unwrap().trim().into()),
            subdir: Some("plugin".into()),
            ..Default::default()
        };
        let cache = t.cache();
        let first = cache.resolve(&source).unwrap();
        // Cached snapshots work even when the original repository is unavailable.
        std::fs::remove_dir_all(repo).unwrap();
        assert_eq!(cache.resolve(&source).unwrap(), first);
    }
    #[test]
    fn raw_wasm_urls_fetch_once_and_work_offline_afterwards() {
        use std::io::{Read, Write};
        let t = Temp::new();
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/plugin.wasm", server.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let (mut conn, _) = server.accept().unwrap();
            let mut buf = [0; 4096];
            let _ = conn.read(&mut buf).unwrap();
            conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n\0asm\x0d\0\x01\0")
                .unwrap();
        });
        let source = Source { source: url, id: Some("remote.test".into()), ..Default::default() };
        let first = t.cache().resolve(&source).unwrap();
        worker.join().unwrap();
        assert_eq!(t.cache().resolve(&source).unwrap(), first);
    }
    #[test]
    fn rust_source_never_falls_back_to_a_stale_prebuilt_component() {
        let t = Temp::new();
        let dir = t.package();
        // The prebuilt component is otherwise usable, but a source declaration
        // must report a build failure instead of silently loading old code.
        std::fs::write(dir.join("Cargo.toml"), "not valid Cargo TOML = [").unwrap();
        let source = Source { source: dir.display().to_string(), ..Default::default() };
        assert!(t.cache().resolve(&source).unwrap_err().contains("plugin build"));
        assert!(!t.cache().root.join("artifacts").exists());
    }

    #[test]
    fn real_rust_source_build_reuses_cargo_artifact() {
        let sysroot = Command::new("rustc").args(["--print", "sysroot"]).output().unwrap();
        let sysroot = PathBuf::from(String::from_utf8(sysroot.stdout).unwrap().trim());
        if !sysroot.join("lib/rustlib/wasm32-wasip2/lib").is_dir() {
            assert!(
                std::env::var_os("PROTEUS_REQUIRE_WASM_TESTS").is_none(),
                "wasm32-wasip2 is required for this test job"
            );
            eprintln!("skipping real source compilation: no wasm32-wasip2 target");
            return;
        }
        let t = Temp::new();
        let cache = t.cache();
        let dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/pane-list").canonicalize().unwrap();
        let source = Source { source: dir.display().to_string(), ..Default::default() };
        let first = cache.resolve(&source).unwrap();
        assert_eq!(first.id, "example.pane-list");
        assert!(first.wasm.starts_with(b"\0asm\x0d\0\x01\0"));
        let artifact = cache
            .root
            .join("build")
            .join(sha256(dir.as_os_str().as_encoded_bytes()))
            .join("target/wasm32-wasip2/release/pane_list.wasm");
        let modified = std::fs::metadata(&artifact).unwrap().modified().unwrap();
        assert_eq!(cache.resolve(&source).unwrap(), first);
        assert_eq!(
            std::fs::metadata(artifact).unwrap().modified().unwrap(),
            modified,
            "unchanged source should reuse Cargo's cached WASM artifact"
        );
    }

    #[test]
    fn cargo_build_protocol_selects_own_artifact_and_reports_errors() {
        use std::os::unix::fs::PermissionsExt;
        let t = Temp::new();
        let dir = t.package();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname='demo'\nversion='0.1.0'\n").unwrap();
        let artifact = t.0.join("built.wasm");
        std::fs::write(&artifact, b"\0asm\x0d\0\x01\0").unwrap();
        let json=serde_json::json!({"reason":"compiler-artifact","manifest_path":dir.join("Cargo.toml"),"filenames":[artifact]}).to_string();
        let compiler = t.0.join("cargo-test");
        std::fs::write(&compiler, format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", json)).unwrap();
        std::fs::set_permissions(&compiler, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(t.cache().build_with_cargo(&dir, compiler.as_os_str()).unwrap(), artifact);
        std::fs::write(&compiler, "#!/bin/sh\necho 'compiler failed' >&2\nexit 1\n").unwrap();
        assert!(t.cache().build_with_cargo(&dir, compiler.as_os_str()).unwrap_err().contains("compiler failed"));
    }
}
