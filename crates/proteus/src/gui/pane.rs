//! A pane: one terminal emulator attached to one PTY, with a reader thread
//! that forwards output to the event loop; or a plugin's pane surface,
//! whose terminal the plugin's frames are painted into.

use std::collections::{HashMap, HashSet};
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
/// Pty output sent to the GUI in one message once this much is collected.
/// With one more read on top, two batches fit the per-pane budget (64 KiB).
const READ_BATCH: usize = 24 << 10;
/// Longest wait for pty output or for room in the output budget.
const IDLE_POLL: Duration = Duration::from_millis(50);

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
    /// Foreground process group, the remote program found in the shell's
    /// process tree (if any), and when that was checked.
    fg_local: Option<(i32, Option<String>, Instant)>,
    /// The remote program the last whole-session scan found, if any.
    session_remote: Option<String>,
    /// When the last whole-session scan result arrived.
    session_checked: Option<Instant>,
    /// A whole-session scan was requested and has not answered yet.
    session_pending: bool,
    /// Locality last written to the log.
    logged_local: Option<bool>,
    /// `pane{id}` for log events about this pane (none for plugin panes).
    span: tracing::Span,
}

/// How long [`Pane::update_locality`] trusts its last look at `/proc`.
const LOCALITY_TTL: Duration = Duration::from_millis(250);

/// Programs (`comm`, at most 15 bytes) whose output comes from another
/// machine, or from wherever a multiplexer session runs.
const REMOTE_PROGRAMS: &[&str] = &[
    "ssh",
    "autossh",
    "sshpass",
    "mosh",
    "mosh-client",
    "et",
    "telnet",
    "rlogin",
    "nc",
    "ncat",
    "socat",
    "docker",
    "podman",
    "kubectl",
    "lxc",
    "incus",
    "machinectl",
    "toolbox",
    "distrobox",
    "vagrant",
    "minicom",
    "picocom",
    "tsh",
    "gcloud",
    "aws",
    "session-manager",
    "waypipe",
    "tmux",
    "screen",
    "zellij",
    "abduco",
    "dtach",
];

/// A process: pid, parent, session id, `comm`.
type Proc = (i32, i32, i32, String);

/// Every process in `/proc`.
fn processes() -> Vec<Proc> {
    let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
    dir.flatten()
        .filter_map(|e| {
            let pid: i32 = e.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
            parse_stat(pid, &stat)
        })
        .collect()
}

/// `pid (comm) state ppid pgrp session ...`
fn parse_stat(pid: i32, stat: &str) -> Option<Proc> {
    let (open, close) = (stat.find('(')?, stat.rfind(')')?);
    let comm = stat.get(open + 1..close)?.to_string();
    let mut rest = stat.get(close + 1..)?.split_whitespace().skip(1);
    let ppid = rest.next()?.parse().ok()?;
    let session = rest.nth(1)?.parse().ok()?;
    Some((pid, ppid, session, comm))
}

/// The [`REMOTE_PROGRAMS`] entry a process in session `shell` (the shell is
/// its leader) or descending from the shell runs, if any.
fn remote_in_session(procs: &[Proc], shell: i32) -> Option<&str> {
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for p in procs {
        children.entry(p.1).or_default().push(p.0);
    }
    let mut ours: HashSet<i32> = HashSet::new();
    let mut stack: Vec<i32> = procs.iter().filter(|p| p.2 == shell).map(|p| p.0).chain([shell]).collect();
    while let Some(pid) = stack.pop() {
        if ours.insert(pid) {
            stack.extend(children.get(&pid).into_iter().flatten());
        }
    }
    procs.iter().find(|p| ours.contains(&p.0) && is_remote(&p.3)).map(|p| p.3.as_str())
}

fn is_remote(comm: &str) -> bool {
    REMOTE_PROGRAMS.contains(&comm.split(':').next().unwrap_or(comm))
}

/// The [`REMOTE_PROGRAMS`] entry one of `roots` or their descendants runs,
/// if any. Costs a few reads per process of the tree, not of the machine.
fn remote_in_tree(
    roots: &[i32],
    children: impl Fn(i32) -> Vec<i32>,
    comm: impl Fn(i32) -> Option<String>,
) -> Option<String> {
    let mut seen: HashSet<i32> = HashSet::new();
    let mut stack = roots.to_vec();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(c) = comm(pid).filter(|c| is_remote(c)) {
            return Some(c);
        }
        stack.extend(children(pid));
    }
    None
}

