//! A pane: one terminal emulator attached to one PTY, with a reader thread
//! that forwards output to the event loop; or a plugin's pane surface,
//! whose terminal the plugin's frames are painted into.

use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use proteus_core::Terminal;
use proteus_pty::{InputWriter, Pty, WindowSize};
use smithay_client_toolkit::reexports::calloop::channel::Sender;

use super::Msg;
use crate::layout::PaneId;
use crate::session::{self, SpawnOptions};
use crate::spec::Cmd;

/// How often the fallback title (foreground process) is re-read.
const PROC_REFRESH: Duration = Duration::from_secs(1);

pub struct Pane {
    pub term: Terminal,
    /// Set by the user; overrides the title.
    pub name: Option<String>,
    /// What the pane runs instead of the shell (from a layout), for saving.
    pub cmd: Option<Cmd>,
    pub(super) remote: Option<super::ssh::Remote>,
    /// A layout's `run` command, typed once the shell waits for input.
    pending_run: Option<(String, Instant)>,
    /// None for a plugin's pane.
    pub pty: Option<Pty>,
    writer: Option<InputWriter>,
    reader_stop: Arc<AtomicBool>,
    /// Size last sent to the PTY.
    size: WindowSize,
    /// Foreground process name and cwd, used when the app sets no title.
    proc_title: String,
    proc_checked: Option<Instant>,
    /// The bell rang while the pane was not looked at (marker on its tab).
    pub bell: bool,
    /// Visual bell: tinted until then.
    pub flash_until: Option<Instant>,
    /// Hidden from plugins that read screens.
    pub private: bool,
    /// A plugin's pane: its surface.
    pub surface: Option<u32>,
    /// A plugin's pane from a layout (or to save in one): plugin id, args.
    pub plugin: Option<(String, String)>,
    /// Size (cols, rows, cell) to apply once its hidden tab is shown.
    deferred_size: Option<(u16, u16, (u32, u32))>,
}

