//! Programs a plugin runs (permission `run-command`): looked up in
//! [`TRUSTED_PATH`],
//! checked against the manifest's list, started without a shell in their
//! own process group (so stopping kills what they started too). Their
//! output and exit are awaited by tasks.

use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Notify, mpsc};
use tokio::task::AbortHandle;

use crate::runtime::Poster;
use crate::runtime::types::{Command, Event, Output, ProcessOutput};

/// Output kept by `run`, per stream.
pub const MAX_OUTPUT: usize = 8 << 20;
pub const MAX_PROCESSES: usize = 16;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Where bare program names are looked up, and the `PATH` children get.
/// The user's own `PATH` usually holds directories a plugin may be able to
/// write (`~/.local/bin`), which would let it substitute an allowed program.
pub const TRUSTED_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// The program to start for `name`, if the manifest lists it.
pub fn resolve(allowed: &[String], name: &str) -> Result<PathBuf, String> {
    if !allowed.iter().any(|a| a == name) {
        tracing::debug!(program = name, "permission denied: run-command");
        return Err(format!("permission denied: run-command:{name}"));
    }
    if name.starts_with('/') {
        return if executable(Path::new(name)) { Ok(name.into()) } else { Err(format!("{name}: not found")) };
    }
    std::env::split_paths(TRUSTED_PATH)
        .map(|d| d.join(name))
        .find(|p| executable(p))
        .ok_or_else(|| format!("{name}: not found in {TRUSTED_PATH}"))
}

fn build(program: &Path, cmd: &Command, cwd: &Path) -> Result<std::process::Command, String> {
    if let Some((k, _)) = cmd.env.iter().find(|(k, _)| !settable(k)) {
        return Err(format!("{}: plugins may not set the environment variable {k}", cmd.argv[0]));
    }
    let mut c = std::process::Command::new(program);
    c.arg0(&cmd.argv[0]).args(&cmd.argv[1..]).current_dir(cwd).process_group(0);
    c.env_clear();
    c.envs(std::env::vars_os().filter(|(k, _)| k.to_str().is_some_and(inherited)));
    c.env("PATH", TRUSTED_PATH);
    c.envs(cmd.env.iter().map(|(k, v)| (k, v)));
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    Ok(c)
}

/// Only the program name is logged: arguments may hold secrets.
fn start(program: &Path, cmd: &Command, cwd: &Path) -> Result<Child, String> {
    let name = &cmd.argv[0];
    let mut c = tokio::process::Command::from(build(program, cmd, cwd)?);
    c.kill_on_drop(true);
    let _runtime = proteus_tasks::enter()?;
    let child = c.spawn().map_err(|e| {
        tracing::warn!(program = %name, error = %e, "plugin process failed to start");
        format!("{name}: {e}")
    })?;
    tracing::debug!(program = %name, pid = child.id(), "plugin process started");
    Ok(child)
}

/// Variables a child gets from the terminal's environment; everything else
/// (tokens, agent sockets, loader settings) stays with the terminal.
fn inherited(name: &str) -> bool {
    matches!(name, "HOME" | "USER" | "LOGNAME" | "LANG" | "LANGUAGE" | "TERM" | "TZ" | "TMPDIR")
        || name.starts_with("LC_")
}

/// Variables a plugin may set: locale and output formatting, or its own
/// `PROTEUS_*` names. Others can make an allowed program run other code
/// (`LD_PRELOAD`, `GIT_SSH_COMMAND`, `PATH`, `BASH_ENV`, ...).
pub fn settable(name: &str) -> bool {
    let own = name
        .strip_prefix("PROTEUS_")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'));
    own || name.starts_with("LC_")
        || matches!(
            name,
            "LANG" | "LANGUAGE" | "TZ" | "NO_COLOR" | "CLICOLOR" | "CLICOLOR_FORCE" | "COLUMNS" | "LINES" | "TERM"
        )
}

/// Parent of every process, from `/proc/*/stat` (field 4, after the
/// parenthesised command name, which may itself contain spaces or `)`).
fn parents() -> Vec<(i32, i32)> {
    let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
    dir.filter_map(|e| {
        let pid: i32 = e.ok()?.file_name().to_str()?.parse().ok()?;
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let ppid = stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()?;
        Some((pid, ppid))
    })
    .collect()
}

