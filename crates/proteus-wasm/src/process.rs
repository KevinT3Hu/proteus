//! Programs a plugin runs (permission `run-command`): looked up in `PATH`,
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

/// The program to start for `name`, if the manifest lists it.
pub fn resolve(allowed: &[String], name: &str) -> Result<PathBuf, String> {
    if !allowed.iter().any(|a| a == name) {
        return Err(format!("permission denied: run-command:{name}"));
    }
    if name.starts_with('/') {
        return if executable(Path::new(name)) { Ok(name.into()) } else { Err(format!("{name}: not found")) };
    }
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    std::env::split_paths(&path)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(name))
        .find(|p| executable(p))
        .ok_or_else(|| format!("{name}: not found in PATH"))
}

fn build(program: &Path, cmd: &Command, cwd: &Path) -> std::process::Command {
    let mut c = std::process::Command::new(program);
    c.arg0(&cmd.argv[0]).args(&cmd.argv[1..]).current_dir(cwd).process_group(0);
    c.envs(cmd.env.iter().map(|(k, v)| (k, v)));
    c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    c
}

fn kill_group(child: &mut Child) {
    let pid = rustix::process::Pid::from_raw(child.id() as i32);
    if let Some(pid) = pid {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
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
    let mut child = build(program, cmd, cwd).spawn().map_err(|e| format!("{name}: {e}"))?;
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
        let mut child = build(program, cmd, cwd).spawn().map_err(|e| format!("{name}: {e}"))?;
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
}