/// Children of every thread of `pid` (`/proc/<pid>/task/*/children`).
fn proc_children(pid: i32) -> Vec<i32> {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else { return Vec::new() };
    let mut out = Vec::new();
    for task in tasks.flatten() {
        if let Ok(list) = std::fs::read_to_string(task.path().join("children")) {
            out.extend(list.split_whitespace().filter_map(|c| c.parse::<i32>().ok()));
        }
    }
    out
}

fn proc_comm(pid: i32) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(comm.trim_end_matches('\n').to_string())
}

/// Scans all of `/proc` off the GUI thread for [`Pane::update_locality`],
/// at most once per [`LOCALITY_TTL`] for all panes together. The scan
/// finds members of a shell's session that left its process tree
/// (orphans adopted by init or a subreaper).
pub(super) struct SessionScanner(std::sync::mpsc::Sender<(PaneId, i32)>);

impl SessionScanner {
    pub fn new(tx: Sender<Msg>) -> Self {
        let (req, rx) = std::sync::mpsc::channel::<(PaneId, i32)>();
        let worker = std::thread::Builder::new().name("session-scan".into()).spawn(move || {
            while let Ok(first) = rx.recv() {
                let started = Instant::now();
                let wanted: HashSet<(PaneId, i32)> = std::iter::once(first).chain(rx.try_iter()).collect();
                let procs = processes();
                for (id, shell) in wanted {
                    let remote = remote_in_session(&procs, shell).map(str::to_string);
                    if tx.send(Msg::SessionScanned(id, shell, remote)).is_err() {
                        return;
                    }
                }
                std::thread::sleep(LOCALITY_TTL.saturating_sub(started.elapsed()));
            }
        });
        if let Err(e) = worker {
            tracing::warn!("session scan thread failed to start, scanning on the GUI thread: {e}");
        }
        Self(req)
    }

    /// False if the worker is gone; the caller scans itself then.
    fn request(&self, id: PaneId, shell: i32) -> bool {
        self.0.send((id, shell)).is_ok()
    }
}