/// Stop, then kill, every descendant of `root`, including ones that left its
/// process group with `setsid` or `setpgid`. Stopping first keeps them from
/// forking new children while the tree is walked.
fn kill_descendants(root: i32) {
    use rustix::process::{Pid, Signal, kill_process};
    let mut stopped = std::collections::HashSet::new();
    for _ in 0..8 {
        let table = parents();
        let mut tree = vec![root];
        let mut i = 0;
        while i < tree.len() {
            let p = tree[i];
            tree.extend(table.iter().filter(|&&(_, pp)| pp == p).map(|&(c, _)| c));
            i += 1;
        }
        let fresh: Vec<i32> = tree.into_iter().skip(1).filter(|p| stopped.insert(*p)).collect();
        if fresh.is_empty() {
            break;
        }
        for p in fresh.iter().filter_map(|&p| Pid::from_raw(p)) {
            let _ = kill_process(p, Signal::STOP);
        }
    }
    for p in stopped.into_iter().filter_map(Pid::from_raw) {
        let _ = kill_process(p, Signal::KILL);
    }
}

/// Kill `root`'s process group, then `child` itself. `root` is the child's
/// pid, which stays ours (and names its group) until the child is reaped.
fn kill_group(root: u32, child: &mut Child) {
    if let Some(pid) = rustix::process::Pid::from_raw(root as i32) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.start_kill();
}

/// Kill `child` with its process group and its other descendants. The
/// `/proc` walk blocks, so this is not for workers.
fn kill_tree(child: &mut Child) {
    if let Some(root) = child.id() {
        kill_descendants(root as i32);
        kill_group(root, child);
    }
}

/// [`kill_tree`] for tasks: the walk runs on a pooled thread. The child
/// cannot be reaped meanwhile, as nothing waits for it.
async fn terminate(child: &mut Child) {
    if let Some(root) = child.id() {
        if let Ok(walk) = proteus_tasks::spawn_blocking(move || kill_descendants(root as i32)) {
            let _ = walk.await;
        }
        kill_group(root, child);
    }
}

/// A child of [`run`], killed with what it started when dropped before it
/// was reaped (the call was cancelled).
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        kill_tree(&mut self.0);
    }
}

/// Read all of `r`, keeping the first `max` bytes in `out`.
async fn collect(r: Option<impl AsyncRead + Unpin>, max: usize, out: &mut Vec<u8>) {
    let Some(mut r) = r else { return };
    let mut buf = vec![0u8; 16384];
    while let Ok(n @ 1..) = r.read(&mut buf).await {
        let keep = n.min(max.saturating_sub(out.len()));
        out.extend_from_slice(&buf[..keep]);
    }
}

/// Run to the end (or `timeout`) and collect the output. Dropping the
/// future kills the process; drop it off the workers (see [`kill_tree`]).
pub async fn run(
    program: &Path,
    cmd: &Command,
    cwd: &Path,
    input: Vec<u8>,
    timeout: Duration,
) -> Result<Output, String> {
    let mut child = Running(start(program, cmd, cwd)?);
    let stdin = child.0.stdin.take();
    let (stdout, stderr) = (child.0.stdout.take(), child.0.stderr.take());
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let ended = tokio::time::timeout(timeout, async {
        let write = async move {
            if let Some(mut stdin) = stdin {
                // A program that does not read its input may exit first.
                let _ = stdin.write_all(&input).await;
            }
        };
        tokio::join!(write, collect(stdout, MAX_OUTPUT, &mut out), collect(stderr, MAX_OUTPUT, &mut err));
        child.0.wait().await
    })
    .await;
    let exit_code = match ended {
        Ok(Ok(status)) => status.code(),
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => {
            terminate(&mut child.0).await;
            let _ = child.0.wait().await;
            None
        }
    };
    Ok(Output { exit_code, stdout: out, stderr: err })
}

struct Live {
    stdin: Option<Stdin>,
    kill: Arc<Notify>,
    task: AbortHandle,
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(stdin) = &self.stdin {
            stdin.cancel();
        }
        self.kill.notify_one();
    }
}

/// Input queued for a process and not written yet, at most.
pub const MAX_PENDING_INPUT: usize = 16 << 20;
/// How long input queued before `close` may take to reach a process that
/// stopped reading; then it is dropped.
const DRAIN: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Feed {
    /// Queued bytes not written yet.
    pending: AtomicUsize,
    /// Why writing failed.
    error: std::sync::Mutex<Option<String>>,
    closed: Notify,
}

