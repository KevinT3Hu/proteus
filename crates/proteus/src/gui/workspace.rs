//! Layouts and session restore, as methods of [`App`]: opening a
//! [`LayoutSpec`] as tabs, describing the window as one, and keeping the
//! session file up to date.

use proteus_pty::persistence::{Lock, StateFile};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use std::time::Duration;

use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};

use super::{App, Prompt, Win};

const RUN_POLL: Duration = Duration::from_millis(50);
/// Shells without a line editor never leave canonical mode: type anyway.
pub(super) const RUN_TIMEOUT: Duration = Duration::from_secs(3);
use crate::layout::{Float, PaneId, Tab};
use crate::spec::{self, FloatSpec, LayoutSpec, NodeSpec, PaneSpec, TabSpec};

/// Owner of the session file. Only one instance (the one holding the lock)
/// saves and restores; later ones run without a session.
pub struct SessionStore {
    path: PathBuf,
    _lock: Lock,
    file: StateFile,
    /// Only a fully synchronized save may populate this cache.
    last: Option<String>,
    /// Preserve unreadable/oversized snapshots if they cannot be backed up.
    blocked: bool,
}

/// `$XDG_STATE_HOME/proteus` (default `~/.local/state/proteus`).
pub(super) fn state_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(base.join("proteus"))
}

impl SessionStore {
    pub fn acquire() -> Option<SessionStore> {
        Self::acquire_at(state_dir()?)
            .map_err(|e| {
                if e.kind() != io::ErrorKind::WouldBlock {
                    eprintln!("proteus: session persistence disabled: {e}");
                }
                e
            })
            .ok()
    }

    fn acquire_at(dir: PathBuf) -> io::Result<SessionStore> {
        let path = dir.join("session.lua");
        let file = StateFile::new(&path)?;
        // Preserve the existing session.lock filename across upgrades.
        let lock = StateFile::new(&dir.join("session"))?.lock(true)?;
        Ok(SessionStore { path, file, _lock: lock, last: None, blocked: false })
    }

