//! Layouts and session restore, as methods of [`App`]: opening a
//! [`LayoutSpec`] as tabs, describing the window as one, and keeping the
//! session file up to date.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::PathBuf;

use std::time::Duration;

use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};

use super::{App, Prompt};

const RUN_POLL: Duration = Duration::from_millis(50);
/// Shells without a line editor never leave canonical mode: type anyway.
pub(super) const RUN_TIMEOUT: Duration = Duration::from_secs(3);
use crate::layout::{PaneId, Tab};
use crate::spec::{self, LayoutSpec, NodeSpec, PaneSpec, TabSpec};

/// Owner of the session file. Only one instance (the one holding the lock)
/// saves and restores; later ones run without a session.
pub struct SessionStore {
    path: PathBuf,
    _lock: File,
    /// Last text written, to skip identical saves.
    last: String,
}

/// `$XDG_STATE_HOME/proteus` (default `~/.local/state/proteus`).
fn state_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(base.join("proteus"))
}

impl SessionStore {
    pub fn acquire() -> Option<SessionStore> {
        let dir = state_dir()?;
        std::fs::create_dir_all(&dir).ok()?;
        let lock = File::options().create(true).truncate(false).write(true).open(dir.join("session.lock")).ok()?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).ok()?;
        Some(SessionStore { path: dir.join("session.lua"), _lock: lock, last: String::new() })
    }

    pub fn read(&self) -> Option<LayoutSpec> {
        match spec::load(&self.path) {
            Ok(s) => Some(s),
            Err(e) => {
                if self.path.exists() {
                    eprintln!("proteus: ignoring saved session: {e}");
                }
                None
            }
        }
    }

    fn write(&mut self, text: String) {
        if text == self.last {
            return;
        }
        let tmp = self.path.with_extension("lua.tmp");
        match std::fs::write(&tmp, &text).and_then(|_| std::fs::rename(&tmp, &self.path)) {
            Ok(()) => self.last = text,
            Err(e) => eprintln!("proteus: saving session: {e}"),
        }
    }

    pub fn remove(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        self.last.clear();
    }
}