/// A spawned process's stdin, fed in order by a task. `write` never waits.
/// Dropping it without `close` drops the queued input.
struct Stdin {
    tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
    feed: Arc<Feed>,
    task: AbortHandle,
}

impl Stdin {
    fn new(pipe: ChildStdin) -> Result<Stdin, String> {
        let (tx, rx) = mpsc::unbounded_channel();
        let feed = Arc::<Feed>::default();
        let task = proteus_tasks::spawn(feed_input(pipe, rx, feed.clone()))?.abort_handle();
        Ok(Stdin { tx: Some(tx), feed, task })
    }

    /// Queue all of `data` or refuse it whole.
    fn write(&self, data: &[u8]) -> Result<(), String> {
        if let Some(e) = self.feed.error.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
            return Err(e.clone());
        }
        let tx = self.tx.as_ref().filter(|tx| !tx.is_closed()).ok_or("stdin is closed")?;
        if data.is_empty() {
            return Ok(());
        }
        let pending = self.feed.pending.load(Ordering::Relaxed);
        if data.len() > MAX_PENDING_INPUT.saturating_sub(pending) {
            tracing::warn!(len = data.len(), pending_bytes = pending, "stdin queue full; input refused");
            return Err("stdin queue is full (16 MiB)".into());
        }
        self.feed.pending.fetch_add(data.len(), Ordering::Relaxed);
        tx.send(data.to_vec()).map_err(|_| "stdin is closed".to_string())
    }

    /// End the input once what is queued has been written.
    fn close(&mut self) {
        if self.tx.take().is_some() {
            self.feed.closed.notify_one();
        }
    }

    fn cancel(&self) {
        self.task.abort();
    }
}

impl Drop for Stdin {
    fn drop(&mut self) {
        if self.tx.is_some() {
            self.cancel();
        }
    }
}

async fn feed_input(mut pipe: ChildStdin, mut rx: mpsc::UnboundedReceiver<Vec<u8>>, feed: Arc<Feed>) {
    let deliver = async {
        while let Some(data) = rx.recv().await {
            let mut rest = &data[..];
            while !rest.is_empty() {
                let n = pipe.write(rest).await?;
                if n == 0 {
                    return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
                }
                rest = &rest[n..];
                feed.pending.fetch_sub(n, Ordering::Relaxed);
            }
        }
        Ok(())
    };
    let deadline = async {
        feed.closed.notified().await;
        tokio::time::sleep(DRAIN).await;
    };
    tokio::select! {
        result = deliver => if let Err(e) = result {
            tracing::warn!(error = %e, pending_bytes = feed.pending.load(Ordering::Relaxed), "writing input to the process failed");
            *feed.error.lock().unwrap_or_else(PoisonError::into_inner) = Some(e.to_string());
        },
        () = deadline => tracing::warn!(
            pending_bytes = feed.pending.load(Ordering::Relaxed),
            timeout = ?DRAIN,
            "process stopped reading input; dropping queued input on close"
        ),
    }
}

/// Processes from `spawn`. Dropping it kills them.
#[derive(Default)]
pub struct Processes {
    next: u32,
    live: HashMap<u32, Live>,
}

/// Hand `r`'s output to the plugin until it ends or the plugin is gone.
async fn forward(id: u32, stderr: bool, r: Option<impl AsyncRead + Unpin>, post: &Poster) {
    let Some(mut r) = r else { return };
    let mut buf = vec![0u8; 65536];
    while let Ok(n @ 1..) = r.read(&mut buf).await {
        let e = Event::ProcessOutput(ProcessOutput { id, stderr, data: buf[..n].to_vec() });
        if !post.data_async(n, e).await {
            return;
        }
    }
}

/// Forward a spawned process's output, then report its exit (after all
/// output, unless it was killed).
async fn supervise(id: u32, mut child: Child, post: Poster, kill: Arc<Notify>) {
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let mut output = std::pin::pin!(async {
        tokio::join!(forward(id, false, stdout, &post), forward(id, true, stderr, &post));
    });
    let (mut drained, mut killed) = (false, false);
    let exit_code = loop {
        tokio::select! {
            () = &mut output, if !drained => drained = true,
            () = kill.notified(), if !killed => {
                terminate(&mut child).await;
                killed = true;
            }
            status = child.wait(), if drained || killed => break status.ok().and_then(|s| s.code()),
        }
    };
    tracing::debug!(process = id, exit_code, "plugin process exited");
    post.event_async(Event::ProcessExited((id, exit_code))).await;
}

