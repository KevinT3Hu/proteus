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
//! fs = ["~/src:rw", "/etc/hosts:r"]
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

    /// The directory on this machine.
    pub fn host_path(&self) -> PathBuf {
        match self.path.strip_prefix('~') {
            Some(rest) => {
                let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into());
                home.join(rest.trim_start_matches('/'))
            }
            None => PathBuf::from(&self.path),
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

/// Does `host` (lowercase, `port` explicit or the scheme's default) match a
/// `network` entry?
pub fn host_allowed(patterns: &[String], host: &str, port: u16) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    patterns.iter().any(|p| {
        let (name, want_port) = match p.rsplit_once(':') {
            Some((n, pp)) => (n, pp.parse::<u16>().ok()),
            None => (p.as_str(), None),
        };
        if want_port.is_some_and(|w| w != port) {
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
            v.push("open panes running programs".into());
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
                self.fs.retain(|f| f.path != path.trim_end_matches('/'));
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

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let raw: Raw = toml::from_str(text).map_err(|e| e.message().to_string())?;
        let p = raw.plugin;
        let id_ok = !p.id.is_empty()
            && p.id.len() <= 64
            && p.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c))
            && !p.id.starts_with('.')
            && !p.id.contains("..");
        if !id_ok {
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
        assert!(host_allowed(&p.network, "api.example.com", 443));
        assert!(host_allowed(&p.network, "a.b.x.org", 80));
        assert!(!host_allowed(&p.network, "x.org", 80));
        assert!(!host_allowed(&p.network, "evilx.org", 80));
        assert!(host_allowed(&p.network, "localhost", 11434));
        assert!(!host_allowed(&p.network, "localhost", 80));
        assert!(!host_allowed(&p.network, "example.com", 443));
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