/// `argv` as a shell command line.
pub fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            let safe = !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "_-./=:,+@%".contains(c));
            if safe { a.clone() } else { format!("'{}'", a.replace('\'', "'\\''")) }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl App {
    /// Start a pane for `p`. A command that cannot start falls back to the
    /// shell, with a notice.
    fn spawn_spec(&mut self, p: &PaneSpec, cols: u16, rows: u16) -> io::Result<PaneId> {
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
            pane.queue_run(run.to_string());
            self.watch_runs();
        }
        Ok(id)
    }

    /// Poll panes with a queued `run` line until all are sent. Output also
    /// triggers delivery (see `pty_output`); this covers a quiet shell.
    fn watch_runs(&mut self) {
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

    /// Open the tabs of `spec` after the existing ones and switch to the
    /// focused (or first) of them.
    pub(super) fn open_layout(&mut self, spec: &LayoutSpec) -> io::Result<()> {
        let m = self.layout_metrics();
        let area = self.area();
        let (cols, rows) = ((area.w / m.cell_w).max(2) as u16, (area.h / m.cell_h).max(1) as u16);
        let before: Vec<PaneId> = self.panes.keys().copied().collect();
        let first = self.tabs.len();
        let mut focus_tab = None;
        for t in &spec.tabs {
            let built = t.root.to_node(&mut |p| self.spawn_spec(p, cols, rows));
            let (root, focus) = match built {
                Ok(b) => b,
                Err(e) => {
                    // Drop panes started for tabs that never opened.
                    let placed: Vec<PaneId> = self.tabs.iter().flat_map(|t| t.panes()).collect();
                    self.panes.retain(|id, _| before.contains(id) || placed.contains(id));
                    return Err(e);
                }
            };
            let mut ids = Vec::new();
            root.leaves(&mut ids);
            let mut tab = Tab::with_root(root, focus.unwrap_or(ids[0]));
            tab.zoomed = t.zoomed && ids.len() > 1;
            tab.name = t.name.clone();
            self.tabs.push(tab);
            if t.focus {
                focus_tab = Some(self.tabs.len() - 1);
            }
        }
        let old = self.tabs.get(self.active).map(|t| t.focus);
        self.active = focus_tab.unwrap_or(first);
        self.relayout();
        if let Some(old) = old {
            self.after_focus(old);
        } else {
            self.update_title();
        }
        Ok(())
    }

    /// One pane running `command` (or the shell).
    pub(super) fn open_default(&mut self, command: Option<Vec<OsString>>) -> io::Result<()> {
        let id = self.spawn_pane(80, 24, None, command)?;
        self.tabs.push(Tab::new(id));
        self.active = self.tabs.len() - 1;
        self.relayout();
        self.update_title();
        Ok(())
    }

    fn pane_spec(&self, id: PaneId, focus: bool) -> PaneSpec {
        let Some(p) = self.panes.get(&id) else { return PaneSpec::default() };
        let run = p
            .foreground_command()
            .filter(|(name, _)| self.cfg.restore_commands.contains(name))
            .map(|(_, argv)| shell_join(&argv));
        PaneSpec { cmd: p.cmd.clone(), run, cwd: p.cwd(&self.hostname), name: p.name.clone(), focus }
    }

    /// The window as a layout: tabs, splits, ratios, cwd, names, and
    /// foreground commands from `session.restore_commands`.
    pub(super) fn current_spec(&self) -> LayoutSpec {
        let tabs = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let root = NodeSpec::from_node(&t.root, &mut |id| self.pane_spec(id, id == t.focus && t.panes().len() > 1));
                // One pane: its `cwd` doubles as the tab's.
                let cwd = match &root {
                    NodeSpec::Pane(p) => p.cwd.clone(),
                    _ => None,
                };
                TabSpec { name: t.name.clone(), cwd, focus: i == self.active, zoomed: t.zoomed, root }
            })
            .collect();
        LayoutSpec { tabs, clean: None }
    }

    pub(super) fn save_layout(&mut self, name: &str) {
        let Some(path) = spec::resolve(name) else { return self.notice("no config directory for layouts".into()) };
        let text = spec::to_lua(&self.current_spec());
        let res = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(&path, text));
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

    /// Write the session file if anything changed (runs on a timer).
    pub(super) fn autosave(&mut self) {
        // Do not overwrite a session the user may still restore.
        if self.exit || self.tabs.is_empty() || self.prompts.iter().any(|p| matches!(p, Prompt::Restore(_))) {
            return;
        }
        if self.session.is_none() {
            return;
        }
        let mut spec = self.current_spec();
        spec.clean = Some(false);
        let text = spec::to_lua(&spec);
        if let Some(s) = self.session.as_mut() {
            s.write(text);
        }
    }

    /// On exit: closing the window keeps the session (marked clean, so it
    /// is only restored with `restore = "always"`); the last pane ending
    /// ends the session.
    pub(super) fn finish_session(&mut self) {
        if self.prompts.iter().any(|p| matches!(p, Prompt::Restore(_))) {
            return;
        }
        if self.quit_requested && !self.tabs.is_empty() {
            let mut spec = self.current_spec();
            spec.clean = Some(true);
            let text = spec::to_lua(&spec);
            if let Some(s) = self.session.as_mut() {
                s.write(text);
            }
        } else if let Some(s) = self.session.as_mut() {
            s.remove();
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
    fn quoting() {
        let argv = ["nvim", "my file.txt", "it's", "-c", "set nu"].map(String::from);
        assert_eq!(shell_join(&argv), r#"nvim 'my file.txt' 'it'\''s' -c 'set nu'"#);
        assert_eq!(shell_join(&["".to_string()]), "''");
    }
}