impl Processes {
    fn reap(&mut self) {
        self.live.retain(|_, p| !p.task.is_finished());
    }

    pub fn spawn(&mut self, program: &Path, cmd: &Command, cwd: &Path, post: Poster) -> Result<u32, String> {
        self.reap();
        if self.live.len() >= MAX_PROCESSES {
            return Err(format!("at most {MAX_PROCESSES} processes at a time"));
        }
        let mut child = start(program, cmd, cwd)?;
        self.next = self.next.wrapping_add(1).max(1);
        let id = self.next;
        let stdin = child.stdin.take().ok_or_else(|| "stdin is not piped".to_string()).and_then(Stdin::new);
        let stdin = match stdin {
            Ok(w) => w,
            Err(e) => {
                kill_tree(&mut child);
                return Err(e);
            }
        };
        let kill = Arc::new(Notify::new());
        let task = proteus_tasks::spawn(supervise(id, child, post, kill.clone()))?.abort_handle();
        self.live.insert(id, Live { stdin: Some(stdin), kill, task });
        Ok(id)
    }

    pub fn write(&mut self, id: u32, data: &[u8]) -> Result<(), String> {
        self.reap();
        let p = self.live.get_mut(&id).ok_or_else(|| format!("no process {id}"))?;
        let s = p.stdin.as_mut().ok_or("stdin is closed")?;
        s.write(data)
    }

    pub fn close_stdin(&mut self, id: u32) {
        if let Some(p) = self.live.get_mut(&id)
            && let Some(stdin) = p.stdin.as_mut()
        {
            stdin.close();
        }
    }

