//! `proteus-plugin.toml`: what a plugin is and the permissions it asks for.
//!
//! ```toml
//! [plugin]
//! id = "example.git-ui"
//! name = "Git UI"
//! version = "0.3.1"
//! api = "1"
//! description = "..."
//! wasm = "plugin.wasm"     # default
//!
//! [permissions]
//! ui = true
//! read-panes = true
//! fs = ["~/src:rw", "/etc:r"]
//! run-command = ["git"]
//! network = ["api.example.com"]
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The host API major version this terminal implements.
pub const API: &str = "1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    /// The component, relative to the manifest.
    pub wasm: PathBuf,
    pub permissions: Permissions,
}

/// A directory a plugin may use (`fs = ["PATH:r"]` or `"PATH:rw"`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FsGrant {
    /// Absolute (`~` expanded when used).
    pub path: String,
    pub write: bool,
}

impl FsGrant {
    pub fn parse(s: &str) -> Result<FsGrant, String> {
        let (path, mode) = s.rsplit_once(':').ok_or_else(|| format!("fs: `{s}`: expected PATH:r or PATH:rw"))?;
        let write = match mode {
            "r" => false,
            "rw" => true,
            _ => return Err(format!("fs: `{s}`: the mode must be r or rw")),
        };
        if !(path.starts_with('/') || path == "~" || path.starts_with("~/")) {
            return Err(format!("fs: `{path}`: the path must be absolute or start with ~/"));
        }
        if path.split('/').any(|c| c == "..") {
            return Err(format!("fs: `{path}`: `..` is not allowed"));
        }
        let path = match path.trim_end_matches('/') {
            "" => "/",
            p => p,
        };
        Ok(FsGrant { path: path.to_string(), write })
    }

    /// The directory on this machine. `~` needs an absolute `HOME`.
    pub fn host_path(&self) -> Result<PathBuf, String> {
        match self.path.strip_prefix('~') {
            Some(rest) => {
                let home = std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_absolute());
                let home = home.ok_or_else(|| format!("fs: `{}`: HOME is not set to an absolute path", self.path))?;
                Ok(home.join(rest.trim_start_matches('/')))
            }
            None => Ok(PathBuf::from(&self.path)),
        }
    }

    /// WASI preopens directories, not individual files. Missing directories
    /// may be mounted on a later start; existing files must not be ignored.
    pub fn validate_directory(&self) -> Result<(), String> {
        match std::fs::metadata(self.host_path()?) {
            Ok(m) if !m.is_dir() => {
                Err(format!("fs: `{}`: expected a directory; single-file grants are not supported", self.path))
            }
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("fs: `{}`: {e}", self.path)),
        }
    }

    fn label(&self) -> String {
        format!("{}:{}", self.path, if self.write { "rw" } else { "r" })
    }
}

/// What a plugin may do. Everything is off unless granted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct Permissions {
    pub ui: bool,
    pub read_panes: bool,
    pub read_screen: bool,
    pub write_pty: bool,
    pub open_pane: bool,
    #[serde(with = "fs_list")]
    pub fs: Vec<FsGrant>,
    /// Programs it may run: names looked up in `PATH`, or absolute paths.
    #[serde(deserialize_with = "programs")]
    pub run_command: Vec<String>,
    /// Hosts it may reach over HTTP(S): `api.example.com`, `*.example.com`
    /// (any subdomain), `localhost:11434` (only that port).
    #[serde(deserialize_with = "hosts")]
    pub network: Vec<String>,
    pub clipboard_read: bool,
    pub clipboard_write: bool,
    pub secrets: bool,
}

fn programs<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let v = Vec::<String>::deserialize(d)?;
    for p in &v {
        let ok = !p.is_empty() && !p.chars().any(|c| c.is_whitespace() || c.is_control()) && {
            !p.contains('/') || (p.starts_with('/') && !p.split('/').any(|c| c == ".."))
        };
        if !ok {
            return Err(serde::de::Error::custom(format!("run-command: `{p}`: a program name or an absolute path")));
        }
    }
    Ok(v)
}

fn hosts<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let v = Vec::<String>::deserialize(d)?;
    for h in &v {
        if !valid_host_pattern(h) {
            return Err(serde::de::Error::custom(format!(
                "network: `{h}`: a host name like `api.example.com`, `*.example.com` or `localhost:8080`"
            )));
        }
    }
    Ok(v.into_iter().map(|h| h.to_ascii_lowercase()).collect())
}

