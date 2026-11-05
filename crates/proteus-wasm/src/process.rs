//! Programs a plugin runs (permission `run-command`): looked up in
//! [`TRUSTED_PATH`],
//! checked against the manifest's list, started without a shell in their
//! own process group (so stopping kills what they started too).

use proteus_pty::InputWriter;
use std::collections::HashMap;
use std::io::Read;
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

fn kill_group(child: &mut Child) {
    use rustix::process::{WaitId, WaitIdOptions};
    if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
        let alive = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG;
        if rustix::process::waitid(WaitId::Pid(pid), alive).is_ok() {
            kill_descendants(pid.as_raw_nonzero().get());
        }
        // The group id is the leader's pid, which stays ours only until the
        // leader is reaped; after that it may belong to another process.
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG;
        if rustix::process::waitid(WaitId::Pid(pid), options).is_ok() {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }
    let _ = child.kill();
}

fn wait_readable(fd: impl AsFd) {
    let mut polls = [rustix::event::PollFd::new(&fd, rustix::event::PollFlags::IN)];
    let wait = rustix::event::Timespec::try_from(Duration::from_millis(50)).expect("small timeout");
    let _ = rustix::event::poll(&mut polls, Some(&wait));
}

/// Read all of `r`, keeping the first `max` bytes.
fn drain(
    mut r: impl Read + AsFd + Send + 'static,
    max: usize,
    stop: Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<Vec<u8>>> {
    rustix::io::ioctl_fionbio(r.as_fd(), true)?;
    Ok(std::thread::spawn(move || {
        let (mut out, mut buf) = (Vec::new(), [0u8; 16384]);
        while !stop.load(Ordering::Relaxed) {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let keep = n.min(max.saturating_sub(out.len()));
                    out.extend_from_slice(&buf[..keep]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => wait_readable(r.as_fd()),
                Err(_) => break,
            }
        }
        out
    }))
}

/// Run to the end (or `timeout`, or `stop`) and collect the output.
pub fn run(
    program: &Path,
    cmd: &Command,
    cwd: &Path,
    input: Vec<u8>,
    timeout: Duration,
    stop: &AtomicBool,
) -> Result<Output, String> {
    let name = &cmd.argv[0];
    let mut child = build(program, cmd, cwd)?.spawn().map_err(|e| format!("{name}: {e}"))?;
    let io_stop = Arc::new(AtomicBool::new(false));
    let setup = (|| {
        let mut stdin = InputWriter::new(child.stdin.take().expect("piped"))?;
        stdin.write(&input)?;
        stdin.close();
        let out = drain(child.stdout.take().expect("piped"), MAX_OUTPUT, io_stop.clone())?;
        let err = drain(child.stderr.take().expect("piped"), MAX_OUTPUT, io_stop.clone())?;
        Ok::<_, std::io::Error>((stdin, out, err))
    })();
    let (stdin, out, err) = match setup {
        Ok(v) => v,
        Err(e) => {
            io_stop.store(true, Ordering::Relaxed);
            kill_group(&mut child);
            let _ = child.wait();
            return Err(e.to_string());
        }
    };
    let end = Instant::now() + timeout;
    let mut status = None;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => status = Some(st),
            Ok(None) => {}
            Err(e) => {
                io_stop.store(true, Ordering::Relaxed);
                stdin.cancel();
                kill_group(&mut child);
                let _ = child.wait();
                return Err(e.to_string());
            }
        }
        if status.is_some() && out.is_finished() && err.is_finished() && stdin.is_finished() {
            break;
        }
        if stop.load(Ordering::Relaxed) || Instant::now() >= end {
            kill_group(&mut child);
            let _ = child.wait();
            stdin.cancel();
            io_stop.store(true, Ordering::Relaxed);
            status = None;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let (stdout, stderr) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
    if stop.load(Ordering::Relaxed) {
        return Err("stopped".into());
    }
    Ok(Output { exit_code: status.and_then(|s| s.code()), stdout, stderr })
}

trait Pipe: Read + AsFd + Send {}
impl<T: Read + AsFd + Send> Pipe for T {}

struct Live {
    child: Arc<Mutex<Child>>,
    stdin: Option<InputWriter>,
}
impl Drop for Live {
    fn drop(&mut self) {
        if let Some(stdin) = &self.stdin {
            stdin.cancel();
        }
    }
}

/// Processes from `spawn`. Dropping it kills them.
#[derive(Default)]
pub struct Processes {
    next: u32,
    live: HashMap<u32, Live>,
    /// Ended processes the waiter threads report (removed on the next call).
    ended: Arc<Mutex<Vec<u32>>>,
}

impl Processes {
    fn reap(&mut self) {
        if let Ok(mut e) = self.ended.lock() {
            for id in e.drain(..) {
                self.live.remove(&id);
            }
        }
    }

    pub fn spawn(&mut self, program: &Path, cmd: &Command, cwd: &Path, post: Poster) -> Result<u32, String> {
        self.reap();
        if self.live.len() >= MAX_PROCESSES {
            return Err(format!("at most {MAX_PROCESSES} processes at a time"));
        }
        let name = &cmd.argv[0];
        let mut child = build(program, cmd, cwd)?.spawn().map_err(|e| format!("{name}: {e}"))?;
        self.next = self.next.wrapping_add(1).max(1);
        let id = self.next;
        let stdin = match InputWriter::new(child.stdin.take().expect("piped")) {
            Ok(w) => Some(w),
            Err(e) => {
                kill_group(&mut child);
                let _ = child.wait();
                return Err(e.to_string());
            }
        };
        let readers: Vec<_> = [
            (false, Box::new(child.stdout.take().expect("piped")) as Box<dyn Pipe>),
            (true, Box::new(child.stderr.take().expect("piped"))),
        ]
        .into_iter()
        .map(|(stderr, mut r)| {
            let post = post.clone();
            let nonblocking = rustix::io::ioctl_fionbio(r.as_fd(), true);
            std::thread::spawn(move || {
                if nonblocking.is_err() {
                    return;
                }
                let mut buf = vec![0u8; 65536];
                while !post.stopped() {
                    match r.read(&mut buf) {
                        Ok(0) => return,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            wait_readable(r.as_fd());
                            continue;
                        }
                        Err(_) => return,
                        Ok(n) => {
                            let e = Event::ProcessOutput(ProcessOutput { id, stderr, data: buf[..n].to_vec() });
                            if !post.data(n, e) {
                                return;
                            }
                        }
                    }
                }
            })
        })
        .collect();
        let child = Arc::new(Mutex::new(child));
        let (c, ended) = (child.clone(), self.ended.clone());
        std::thread::spawn(move || {
            for r in readers {
                let _ = r.join();
            }
            let code = loop {
                let st = c.lock().map(|mut c| c.try_wait());
                match st {
                    Ok(Ok(Some(st))) => break st.code(),
                    Ok(Ok(None)) => std::thread::sleep(Duration::from_millis(10)),
                    _ => break None,
                }
            };
            if let Ok(mut e) = ended.lock() {
                e.push(id);
            }
            post.event(Event::ProcessExited((id, code)));
        });
        self.live.insert(id, Live { child, stdin });
        Ok(id)
    }

    pub fn write(&mut self, id: u32, data: &[u8]) -> Result<(), String> {
        self.reap();
        let p = self.live.get_mut(&id).ok_or_else(|| format!("no process {id}"))?;
        let s = p.stdin.as_mut().ok_or("stdin is closed")?;
        s.write(data).map_err(|e| e.to_string())
    }

    pub fn close_stdin(&mut self, id: u32) {
        if let Some(p) = self.live.get_mut(&id)
            && let Some(stdin) = p.stdin.as_mut()
        {
            stdin.close();
        }
    }

    pub fn kill(&mut self, id: u32) {
        if let Some(p) = self.live.get(&id)
            && let Ok(mut c) = p.child.lock()
        {
            if let Some(stdin) = &p.stdin {
                stdin.cancel();
            }
            kill_group(&mut c);
        }
    }
}