impl Pane {
    pub fn spawn(
        id: PaneId,
        opts: &SpawnOptions,
        core: proteus_core::Config,
        cell: (u32, u32),
        tx: Sender<Msg>,
    ) -> io::Result<Pane> {
        let span = tracing::info_span!("pane", id);
        let (pty, _shell) = span.in_scope(|| session::spawn_pty(opts))?;
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
        let pane_span = span.clone();
        std::thread::Builder::new().name(format!("pty-reader-{id}")).spawn(move || {
            let _span = span.entered();
            let mut buf = vec![0u8; 8 * 1024];
            // Output read but not sent yet: everything the pty has ready goes
            // out in one message, and while the GUI is behind, output keeps
            // collecting here instead of in many small messages.
            let mut batch: Vec<u8> = Vec::new();
            // Send `batch`, waiting for room until `deadline` (or for as long
            // as it takes). `Some(false)`: no room by the deadline.
            let send = |batch: &mut Vec<u8>, deadline: Option<Instant>| -> Option<bool> {
                let n = batch.len();
                let Ok(local) = pending.acquire_until(n, &stop, deadline) else { return Some(false) };
                let Ok(global) = total.acquire_until(n, &stop, deadline) else { return Some(false) };
                tracing::trace!(bytes = n, "pty output");
                let data = std::mem::take(batch);
                tx.send(Msg::Pty(id, data, (local?, global?))).ok().map(|()| true)
            };
            let poll = |reader: &std::fs::File, wait: Duration| -> rustix::io::Result<()> {
                let mut polls = [rustix::event::PollFd::new(reader, rustix::event::PollFlags::IN)];
                let wait = rustix::event::Timespec::try_from(wait).expect("small timeout");
                match rustix::event::poll(&mut polls, Some(&wait)) {
                    Err(rustix::io::Errno::INTR) => Ok(()),
                    r => r.map(drop),
                }
            };
            loop {
                if stop.load(Ordering::Relaxed) {
                    tracing::trace!("pty reader stopped");
                    return;
                }
                if batch.len() >= READ_BATCH {
                    match send(&mut batch, None) {
                        Some(true) => continue,
                        _ => return,
                    }
                }
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => batch.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // Nothing more is ready. While the GUI is behind, wait
                        // for room for the batch for a while, then go back to
                        // collecting whatever the pty has meanwhile.
                        if !batch.is_empty() {
                            match send(&mut batch, Some(Instant::now() + IDLE_POLL)) {
                                None => return,
                                Some(_) => continue,
                            }
                        }
                        // Readiness wakes immediately on output; the timeout
                        // only exists to notice a closed pane.
                        if let Err(e) = poll(&reader, IDLE_POLL) {
                            tracing::warn!("pty poll failed: {e}");
                            break;
                        }
                    }
                    Err(e) => {
                        // EIO is how the master reports that the child side closed.
                        if e.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) {
                            tracing::debug!("pty closed");
                        } else {
                            tracing::warn!("pty read failed: {e}");
                        }
                        break;
                    }
                }
            }
            if !batch.is_empty() && send(&mut batch, None).is_none() {
                return;
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
            fg_local: None,
            session_remote: None,
            session_checked: None,
            session_pending: false,
            logged_local: None,
            span: pane_span,
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
            fg_local: None,
            session_remote: None,
            session_checked: None,
            session_pending: false,
            logged_local: None,
            span: tracing::Span::none(),
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
            let pid = self.pty.as_ref().map(|p| p.child_pid());
            self.span.in_scope(|| tracing::warn!(pid, bytes = bytes.len(), "queue input to pty: {e}"));
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
            let _span = self.span.enter();
            tracing::debug!(pid = pty.child_pid(), cols, rows, "pty resized");
            if let Err(e) = pty.resize(ws) {
                tracing::warn!(pid = pty.child_pid(), cols, rows, "pty resize failed: {e}");
            }
        }
    }

    pub(super) fn ended_title(&mut self, title: String) {
        self.proc_title = title;
        self.reader_stop.store(true, Ordering::Relaxed);
        self.writer = None;
        self.pty = None;
    }

    /// Hang up and collect the exit status if the child is already gone,
    /// without waiting: 0 while it still runs (it is reaped once it exits).
    pub fn finish(&mut self) -> i32 {
        let Some(pty) = &mut self.pty else { return 0 };
        if let Some(code) = exited(pty) {
            return code;
        }
        pty.hangup();
        exited(pty).unwrap_or(0)
    }

    /// Exit status if the child has already exited (it is not signalled).
    pub fn exit_status(&mut self) -> Option<i32> {
        exited(self.pty.as_mut()?)
    }

    /// Hang up and wait up to `grace` for the exit status; then kill the
    /// session and wait up to `grace` again.
    pub fn reap(&mut self, grace: Duration) -> i32 {
        self.pty.as_mut().map_or(0, |pty| reap(pty, grace))
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

    /// Tell the terminal whether its output comes from this machine: SSH
    /// panes, and panes where any process of the shell's session or below
    /// it is a remote-access program or a multiplexer client, do not.
    ///
    /// The shell's process tree is walked here (cheap, and current when a
    /// new job takes the foreground); the rest of the session needs a scan
    /// of all of `/proc`, which `scanner` runs in the background.
    pub fn update_locality(&mut self, id: PaneId, scanner: &SessionScanner) {
        let Some((pgrp, shell)) = self
            .pty
            .as_ref()
            .filter(|_| self.remote.is_none())
            .and_then(|p| Some((p.foreground_pgrp()?, p.child_pid())))
        else {
            self.term.set_local(false);
            return;
        };
        let fresh = matches!(&self.fg_local, Some((cached, _, at)) if *cached == pgrp && at.elapsed() < LOCALITY_TTL);
        if !fresh {
            let remote = remote_in_tree(&[shell, pgrp], proc_children, proc_comm);
            self.fg_local = Some((pgrp, remote, Instant::now()));
        }
        if !self.session_pending && self.session_checked.is_none_or(|t| t.elapsed() >= LOCALITY_TTL) {
            if scanner.request(id, shell) {
                self.session_pending = true;
            } else {
                self.session_remote = remote_in_session(&processes(), shell).map(str::to_string);
                self.session_checked = Some(Instant::now());
            }
        }
        self.apply_locality(shell);
    }

    /// The result of a whole-session scan requested by [`Pane::update_locality`].
    pub fn session_scanned(&mut self, shell: i32, remote: Option<String>) {
        if self.pty.as_ref().map(|p| p.child_pid()) != Some(shell) {
            return;
        }
        self.session_pending = false;
        self.session_checked = Some(Instant::now());
        self.session_remote = remote;
        if self.remote.is_none() && self.fg_local.is_some() {
            self.apply_locality(shell);
        }
    }

    fn apply_locality(&mut self, shell: i32) {
        let tree = self.fg_local.as_ref().and_then(|(_, r, _)| r.as_deref());
        let remote = tree.or(self.session_remote.as_deref());
        let local = remote.is_none();
        if self.logged_local != Some(local) {
            self.logged_local = Some(local);
            self.span.in_scope(|| tracing::debug!(shell, local, remote_program = remote, "pane locality changed"));
        }
        self.term.set_local(local);
    }

    /// Re-read the foreground process on the next refresh.
    pub fn expire_proc_title(&mut self) {
        self.proc_checked = None;
        self.fg_local = None;
        self.session_checked = None;
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

/// Exit status of a child that has exited (128 + signal if killed).
fn exited(pty: &mut Pty) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    let st = pty.try_wait().ok()??;
    Some(st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(0)))
}