    pub fn read(&mut self) -> Option<LayoutSpec> {
        self.last = None;
        self.blocked = false;
        let bytes = match self.file.read(1 << 20) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return None,
            Err(e) => {
                self.blocked = true;
                eprintln!("proteus: cannot read saved session; preserving it without autosaving: {e}");
                return None;
            }
        };
        let parsed = std::str::from_utf8(&bytes)
            .map_err(|e| e.to_string())
            .and_then(|text| spec::parse(text, &self.path.display().to_string()));
        match parsed {
            Ok(spec) => Some(spec),
            Err(e) => {
                match self.file.backup(&bytes) {
                    Ok(name) => eprintln!(
                        "proteus: ignoring malformed session ({e}); recovery copy: {}",
                        self.path.with_file_name(name).display()
                    ),
                    Err(backup_error) => {
                        self.blocked = true;
                        eprintln!(
                            "proteus: malformed session ({e}); cannot back up ({backup_error}); autosave disabled to preserve original"
                        );
                    }
                }
                None
            }
        }
    }

    fn write(&mut self, text: String) -> io::Result<()> {
        self.write_with(text, |file, bytes| file.write(bytes))
    }

    fn write_with(
        &mut self,
        text: String,
        persist: impl FnOnce(&StateFile, &[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.blocked {
            return Err(io::Error::other("saved session needs manual recovery; original preserved"));
        }
        if text.len() > 1 << 20 {
            return Err(io::Error::other("session exceeds 1 MiB"));
        }
        if self.last.as_ref() == Some(&text) {
            return Ok(());
        }
        match persist(&self.file, text.as_bytes()) {
            Ok(()) => {
                self.last = Some(text);
                Ok(())
            }
            Err(e) => {
                // Rename may already have succeeded before directory sync
                // failed. A subsequent identical save must retry durability.
                self.last = None;
                Err(e)
            }
        }
    }

    pub fn remove(&mut self) -> io::Result<()> {
        if self.blocked {
            return Err(io::Error::other("saved session needs manual recovery; original preserved"));
        }
        self.last = None;
        self.file.remove()
    }
}

/// `argv` as a shell command line (see [`super::hint::shell_quote`]).
pub fn shell_join(argv: &[String]) -> String {
    argv.iter().map(|a| super::hint::shell_quote(a)).collect::<Vec<_>>().join(" ")
}

/// The lock lives in the private `lock_dir`, keyed by path, so that saving
/// into a project leaves nothing beside the layout.
fn write_layout(path: &Path, text: &str, lock_dir: &Path) -> io::Result<()> {
    let path = std::path::absolute(path)?;
    let key = proteus_pty::persistence::stable_hash(path.as_os_str().as_encoded_bytes());
    let file = StateFile::in_directory(&path)?;
    let _lock = StateFile::new(&lock_dir.join(format!("layout-{key:016x}")))?.lock(false)?;
    file.write(text.as_bytes())
}

impl App {
    /// Start a pane for `p`. A command that cannot start falls back to the
    /// shell, with a notice.
    fn spawn_spec(&mut self, p: &PaneSpec, cols: u16, rows: u16) -> io::Result<PaneId> {
        if let Some((plugin, args)) = &p.plugin {
            let id = self.plugin_pane(plugin, args, cols, rows);
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.name = p.name.clone().or(pane.name.take());
            }
            return Ok(id);
        }
        let command = p.cmd.as_ref().map(|c| c.argv().into_iter().map(OsString::from).collect());
        let id = match self.spawn_pane(cols, rows, p.cwd.clone(), command) {
            Ok(id) => id,
            Err(e) if p.cmd.is_some() => {
                self.notice(format!("cannot start {:?}: {e}", p.cmd.as_ref().map(|c| c.argv().join(" "))));
                self.spawn_pane(cols, rows, p.cwd.clone(), None)?
            }
            Err(e) => return Err(e),
        };
        let pane = self.panes.get_mut(&id).expect("just spawned");
        pane.name = p.name.clone();
        pane.cmd = p.cmd.clone();
        if let Some(run) = p.run.as_deref().filter(|r| !r.trim().is_empty()) {
            if run.chars().any(char::is_control) {
                self.notice(format!("not running {run:?}: it contains control characters"));
            } else {
                let pane = self.panes.get_mut(&id).expect("just spawned");
                pane.queue_run(run.to_string());
                self.watch_runs();
            }
        }
        Ok(id)
    }

    /// Poll panes with a queued `run` line until all are sent. Output also
    /// triggers delivery (see `pty_output`); this covers a quiet shell.
    pub(super) fn watch_runs(&mut self) {
        if self.run_timer {
            return;
        }
        self.run_timer = true;
        let _ = self.handle.insert_source(Timer::from_duration(RUN_POLL), |_, _, app: &mut App| {
            let mut waiting = false;
            for p in app.panes.values_mut() {
                waiting |= p.deliver_run(RUN_TIMEOUT);
            }
            app.run_timer = waiting;
            if waiting { TimeoutAction::ToDuration(RUN_POLL) } else { TimeoutAction::Drop }
        });
    }

    /// Open `spec`: its first window's tabs after the current window's
    /// tabs (switching to the focused, or first, of them), further windows
    /// as new windows.
    pub(super) fn open_layout(&mut self, spec: &LayoutSpec) -> io::Result<()> {
        let home = self.w.id;
        for (i, tabs) in spec.windows.iter().enumerate() {
            if i > 0 {
                self.new_window()?;
            }
            let res = self.open_tabs(tabs);
            if i > 0 && self.w.tabs.is_empty() {
                self.destroy_window();
            }
            res?;
        }
        self.activate(home);
        Ok(())
    }

    fn open_tabs(&mut self, tabs: &[TabSpec]) -> io::Result<()> {
        let m = self.layout_metrics();
        let area = self.area();
        let (cols, rows) = ((area.w / m.cell_w).max(2) as u16, (area.h / m.cell_h).max(1) as u16);
        let before: Vec<PaneId> = self.panes.keys().copied().collect();
        let first = self.w.tabs.len();
        let mut focus_tab = None;
        let cleanup = |app: &mut App| {
            // Drop panes started for tabs that never opened.
            let placed: Vec<PaneId> = app.w.tabs.iter().flat_map(|t| t.panes()).collect();
            app.panes.retain(|id, _| before.contains(id) || placed.contains(id));
        };
        for t in tabs {
            let (root, focus) = match t.root.to_node(&mut |p| self.spawn_spec(p, cols, rows)) {
                Ok(b) => b,
                Err(e) => {
                    cleanup(self);
                    return Err(e);
                }
            };
            let mut ids = Vec::new();
            root.leaves(&mut ids);
            let mut tab = Tab::with_root(root, focus.unwrap_or(ids[0]));
            let mut float_focus = None;
            for f in &t.floating {
                let id = match self.spawn_spec(&f.pane, cols / 2, rows / 2) {
                    Ok(id) => id,
                    Err(e) => {
                        cleanup(self);
                        return Err(e);
                    }
                };
                let geom = match f.geometry {
                    Some((col, row, w, h)) => Float { id, col, row, cols: w, rows: h },
                    None => tab.default_float(id, area, m),
                };
                tab.floats.push(geom);
                if f.pane.focus {
                    float_focus = Some(id);
                }
            }
            tab.floats_visible = t.show_floating && !tab.floats.is_empty();
            if let Some(id) = float_focus.filter(|_| tab.floats_visible) {
                tab.set_focus(id);
            }
            tab.zoomed = t.zoomed && ids.len() > 1;
            tab.name = t.name.clone();
            self.w.tabs.push(tab);
            if t.focus {
                focus_tab = Some(self.w.tabs.len() - 1);
            }
        }
        let old = self.w.tabs.get(self.w.active).map(|t| t.focus);
        self.w.active = focus_tab.unwrap_or(first);
        self.relayout();
        match old {
            Some(old) if first > 0 => self.after_focus(old),
            _ => self.update_title(),
        }
        Ok(())
    }

    /// One pane running `command` (or the shell).
    pub(super) fn open_default(&mut self, command: Option<Vec<OsString>>) -> io::Result<()> {
        let id = self.spawn_pane(80, 24, None, command)?;
        self.w.tabs.push(Tab::new(id));
        self.w.active = self.w.tabs.len() - 1;
        self.relayout();
        self.update_title();
        Ok(())
    }

    fn pane_spec(&self, id: PaneId, focus: bool) -> PaneSpec {
        let Some(p) = self.panes.get(&id) else { return PaneSpec::default() };
        let run = p
            .foreground_command()
            .filter(|(name, _)| self.cfg.restore_commands.contains(name))
            // Typed into a shell later: control characters could edit or end the line.
            .filter(|(_, argv)| !argv.iter().any(|a| a.chars().any(char::is_control)))
            .map(|(_, argv)| shell_join(&argv));
        if p.plugin.is_some() {
            return PaneSpec { plugin: p.plugin.clone(), name: p.name.clone(), focus, ..PaneSpec::default() };
        }
        PaneSpec { cmd: p.cmd.clone(), run, cwd: p.cwd(&self.hostname), name: p.name.clone(), focus, plugin: None }
    }

    fn tab_specs(&self, w: &Win) -> Vec<TabSpec> {
        w.tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let many = t.panes().len() > 1;
                let root = NodeSpec::from_node(&t.root, &mut |id| self.pane_spec(id, id == t.focus && many));
                // One pane: its `cwd` doubles as the tab's.
                let cwd = match &root {
                    NodeSpec::Pane(p) if t.floats.is_empty() => p.cwd.clone(),
                    _ => None,
                };
                let floating = t
                    .floats
                    .iter()
                    .map(|f| FloatSpec {
                        pane: self.pane_spec(f.id, f.id == t.focus),
                        geometry: Some((f.col, f.row, f.cols, f.rows)),
                    })
                    .collect();
                TabSpec {
                    name: t.name.clone(),
                    cwd,
                    focus: i == w.active,
                    zoomed: t.zoomed,
                    root,
                    floating,
                    show_floating: t.floats_visible,
                }
            })
            .collect()
    }

    /// The current window (or, with `all`, every window) as a layout:
    /// tabs, splits, stacks, floats, ratios, cwd, names, and foreground
    /// commands from `session.restore_commands`.
    pub(super) fn current_spec(&self, all: bool) -> LayoutSpec {
        if !all {
            return LayoutSpec::single(self.tab_specs(&self.w));
        }
        let mut wins: Vec<&Win> = std::iter::once(&self.w).chain(&self.parked).collect();
        wins.sort_by_key(|w| w.id);
        LayoutSpec {
            windows: wins.into_iter().map(|w| self.tab_specs(w)).filter(|t| !t.is_empty()).collect(),
            clean: None,
        }
    }

    pub(super) fn save_layout(&mut self, name: &str) {
        let Some(path) = spec::resolve(name) else { return self.notice("no config directory for layouts".into()) };
        let text = spec::to_lua(&self.current_spec(false));
        let res = write_layout(&path, &text, &proteus_pty::shell::runtime_dir());
        match res {
            Ok(()) => self.notice(format!("Layout saved to {}", path.display())),
            Err(e) => self.notice(format!("cannot save layout: {}: {e}", path.display())),
        }
    }

    pub(super) fn load_layout(&mut self, name: &str) {
        let Some(path) = spec::resolve(name) else { return };
        match spec::load(&path) {
            Ok(s) => {
                if let Err(e) = self.open_layout(&s) {
                    self.notice(format!("cannot open layout: {e}"));
                }
            }
            Err(e) => self.notice(e),
        }
    }

    /// Saved layout names, for the load prompt.
    pub(super) fn layout_names() -> Vec<String> {
        let Some(dir) = spec::layouts_dir() else { return Vec::new() };
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                (p.extension()? == "lua").then(|| p.file_stem()?.to_str().map(String::from))?
            })
            .collect();
        names.sort();
        names
    }

    // --- session -------------------------------------------------------------------

    /// The restore question is still open (the old session must not be
    /// overwritten yet).
    fn restore_pending(&self) -> bool {
        std::iter::once(&self.w).chain(&self.parked).any(|w| w.prompts.iter().any(|p| matches!(p, Prompt::Restore(_))))
    }

    /// Write the session file if anything changed (runs on a timer).
    pub(super) fn autosave(&mut self) {
        // Do not overwrite a session the user may still restore.
        if self.exit || self.restore_pending() {
            return;
        }
        if self.session.is_none() {
            return;
        }
        let mut spec = self.current_spec(true);
        spec.clean = Some(false);
        let text = spec::to_lua(&spec);
        if let Some(s) = self.session.as_mut()
            && let Err(e) = s.write(text)
        {
            eprintln!("proteus: saving session: {e}");
            self.toast(format!("Session could not be saved: {e}"), true, None);
        }
    }

    /// On exit: closing the window keeps the session (marked clean, so it
    /// is only restored with `restore = "always"`); the last pane ending
    /// ends the session.
    pub(super) fn finish_session(&mut self) {
        if self.restore_pending() {
            return;
        }
        if self.quit_requested {
            let mut spec = self.current_spec(true);
            spec.clean = Some(true);
            let text = spec::to_lua(&spec);
            if let Some(s) = self.session.as_mut()
                && let Err(e) = s.write(text)
            {
                eprintln!("proteus: saving final session: {e}");
            }
        } else if let Some(s) = self.session.as_mut()
            && let Err(e) = s.remove()
        {
            eprintln!("proteus: removing session: {e}");
        }
    }

    /// Answer to the restore question: replace the fresh pane(s).
    pub(super) fn restore(&mut self, spec: &LayoutSpec) {
        let old: Vec<PaneId> = self.panes.keys().copied().collect();
        match self.open_layout(spec) {
            Ok(()) => {
                for id in old {
                    self.close_pane(id);
                }
            }
            Err(e) => self.notice(format!("cannot restore session: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_lock_recover_corruption_and_retry_failed_saves() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("proteus-session-persistence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut session = SessionStore::acquire_at(dir.clone()).unwrap();
        assert!(SessionStore::acquire_at(dir.clone()).is_err());
        let first = "return {tabs={{name='original'}}}".to_string();
        session.write(first.clone()).unwrap();
        assert_eq!(std::fs::metadata(&session.path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(
            session
                .write_with("return {tabs={{name='new'}}}".into(), |file, bytes| {
                    file.write(bytes)?;
                    Err(io::Error::other("directory sync failed"))
                })
                .is_err()
        );
        assert!(session.last.is_none());
        // Returning to the previously saved layout must still write, since
        // the failed publication may already have replaced it on disk.
        session.write(first.clone()).unwrap();
        assert_eq!(std::fs::read_to_string(&session.path).unwrap(), first);
        let malformed = b"return { tabs = [";
        std::fs::write(&session.path, malformed).unwrap();
        assert!(session.read().is_none());
        assert!(!session.blocked);
        let backup = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .unwrap()
            .path();
        assert_eq!(std::fs::read(&backup).unwrap(), malformed);
        assert_eq!(std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777, 0o600);
        session.write(first.clone()).unwrap();
        assert!(session.read().is_some());
        drop(session);
        let mut restarted = SessionStore::acquire_at(dir.clone()).unwrap();
        assert!(restarted.read().is_some());
        restarted.remove().unwrap();
        assert!(!dir.join("session.lua").exists());
        drop(restarted);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oversized_session_is_preserved_until_manual_recovery() {
        let dir = std::env::temp_dir().join(format!("proteus-session-oversized-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut session = SessionStore::acquire_at(dir.clone()).unwrap();
        let bytes = vec![b'x'; (1 << 20) + 1];
        std::fs::write(&session.path, &bytes).unwrap();
        assert!(session.read().is_none());
        assert!(session.write("return {tabs={{}}}".into()).is_err());
        assert!(session.remove().is_err());
        assert_eq!(std::fs::read(&session.path).unwrap(), bytes);
        drop(session);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn explicit_layout_paths_get_no_sidecar_lock() {
        let root = std::env::temp_dir().join(format!("proteus-layout-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        write_layout(&root.join("dev.lua"), "return {tabs={{}}}", &root.join("locks")).unwrap();
        let names: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert_eq!(std::fs::read_to_string(root.join("dev.lua")).unwrap(), "return {tabs={{}}}");
        assert_eq!(std::fs::read_dir(root.join("locks")).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn quoting() {
        let argv = ["nvim", "my file.txt", "it's", "-c", "set nu"].map(String::from);
        assert_eq!(shell_join(&argv), r#"nvim 'my file.txt' 'it'"'"'s' -c 'set nu'"#);
        assert_eq!(shell_join(&["".to_string()]), "''");
    }
}