impl Drop for Processes {
    fn drop(&mut self) {
        for p in self.live.values() {
            if let Ok(mut c) = p.child.lock() {
                kill_group(&mut c);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_covers_descendants_holding_pipes() {
        let cmd = Command { argv: vec!["sh".into(), "-c".into(), "sleep 10 &".into()], cwd: None, env: vec![] };
        let start = Instant::now();
        let out = run(
            Path::new("/bin/sh"),
            &cmd,
            Path::new("/"),
            vec![],
            Duration::from_millis(100),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(out.exit_code.is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn timeout_kills_descendants_that_leave_the_process_group() {
        let marker = std::env::temp_dir().join(format!("proteus-setsid-{}", std::process::id()));
        let script = format!("setsid sh -c 'sleep 0.5; touch {}' & wait", marker.display());
        let cmd = Command { argv: vec!["sh".into(), "-c".into(), script], cwd: None, env: vec![] };
        let out = run(
            Path::new("/bin/sh"),
            &cmd,
            Path::new("/"),
            vec![],
            Duration::from_millis(100),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(out.exit_code.is_none());
        std::thread::sleep(Duration::from_secs(1));
        assert!(!marker.exists(), "the setsid child survived");
    }

    #[test]
    fn spawned_stdin_backpressure_does_not_block_stop() {
        let (post, _events) = Poster::testing();
        let mut processes = Processes::default();
        let cmd = Command { argv: vec!["sh".into(), "-c".into(), "sleep 10".into()], cwd: None, env: vec![] };
        let id = processes.spawn(Path::new("/bin/sh"), &cmd, Path::new("/"), post).unwrap();
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
        let cmd = Command {
            argv: vec!["sh".into(), "-c".into(), "cat; echo err >&2; exit 3".into()],
            cwd: None,
            env: vec![],
        };
        let sh = resolve(&allowed, "sh").unwrap();
        let stop = AtomicBool::new(false);
        let o = run(&sh, &cmd, Path::new("/"), b"in".to_vec(), DEFAULT_TIMEOUT, &stop).unwrap();
        assert_eq!((o.exit_code, &o.stdout[..], &o.stderr[..]), (Some(3), &b"in"[..], &b"err\n"[..]));
        let slow = Command { argv: vec!["sh".into(), "-c".into(), "sleep 10".into()], cwd: None, env: vec![] };
        let t = Instant::now();
        let o = run(&sh, &slow, Path::new("/"), vec![], Duration::from_millis(100), &stop).unwrap();
        assert_eq!(o.exit_code, None);
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn children_get_a_small_environment_and_plugins_cannot_set_loader_variables() {
        let allowed = vec!["sh".to_string()];
        let sh = resolve(&allowed, "sh").unwrap();
        let stop = AtomicBool::new(false);
        let env =
            |env: Vec<(String, String)>| Command { argv: vec!["sh".into(), "-c".into(), "env".into()], cwd: None, env };
        let o =
            run(&sh, &env(vec![("PROTEUS_MODE".into(), "x".into())]), Path::new("/"), vec![], DEFAULT_TIMEOUT, &stop)
                .unwrap();
        let out = String::from_utf8(o.stdout).unwrap();
        assert!(out.lines().any(|l| l == "PROTEUS_MODE=x"));
        assert!(out.lines().any(|l| l == format!("PATH={TRUSTED_PATH}")));
        for line in out.lines() {
            let name = line.split('=').next().unwrap();
            assert!(inherited(name) || ["PATH", "PROTEUS_MODE", "PWD", "SHLVL", "_"].contains(&name), "{line}");
        }
        for bad in ["LD_PRELOAD", "GIT_SSH_COMMAND", "PATH", "BASH_ENV", "HOME", "proteus_x", "PROTEUS_"] {
            let e = run(&sh, &env(vec![(bad.into(), "x".into())]), Path::new("/"), vec![], DEFAULT_TIMEOUT, &stop);
            assert!(e.unwrap_err().contains("may not set"), "{bad}");
        }
    }
}