fn valid_host_pattern(h: &str) -> bool {
    let (name, port) = match h.rsplit_once(':') {
        Some((n, p)) => (n, Some(p)),
        None => (h, None),
    };
    if port.is_some_and(|p| p.parse::<u16>().is_err()) {
        return false;
    }
    let name = name.strip_prefix("*.").unwrap_or(name);
    !name.is_empty()
        && name.len() <= 253
        && name
            .split('.')
            .all(|l| !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

/// Does `host` (lowercase) on `port` match a `network` entry? An entry
/// without a port allows only the scheme's `default_port`.
pub fn host_allowed(patterns: &[String], host: &str, port: u16, default_port: u16) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    patterns.iter().any(|p| {
        let (name, want_port) = match p.rsplit_once(':') {
            Some((n, pp)) => (n, pp.parse::<u16>().ok()),
            None => (p.as_str(), None),
        };
        if want_port.unwrap_or(default_port) != port {
            return false;
        }
        match name.strip_prefix("*.") {
            Some(base) => {
                host.len() > base.len() && host.ends_with(base) && host[..host.len() - base.len()].ends_with('.')
            }
            None => host == name,
        }
    })
}

mod fs_list {
    use super::FsGrant;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[FsGrant], s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(v.iter().map(FsGrant::label))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<FsGrant>, D::Error> {
        let v = Vec::<String>::deserialize(d)?;
        v.iter().map(|s| FsGrant::parse(s).map_err(serde::de::Error::custom)).collect()
    }
}

impl Permissions {
    /// A directory inside a read-write one cannot be enforced as its own
    /// grant: the plugin reaches it through the outer grant, and can replace
    /// it with a symlink that the inner grant would follow on its next start.
    pub fn validate_fs_nesting(&self) -> Result<(), String> {
        for (i, inner) in self.fs.iter().enumerate() {
            let inner_path = inner.host_path()?;
            for outer in self.fs.iter().enumerate().filter(|(j, g)| g.write && *j != i).map(|(_, g)| g) {
                if inner_path.starts_with(outer.host_path()?) {
                    return Err(if inner.write {
                        format!(
                            "fs: `{}` is inside the read-write `{}`; nested grants are not supported",
                            inner.path, outer.path
                        )
                    } else {
                        format!(
                            "fs: `{}` is read-only but inside the read-write `{}`; the plugin could still write it",
                            inner.path, outer.path
                        )
                    });
                }
            }
        }
        Ok(())
    }

    /// Writable grants that reach the user's startup files or Proteus's own
    /// config, state and plugin data: through them a plugin can edit
    /// `.bashrc` or `init.lua`, or its own approval, and so run any code as
    /// the user. WASI cannot carve exceptions out of a grant, so this is
    /// stated in the approval question rather than silently half-enforced.
    pub fn writes_startup_files(&self) -> bool {
        let home = std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_absolute());
        let xdg = |var: &str, fallback: &str| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .or_else(|| home.as_ref().map(|h| h.join(fallback)))
        };
        let protected: Vec<PathBuf> = [
            xdg("XDG_CONFIG_HOME", ".config"),
            xdg("XDG_STATE_HOME", ".local/state"),
            xdg("XDG_DATA_HOME", ".local/share"),
            std::env::var_os("PROTEUS_DATA_DIR").map(PathBuf::from),
        ]
        .into_iter()
        .flatten()
        .collect();
        let written = self.fs.iter().filter(|g| g.write).filter_map(|g| g.host_path().ok());
        written.flat_map(|g| [g.canonicalize().ok(), Some(g)]).flatten().any(|g| {
            let dotfile = home.as_ref().is_some_and(|h| {
                h.starts_with(&g)
                    || g.strip_prefix(h)
                        .ok()
                        .and_then(|r| r.components().next())
                        .is_some_and(|c| c.as_os_str().as_encoded_bytes().starts_with(b"."))
            });
            dotfile || protected.iter().any(|p| p.starts_with(&g) || g.starts_with(p))
        })
    }

    /// One plain-language line per permission, for the approval question.
    pub fn describe(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.ui {
            v.push("show panels, status-bar text, questions and command-palette entries".to_string());
        }
        if self.read_panes {
            v.push("see your panes: titles, directories, running programs".into());
        }
        if self.read_screen {
            v.push("read the text in your panes, including scrollback and command output".into());
        }
        if self.write_pty {
            v.push("type into your panes (it can run commands as you)".into());
        }
        if self.open_pane {
            v.push("open panes running any program (it can run any code as you)".into());
        }
        for f in &self.fs {
            let how = if f.write { "read and change" } else { "read" };
            v.push(format!("{how} files in {}", f.path));
        }
        if !self.run_command.is_empty() {
            v.push(format!("run these programs as you: {}", self.run_command.join(", ")));
        }
        if !self.network.is_empty() {
            v.push(format!("connect to {}", self.network.join(", ")));
        }
        if self.clipboard_read {
            v.push("read the clipboard".into());
        }
        if self.clipboard_write {
            v.push("change the clipboard".into());
        }
        if self.secrets {
            v.push("keep its own passwords and keys in your keyring".into());
        }
        v
    }

    pub fn is_empty(&self) -> bool {
        *self == Permissions::default()
    }

    /// What `self` asks for beyond `granted`.
    pub fn beyond(&self, granted: &Permissions) -> Permissions {
        let more = |a: &[String], b: &[String]| a.iter().filter(|x| !b.contains(x)).cloned().collect();
        Permissions {
            ui: self.ui && !granted.ui,
            read_panes: self.read_panes && !granted.read_panes,
            read_screen: self.read_screen && !granted.read_screen,
            write_pty: self.write_pty && !granted.write_pty,
            open_pane: self.open_pane && !granted.open_pane,
            fs: self
                .fs
                .iter()
                .filter(|f| !granted.fs.iter().any(|g| g.path == f.path && (g.write || !f.write)))
                .cloned()
                .collect(),
            run_command: more(&self.run_command, &granted.run_command),
            network: more(&self.network, &granted.network),
            clipboard_read: self.clipboard_read && !granted.clipboard_read,
            clipboard_write: self.clipboard_write && !granted.clipboard_write,
            secrets: self.secrets && !granted.secrets,
        }
    }

    /// The on/off permissions by name.
    pub fn flag_names(&mut self) -> Vec<(bool, &'static str)> {
        self.flags().into_iter().map(|(on, n)| (*on, n)).collect()
    }

    /// Grant an on/off permission by name.
    pub fn set_flag(&mut self, name: &str) {
        for (flag, n) in self.flags() {
            if n == name {
                *flag = true;
            }
        }
    }

    fn flags(&mut self) -> [(&mut bool, &'static str); 8] {
        [
            (&mut self.ui, "ui"),
            (&mut self.read_panes, "read-panes"),
            (&mut self.read_screen, "read-screen"),
            (&mut self.write_pty, "write-pty"),
            (&mut self.open_pane, "open-pane"),
            (&mut self.clipboard_read, "clipboard-read"),
            (&mut self.clipboard_write, "clipboard-write"),
            (&mut self.secrets, "secrets"),
        ]
    }

    /// Names as in the manifest (`ui`, `read-panes`, `fs:~/x:rw`,
    /// `run-command:git`, `network:example.com`, ...).
    pub fn names(&self) -> Vec<String> {
        let mut me = self.clone();
        let mut v: Vec<String> = me.flags().into_iter().filter(|(on, _)| **on).map(|(_, n)| n.to_string()).collect();
        v.extend(self.fs.iter().map(|f| format!("fs:{}", f.label())));
        v.extend(self.run_command.iter().map(|p| format!("run-command:{p}")));
        v.extend(self.network.iter().map(|h| format!("network:{h}")));
        v
    }

    /// Take away one permission by name (`read-screen`, `fs:~/src:rw` or
    /// `fs:~/src`, `run-command:git` or all of `run-command`, the same for
    /// `network`). Returns false if it was not granted.
    pub fn revoke(&mut self, name: &str) -> bool {
        for (flag, n) in self.flags() {
            if n == name {
                return std::mem::replace(flag, false);
            }
        }
        let list = |l: &mut Vec<String>, item: Option<&str>| {
            let before = l.len();
            match item {
                Some(i) => l.retain(|x| x != i),
                None => l.clear(),
            }
            l.len() != before
        };
        match name.split_once(':').map_or((name, None), |(a, b)| (a, Some(b))) {
            ("run-command", item) => list(&mut self.run_command, item),
            ("network", item) => {
                list(&mut self.network, item.map(|h| h.trim()).map(str::to_ascii_lowercase).as_deref())
            }
            ("fs", Some(p)) => {
                let path = p.strip_suffix(":rw").or_else(|| p.strip_suffix(":r")).unwrap_or(p);
                let before = self.fs.len();
                let path = match path.trim_end_matches('/') {
                    "" => "/",
                    p => p,
                };
                self.fs.retain(|f| f.path != path);
                self.fs.len() != before
            }
            _ => false,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    plugin: RawPlugin,
    #[serde(default)]
    permissions: toml::Table,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlugin {
    id: String,
    name: String,
    version: String,
    api: String,
    #[serde(default)]
    description: String,
    wasm: Option<PathBuf>,
}

/// A local action ID is stable metadata, not a display label or a filesystem path.
pub fn valid_action_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 100 && id.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}

/// IDs are names, never filesystem paths, and never collide with another
/// plugin's files next to its data directory (`ID.storage.toml` and its
/// sidecars).
pub fn valid_plugin_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c))
        && !id.starts_with('.')
        && !id.contains("..")
        && !id.contains(".storage.toml")
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let raw: Raw = toml::from_str(text).map_err(|e| e.message().to_string())?;
        let p = raw.plugin;
        if !valid_plugin_id(&p.id) {
            return Err(format!("plugin.id `{}`: use lowercase letters, digits, `-`, `_` and `.`", p.id));
        }
        if p.api != API {
            return Err(format!("plugin.api `{}`: this terminal implements API {API}", p.api));
        }
        let permissions = Permissions::deserialize(raw.permissions).map_err(|e| format!("permissions: {e}"))?;
        let wasm = p.wasm.unwrap_or_else(|| "plugin.wasm".into());
        if wasm.is_absolute() || wasm.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            return Err("plugin.wasm: must be a path inside the plugin directory".into());
        }
        Ok(Manifest { id: p.id, name: p.name, version: p.version, description: p.description, wasm, permissions })
    }

    /// Read `DIR/proteus-plugin.toml`.
    pub fn load(dir: &Path) -> Result<Manifest, String> {
        let path = dir.join("proteus-plugin.toml");
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Manifest::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
        [plugin]
        id = "example.git-ui"
        name = "Git UI"
        version = "0.3.1"
        api = "1"

        [permissions]
        ui = true
        read-panes = true
        fs = ["~/src:rw", "/etc:r"]
    "#;

    #[test]
    fn parse() {
        let m = Manifest::parse(GOOD).unwrap();
        assert_eq!(m.id, "example.git-ui");
        assert_eq!(m.wasm, PathBuf::from("plugin.wasm"));
        let p = &m.permissions;
        assert!(p.ui && p.read_panes && !p.write_pty);
        assert_eq!(
            p.fs,
            [FsGrant { path: "~/src".into(), write: true }, FsGrant { path: "/etc".into(), write: false }]
        );
        assert_eq!(p.describe().len(), 4);
        assert_eq!(p.names(), ["ui", "read-panes", "fs:~/src:rw", "fs:/etc:r"]);
        for (bad, want) in [
            ("id = \"example.git-ui\"", "id = \"Bad Id\""),
            ("api = \"1\"", "api = \"2\""),
            ("ui = true", "ui = true\nnetwork = [\"bad host\"]"),
            ("ui = true", "ui = true\nrun-command = [\"../git\"]"),
            ("ui = true", "ui = true\nteleport = true"),
            ("\"/etc:r\"", "\"etc:r\""),
            ("\"/etc:r\"", "\"/a/../b:r\""),
            ("api = \"1\"", "api = \"1\"\nwasm = \"../x.wasm\""),
        ] {
            assert!(Manifest::parse(&GOOD.replace(bad, want)).is_err(), "{want}");
        }
        let more = "ui = true\nnetwork = [\"API.example.com\", \"*.x.org\", \"localhost:11434\"]\n\
                    run-command = [\"git\", \"/usr/bin/rg\"]\nclipboard-read = true\nsecrets = true";
        let p = Manifest::parse(&GOOD.replace("ui = true", more)).unwrap().permissions;
        assert_eq!(p.network, ["api.example.com", "*.x.org", "localhost:11434"]);
        assert_eq!(p.run_command, ["git", "/usr/bin/rg"]);
        assert!(p.clipboard_read && !p.clipboard_write && p.secrets);
        assert_eq!(p.describe().len(), 8);
        assert!(host_allowed(&p.network, "api.example.com", 443, 443));
        assert!(!host_allowed(&p.network, "api.example.com", 6379, 443), "only the default port");
        assert!(!host_allowed(&p.network, "api.example.com", 443, 80), "not https on the http port");
        assert!(host_allowed(&p.network, "a.b.x.org", 80, 80));
        assert!(!host_allowed(&p.network, "x.org", 80, 80));
        assert!(!host_allowed(&p.network, "evilx.org", 80, 80));
        assert!(host_allowed(&p.network, "localhost", 11434, 80));
        assert!(!host_allowed(&p.network, "localhost", 80, 80));
        assert!(!host_allowed(&p.network, "example.com", 443, 443));
    }

    #[test]
    fn read_only_grants_inside_writable_ones_are_rejected() {
        let p = |g: &[&str]| Permissions {
            fs: g.iter().map(|g| FsGrant::parse(g).unwrap()).collect(),
            ..Default::default()
        };
        assert!(p(&["/srv:rw", "/srv/secret:r"]).validate_fs_nesting().is_err());
        assert!(p(&["/srv:rw", "/srv/cache:rw"]).validate_fs_nesting().is_err());
        assert!(p(&["/srv:r", "/srv/notes:rw"]).validate_fs_nesting().is_ok());
        assert!(p(&["/srv:rw", "/srvx:r"]).validate_fs_nesting().is_ok());
    }

    #[test]
    fn ids_cannot_name_another_plugins_storage() {
        assert!(valid_plugin_id("a") && valid_plugin_id("a.storage"));
        for id in ["a.storage.toml", "a.storage.toml.lock", "a.storage.toml.corrupt-1-2"] {
            assert!(!valid_plugin_id(id), "{id}");
        }
    }

    #[test]
    fn writable_startup_files_are_recognised() {
        let p = |g: &[&str]| Permissions {
            fs: g.iter().map(|g| FsGrant::parse(g).unwrap()).collect(),
            ..Default::default()
        };
        for g in ["~:rw", "~/.config:rw", "~/.config/proteus:rw", "~/.local/share:rw", "~/.ssh:rw", "/:rw"] {
            assert!(p(&[g]).writes_startup_files(), "{g}");
        }
        for g in ["~:r", "~/notes:rw", "/srv:rw", "/:r"] {
            assert!(!p(&[g]).writes_startup_files(), "{g}");
        }
    }

    #[test]
    fn root_grant_can_be_revoked() {
        for name in ["fs:/", "fs:/:r", "fs:/:rw"] {
            let mut p = Permissions { fs: vec![FsGrant::parse("/:r").unwrap()], ..Default::default() };
            assert!(p.revoke(name));
            assert!(p.fs.is_empty());
        }
    }

    #[test]
    fn re_approval_and_revoke() {
        let new = Manifest::parse(GOOD).unwrap().permissions;
        let mut old = Permissions { ui: true, fs: vec![FsGrant::parse("~/src:r").unwrap()], ..Permissions::default() };
        let more = new.beyond(&old);
        assert!(more.read_panes && !more.ui);
        assert_eq!(more.names(), ["read-panes", "fs:~/src:rw", "fs:/etc:r"]);
        assert!(new.beyond(&new).is_empty());
        assert!(old.revoke("fs:~/src"));
        assert!(old.fs.is_empty() && old.revoke("ui") && !old.revoke("ui"));
        let mut p = Permissions { run_command: vec!["git".into(), "rg".into()], ..Permissions::default() };
        p.network = vec!["a.com".into()];
        assert_eq!(p.names(), ["run-command:git", "run-command:rg", "network:a.com"]);
        assert!(p.revoke("run-command:rg") && p.run_command == ["git"]);
        assert!(p.revoke("network") && p.network.is_empty() && !p.revoke("network:a.com"));
        let asks = Permissions { run_command: vec!["git".into(), "make".into()], ..Permissions::default() }.beyond(&p);
        assert_eq!(asks.run_command, ["make"]);
        let text = toml::to_string(&new).unwrap();
        assert_eq!(toml::from_str::<Permissions>(&text).unwrap(), new);
    }
}