    pub fn kill(&mut self, id: u32) {
        if let Some(p) = self.live.get(&id) {
            if let Some(stdin) = &p.stdin {
                stdin.cancel();
            }
            p.kill.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn run_now(program: &Path, cmd: &Command, input: Vec<u8>, timeout: Duration) -> Result<Output, String> {
        proteus_tasks::handle().unwrap().block_on(run(program, cmd, Path::new("/"), input, timeout))
    }

    fn sh(script: &str) -> Command {
        Command { argv: vec!["sh".into(), "-c".into(), script.into()], cwd: None, env: vec![] }
    }

    #[test]
    fn timeout_covers_descendants_holding_pipes() {
        let start = Instant::now();
        let out = run_now(Path::new("/bin/sh"), &sh("sleep 10 &"), vec![], Duration::from_millis(100)).unwrap();
        assert!(out.exit_code.is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn timeout_kills_descendants_that_leave_the_process_group() {
        let marker = std::env::temp_dir().join(format!("proteus-setsid-{}", std::process::id()));
        let script = format!("setsid sh -c 'sleep 0.5; touch {}' & wait", marker.display());
        let out = run_now(Path::new("/bin/sh"), &sh(&script), vec![], Duration::from_millis(100)).unwrap();
        assert!(out.exit_code.is_none());
        std::thread::sleep(Duration::from_secs(1));
        assert!(!marker.exists(), "the setsid child survived");
    }

    #[test]
    fn cancelling_run_kills_the_process_tree() {
        let marker = std::env::temp_dir().join(format!("proteus-cancel-{}", std::process::id()));
        let script = format!("setsid sh -c 'sleep 0.5; touch {}' & wait", marker.display());
        let tasks = proteus_tasks::handle().unwrap();
        let cmd = sh(&script);
        let running = run(Path::new("/bin/sh"), &cmd, Path::new("/"), vec![], DEFAULT_TIMEOUT);
        let r = tasks.block_on(async { tokio::time::timeout(Duration::from_millis(100), running).await });
        assert!(r.is_err());
        std::thread::sleep(Duration::from_secs(1));
        assert!(!marker.exists(), "the setsid child survived");
    }

    /// Events until the process exits.
    fn until_exit(events: &std::sync::mpsc::Receiver<crate::runtime::ToPlugin>) -> (Vec<u8>, Option<i32>) {
        let mut out = Vec::new();
        loop {
            match events.recv_timeout(Duration::from_secs(2)).unwrap() {
                crate::runtime::ToPlugin::Event(Event::ProcessOutput(o)) => out.extend(o.data),
                crate::runtime::ToPlugin::Event(Event::ProcessExited((_, code))) => return (out, code),
                _ => {}
            }
        }
    }

    #[test]
    fn spawned_output_comes_before_the_exit_and_kill_ends_it() {
        let (post, events) = Poster::testing();
        let mut processes = Processes::default();
        let id = processes.spawn(Path::new("/bin/sh"), &sh("cat; echo done; exit 4"), Path::new("/"), post).unwrap();
        processes.write(id, b"in ").unwrap();
        processes.close_stdin(id);
        assert_eq!(until_exit(&events), (b"in done\n".to_vec(), Some(4)));

        let (post, events) = Poster::testing();
        let id = processes.spawn(Path::new("/bin/sh"), &sh("sleep 10 & wait"), Path::new("/"), post).unwrap();
        let start = Instant::now();
        processes.kill(id);
        assert_eq!(until_exit(&events), (Vec::new(), None));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn stdin_is_bounded_and_reports_a_process_that_stopped_reading() {
        let (post, events) = Poster::testing();
        let mut processes = Processes::default();
        let id = processes.spawn(Path::new("/bin/sh"), &sh("sleep 10"), Path::new("/"), post).unwrap();
        processes.write(id, &vec![0; MAX_PENDING_INPUT]).unwrap();
        let e = processes.write(id, &vec![0; 1 << 20]).unwrap_err();
        assert!(e.contains("full"), "{e}");
        processes.kill(id);
        until_exit(&events);

        let (post, events) = Poster::testing();
        let id = processes.spawn(Path::new("/bin/sh"), &sh("exec 0<&-; sleep 10"), Path::new("/"), post).unwrap();
        let end = Instant::now() + Duration::from_secs(2);
        let mut result = processes.write(id, b"x");
        while result.is_ok() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
            result = processes.write(id, b"x");
        }
        let e = result.unwrap_err();
        assert!(e.to_ascii_lowercase().contains("pipe"), "the write error is reported: {e}");
        processes.kill(id);
        until_exit(&events);
    }

    #[test]
    fn spawned_stdin_backpressure_does_not_block_stop() {
        let (post, _events) = Poster::testing();
        let mut processes = Processes::default();
        let id = processes.spawn(Path::new("/bin/sh"), &sh("sleep 10"), Path::new("/"), post).unwrap();
        let start = Instant::now();
        processes.write(id, &vec![0; 1 << 20]).unwrap();
        processes.close_stdin(id);
        processes.kill(id);
        drop(processes);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn only_listed_programs() {
        let allowed = vec!["sh".to_string(), "/bin/false".into()];
        assert!(resolve(&allowed, "sh").unwrap().is_absolute());
        assert!(resolve(&allowed, "ls").unwrap_err().contains("permission denied"));
        assert!(resolve(&allowed, "/bin/sh").is_err(), "a path must be listed as such");
        let sh_path = resolve(&allowed, "sh").unwrap();
        let o = run_now(&sh_path, &sh("cat; echo err >&2; exit 3"), b"in".to_vec(), DEFAULT_TIMEOUT).unwrap();
        assert_eq!((o.exit_code, &o.stdout[..], &o.stderr[..]), (Some(3), &b"in"[..], &b"err\n"[..]));
        let t = Instant::now();
        let o = run_now(&sh_path, &sh("sleep 10"), vec![], Duration::from_millis(100)).unwrap();
        assert_eq!(o.exit_code, None);
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn children_get_a_small_environment_and_plugins_cannot_set_loader_variables() {
        let allowed = vec!["sh".to_string()];
        let sh_path = resolve(&allowed, "sh").unwrap();
        let env = |env: Vec<(String, String)>| Command { env, ..sh("env") };
        let o = run_now(&sh_path, &env(vec![("PROTEUS_MODE".into(), "x".into())]), vec![], DEFAULT_TIMEOUT).unwrap();
        let out = String::from_utf8(o.stdout).unwrap();
        assert!(out.lines().any(|l| l == "PROTEUS_MODE=x"));
        assert!(out.lines().any(|l| l == format!("PATH={TRUSTED_PATH}")));
        for line in out.lines() {
            let name = line.split('=').next().unwrap();
            assert!(inherited(name) || ["PATH", "PROTEUS_MODE", "PWD", "SHLVL", "_"].contains(&name), "{line}");
        }
        for bad in ["LD_PRELOAD", "GIT_SSH_COMMAND", "PATH", "BASH_ENV", "HOME", "proteus_x", "PROTEUS_"] {
            let e = run_now(&sh_path, &env(vec![(bad.into(), "x".into())]), vec![], DEFAULT_TIMEOUT);
            assert!(e.unwrap_err().contains("may not set"), "{bad}");
        }
    }
}