impl Pane {
    pub fn spawn(
        id: PaneId,
        opts: &SpawnOptions,
        core: proteus_core::Config,
        cell: (u32, u32),
        tx: Sender<Msg>,
    ) -> io::Result<Pane> {
        let (pty, _shell) = session::spawn_pty(opts)?;
        let mut term = Terminal::new(opts.cols as usize, opts.rows as usize, core);
        term.set_cell_pixel_size(cell.0, cell.1);
        let writer = InputWriter::new(pty.try_clone_master()?)?;
        let mut reader = pty.try_clone_master()?;
        let reader_stop = Arc::new(AtomicBool::new(false));
        let stop = reader_stop.clone();
        use proteus_pty::budget::Budget;
        static OUTPUT: std::sync::OnceLock<Budget> = std::sync::OnceLock::new();
        let total = OUTPUT.get_or_init(|| Budget::new(512 << 10, 64)).clone();
        let pending = Budget::new(64 << 10, 8);
        std::thread::Builder::new().name(format!("pty-reader-{id}")).spawn(move || {
            let mut buf = vec![0u8; 8 * 1024];
            loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let Some(local) = pending.acquire(n, &stop) else { return };
                        let Some(global) = total.acquire(n, &stop) else { return };
                        if tx.send(Msg::Pty(id, buf[..n].to_vec(), (local, global))).is_err() {
                            return;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // Readiness wakes immediately on output; the bounded
                        // timeout only exists to notice a closed pane.
                        let mut polls = [rustix::event::PollFd::new(&reader, rustix::event::PollFlags::IN)];
                        let wait = rustix::event::Timespec::try_from(Duration::from_millis(50)).expect("small timeout");
                        if let Err(e) = rustix::event::poll(&mut polls, Some(&wait))
                            && e != rustix::io::Errno::INTR
                        {
                            break;
                        }
                        continue;
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(Msg::PtyClosed(id));
        })?;
        let size = session::window_size(opts.cols, opts.rows, opts.cell);
        let mut pane = Pane {
            term,
            name: None,
            cmd: None,
            remote: None,
            pending_run: None,
            pty: Some(pty),
            writer: Some(writer),
            reader_stop,
            size,
            proc_title: String::new(),
            proc_checked: None,
            bell: false,
            flash_until: None,
            private: false,
            surface: None,
            plugin: None,
            deferred_size: None,
        };
        pane.refresh_proc_title();
        Ok(pane)
    }

    /// A pane showing plugin surface `surface` in `term`.
    pub fn plugin(term: Terminal, surface: u32, title: String) -> Pane {
        Pane {
            term,
            name: Some(title),
            cmd: None,
            remote: None,
            pending_run: None,
            pty: None,
            writer: None,
            reader_stop: Arc::default(),
            size: WindowSize::default(),
            proc_title: String::new(),
            proc_checked: None,
            bell: false,
            flash_until: None,
            private: false,
            surface: Some(surface),
            plugin: None,
            deferred_size: None,
        }
    }

    /// Type `line` into the shell as soon as its line editor reads input
    /// (see [`Pane::deliver_run`]).
    pub fn queue_run(&mut self, line: String) {
        self.pending_run = Some((line, Instant::now()));
    }

    /// Send the queued `run` line once the shell reads keys itself, so it
    /// shows once, after the prompt; or after `timeout` for shells that
    /// never leave canonical mode. Returns true while one is still queued.
    pub fn deliver_run(&mut self, timeout: Duration) -> bool {
        let Some((_, since)) = &self.pending_run else { return false };
        if self.pty.as_ref().is_some_and(|p| !p.raw_input()) && since.elapsed() < timeout {
            return true;
        }
        if let Some((line, _)) = self.pending_run.take() {
            self.write(format!("{line}\r").as_bytes());
        }
        false
    }

    pub fn try_write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.remote.as_ref().is_some_and(|r| r.closed) {
            return Ok(());
        }
        self.writer.as_ref().map_or(Ok(()), |w| w.write(bytes))
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if let Err(e) = self.try_write(bytes) {
            eprintln!("proteus: queue input to pty: {e}");
        }
    }

    /// Fit the grid to `cols` × `rows`. The PTY hears about real changes
    /// only, and only with `notify` (off while a divider is dragged).
    /// A pane's place changed while its tab is hidden: reflowing it waits
    /// until it is shown (see [`Pane::apply_deferred_resize`]).
    pub fn defer_resize(&mut self, cols: u16, rows: u16, cell: (u32, u32)) {
        let same = cols as usize == self.term.cols() && rows as usize == self.term.lines();
        self.deferred_size = (!same).then_some((cols, rows, cell));
        if same {
            self.term.set_cell_pixel_size(cell.0, cell.1);
        }
    }

    pub fn has_deferred_resize(&self) -> bool {
        self.deferred_size.is_some()
    }

