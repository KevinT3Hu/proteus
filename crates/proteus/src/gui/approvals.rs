//! Permissions a plugin requests in its manifest are only granted after the
//! user approves them (unless the config grants permissions explicitly).
//! Approvals are remembered per plugin ID, source and exact permission set,
//! so a plugin asking for more is asked about again.

use std::collections::HashSet;
use std::path::PathBuf;

use proteus_pty::persistence::StateFile;
use proteus_wasm::{Permissions, Resolved};
use serde::{Deserialize, Serialize};

const MAX_FILE: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Approval {
    id: String,
    source: String,
    rev: Option<String>,
    subdir: Option<PathBuf>,
    permissions: Permissions,
}

impl Approval {
    fn of(e: &Resolved) -> Self {
        Approval {
            id: e.id.clone(),
            source: e.source.source.clone(),
            rev: e.source.rev.clone(),
            subdir: e.source.subdir.clone(),
            permissions: e.granted.clone(),
        }
    }
}

#[derive(Default)]
pub(super) struct Approvals {
    file: Option<StateFile>,
    entries: Vec<Approval>,
    /// Plugins the user declined this session.
    pub denied: HashSet<String>,
}

impl Approvals {
    pub fn load() -> Result<Self, String> {
        let Some(dir) = super::workspace::state_dir() else { return Ok(Self::default()) };
        let file = StateFile::new(&dir.join("plugin-approvals.json")).map_err(|e| e.to_string())?;
        let entries = match file.read(MAX_FILE).map_err(|e| e.to_string())? {
            Some(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("plugin-approvals.json: {e}"))?,
            None => Vec::new(),
        };
        Ok(Self { file: Some(file), entries, denied: HashSet::new() })
    }

    /// The plugin requests permissions that neither the config nor the user granted.
    pub fn needs_approval(&self, e: &Resolved) -> bool {
        e.source.permissions.is_none()
            && e.granted != Permissions::default()
            && !self.entries.contains(&Approval::of(e))
    }

    /// Remember the approval; the plugin may run this session even if saving fails.
    pub fn approve(&mut self, e: &Resolved) -> Result<(), String> {
        let approval = Approval::of(e);
        self.entries.retain(|a| a.id != approval.id || a.source != approval.source);
        self.entries.push(approval);
        self.denied.remove(&e.id);
        let Some(file) = &self.file else { return Ok(()) };
        let bytes = serde_json::to_vec_pretty(&self.entries).map_err(|e| e.to_string())?;
        file.write(&bytes).map_err(|e| format!("cannot save plugin approval: {e}"))
    }
}

/// The requested permissions, one per line, in manifest syntax.
pub(super) fn describe(p: &Permissions) -> Vec<String> {
    let mut out = Vec::new();
    for (granted, name) in [
        (p.ui, "ui"),
        (p.read_panes, "read-panes"),
        (p.read_screen, "read-screen"),
        (p.write_pty, "write-pty"),
        (p.open_pane, "open-pane"),
        (p.clipboard_read, "clipboard-read"),
        (p.clipboard_write, "clipboard-write"),
        (p.secrets, "secrets"),
    ] {
        if granted {
            out.push(name.to_string());
        }
    }
    out.extend(p.fs.iter().map(|g| format!("fs: {} ({})", g.path, if g.write { "rw" } else { "r" })));
    out.extend(p.run_command.iter().map(|c| format!("run-command: {c}")));
    out.extend(p.network.iter().map(|h| format!("network: {h}")));
    out.into_iter().map(|l| l.chars().map(|c| if c.is_control() { '\u{fffd}' } else { c }).collect()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_every_permission_without_control_characters() {
        let p = Permissions {
            ui: true,
            write_pty: true,
            fs: vec![proteus_wasm::FsGrant { path: "/home/a\x1b[2J".into(), write: true }],
            run_command: vec!["git".into()],
            network: vec!["*.example.com".into()],
            ..Default::default()
        };
        assert_eq!(
            describe(&p),
            ["ui", "write-pty", "fs: /home/a\u{fffd}[2J (rw)", "run-command: git", "network: *.example.com"]
        );
        assert!(describe(&Permissions::default()).is_empty());
    }

    #[test]
    fn approval_covers_exactly_the_approved_request() {
        let source = proteus_wasm::Source {
            config: serde_json::Value::Null,
            source: "https://example.com/p.git".into(),
            rev: None,
            sha256: None,
            subdir: None,
            id: None,
            permissions: None,
        };
        let mut e = Resolved {
            id: "demo".into(),
            name: "Demo".into(),
            version: "1".into(),
            source,
            hash: "h".into(),
            granted: Permissions { ui: true, ..Default::default() },
            wasm: Default::default(),
        };
        let mut a = Approvals::default();
        assert!(a.needs_approval(&e));
        a.denied.insert("demo".into());
        a.approve(&e).unwrap();
        assert!(!a.needs_approval(&e) && a.denied.is_empty());
        e.hash = "updated".into();
        assert!(!a.needs_approval(&e), "same request after an update");
        e.granted.network = vec!["example.com".into()];
        assert!(a.needs_approval(&e), "more permissions ask again");
        e.source.permissions = Some(e.granted.clone());
        assert!(!a.needs_approval(&e), "granted by the config");
        e.source.permissions = None;
        e.granted = Permissions::default();
        assert!(!a.needs_approval(&e), "nothing requested");
    }
}