fn reap(pty: &mut Pty, grace: Duration) -> i32 {
    let wait = |pty: &mut Pty| {
        let until = Instant::now() + grace;
        loop {
            match exited(pty) {
                Some(code) => return Some(code),
                None if Instant::now() < until => std::thread::sleep(Duration::from_millis(5)),
                None => return None,
            }
        }
    };
    if let Some(code) = exited(pty) {
        return code;
    }
    pty.hangup();
    if let Some(code) = wait(pty) {
        return code;
    }
    use rustix::process::{Pid, Signal, kill_process_group};
    for pgrp in [Some(pty.child_pid()), pty.foreground_pgrp()].into_iter().flatten() {
        if let Some(pid) = Pid::from_raw(pgrp) {
            let _ = kill_process_group(pid, Signal::KILL);
        }
    }
    wait(pty).unwrap_or(128 + 9)
}

/// Last path component, or `~` for the home directory itself.
fn home_relative(p: &std::path::Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if home.as_deref() == Some(p) {
        return "~".into();
    }
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_lines() {
        let s = "42 (my prog) x) S 7 42 40 34816 42 4194560 0 0";
        assert_eq!(parse_stat(42, s), Some((42, 7, 40, "my prog) x".into())));
        assert_eq!(parse_stat(1, "1 (init"), None);
    }

    #[test]
    fn remote_programs_anywhere_in_the_session_count() {
        let p = |pid, ppid, sid, comm: &str| (pid, ppid, sid, comm.to_string());
        let base = vec![p(1, 0, 1, "systemd"), p(10, 1, 10, "bash"), p(11, 10, 10, "myssh.sh"), p(20, 1, 20, "ssh")];
        assert_eq!(remote_in_session(&base, 10), None);
        let mut wrapped = base.clone();
        wrapped.push(p(12, 11, 10, "ssh"));
        assert_eq!(remote_in_session(&wrapped, 10), Some("ssh"));
        let mut sudo = base.clone();
        sudo.extend([p(12, 10, 10, "sudo"), p(13, 12, 13, "ssh")]);
        assert_eq!(remote_in_session(&sudo, 10), Some("ssh"));
        let mut tmux = base;
        tmux.push(p(12, 10, 10, "tmux: client"));
        assert_eq!(remote_in_session(&tmux, 10), Some("tmux: client"));
    }

    #[test]
    fn reaping_a_child_that_ignores_hangup_is_bounded() {
        let spec = proteus_pty::CommandSpec::new("/bin/sh").args(["-c", "trap '' HUP; while :; do sleep 1; done"]);
        let mut pty = Pty::spawn(&spec, WindowSize::new(80, 24)).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let start = Instant::now();
        assert_eq!(reap(&mut pty, Duration::from_millis(200)), 128 + 9);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