    pub fn apply_deferred_resize(&mut self, notify: bool) {
        if let Some((cols, rows, cell)) = self.deferred_size {
            self.resize(cols, rows, cell, notify);
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell: (u32, u32), notify: bool) {
        self.deferred_size = None;
        self.term.set_cell_pixel_size(cell.0, cell.1);
        if cols as usize != self.term.cols() || rows as usize != self.term.lines() {
            self.term.resize(cols as usize, rows as usize);
        }
        let ws = session::window_size(cols, rows, Some((cell.0 as u16, cell.1 as u16)));
        if notify
            && ws != self.size
            && let Some(pty) = &self.pty
        {
            self.size = ws;
            let _ = pty.resize(ws);
        }
    }

    pub(super) fn ended_title(&mut self, title: String) {
        self.proc_title = title;
        self.reader_stop.store(true, Ordering::Relaxed);
        self.writer = None;
        self.pty = None;
    }

    /// Hang up and collect the exit status, waiting briefly.
    pub fn finish(&mut self) -> i32 {
        let Some(pty) = &mut self.pty else { return 0 };
        use std::os::unix::process::ExitStatusExt;
        if let Ok(Some(st)) = pty.try_wait() {
            return st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(0));
        }
        pty.hangup();
        for _ in 0..50 {
            if let Ok(Some(st)) = pty.try_wait() {
                return st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(0));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        0
    }

    /// User-given name, else OSC 0/2 title, else foreground process name
    /// and directory.
    pub fn title(&self) -> &str {
        if let Some(n) = &self.name {
            return n;
        }
        if self.remote.is_some() || self.term.title().is_empty() { &self.proc_title } else { self.term.title() }
    }

    /// Foreground job (not the shell): program name and argv.
    pub fn foreground_command(&self) -> Option<(String, Vec<String>)> {
        let pty = self.pty.as_ref()?;
        if !pty.has_foreground_job() {
            return None;
        }
        let pid = pty.foreground_pgrp()?;
        let proc = std::path::Path::new("/proc").join(pid.to_string());
        let name = std::fs::read_to_string(proc.join("comm")).ok()?.trim_end().to_string();
        let raw = std::fs::read(proc.join("cmdline")).ok()?;
        let argv: Vec<String> =
            raw.split(|&b| b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
        (!argv.is_empty()).then_some((name, argv))
    }

    /// Re-read the foreground process on the next refresh.
    pub fn expire_proc_title(&mut self) {
        self.proc_checked = None;
    }

    /// Re-read the foreground process at most once a second. Returns true
    /// if the fallback title changed.
    pub fn refresh_proc_title(&mut self) -> bool {
        if self.proc_checked.is_some_and(|t| t.elapsed() < PROC_REFRESH) {
            return false;
        }
        self.proc_checked = Some(Instant::now());
        if let Some(remote) = &self.remote {
            if remote.closed {
                return false;
            }
            let location = self
                .term
                .cwd()
                .map(|c| format!(" · {}:{}", c.host.as_deref().unwrap_or("remote"), c.path))
                .unwrap_or_default();
            let title = format!("{} · running{location}", remote.connection.label());
            let changed = self.proc_title != title;
            self.proc_title = title;
            return changed;
        }
        let Some(fg) = self.pty.as_ref().and_then(|p| p.foreground_process()) else { return false };
        let dir = fg.cwd.as_deref().map(home_relative).unwrap_or_default();
        let title = if dir.is_empty() { fg.name } else { format!("{} {dir}", fg.name) };
        if title != self.proc_title {
            self.proc_title = title;
            return true;
        }
        false
    }

    /// Name of the foreground program if it is not the shell and not in
    /// `ignore` (closing would kill it).
    pub fn busy(&self, ignore: &[String]) -> Option<String> {
        let pty = self.pty.as_ref()?;
        if !pty.has_foreground_job() {
            return None;
        }
        let name = pty.foreground_process().map(|p| p.name).unwrap_or_else(|| "a program".into());
        (!ignore.contains(&name)).then_some(name)
    }

    /// Working directory for a new pane opened from this one: OSC 7 if it
    /// is local, else the foreground process's cwd.
    pub fn cwd(&self, hostname: &str) -> Option<PathBuf> {
        if self.remote.is_some() {
            return None;
        }
        if let Some(c) = self.term.cwd().filter(|c| c.is_local(hostname)) {
            let p = PathBuf::from(&c.path);
            if p.is_dir() {
                return Some(p);
            }
        }
        self.pty.as_ref()?.foreground_process().and_then(|f| f.cwd)
    }

    /// The foreground program's name.
    pub fn process(&self) -> Option<String> {
        self.pty.as_ref()?.foreground_process().map(|f| f.name)
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        self.reader_stop.store(true, Ordering::Relaxed);
    }
}

/// Last path component, or `~` for the home directory itself.
fn home_relative(p: &std::path::Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if home.as_deref() == Some(p) {
        return "~".into();
    }
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}
