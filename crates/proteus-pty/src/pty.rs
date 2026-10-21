use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

use rustix::fs::{Mode, OFlags};
use rustix::process::Pid;
use rustix::pty::OpenptFlags;
use rustix::termios::{self, InputModes, OptionalActions, Winsize};

/// Terminal size in cells and pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowSize {
    pub cols: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl WindowSize {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows, pixel_width: 0, pixel_height: 0 }
    }

    fn to_winsize(self) -> Winsize {
        Winsize { ws_row: self.rows, ws_col: self.cols, ws_xpixel: self.pixel_width, ws_ypixel: self.pixel_height }
    }
}

/// What to run in the PTY. Environment changes apply on top of ours.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// `None` value = remove the variable.
    pub env: Vec<(OsString, Option<OsString>)>,
    pub cwd: Option<PathBuf>,
}

impl CommandSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self { program: program.into(), args: Vec::new(), env: Vec::new(), cwd: None }
    }

    /// The user's login shell: `$SHELL`, else the passwd entry, else `/bin/sh`.
    pub fn default_shell() -> Self {
        Self::new(user_shell())
    }

    pub fn arg(mut self, a: impl Into<OsString>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: Into<OsString>>(mut self, args: I) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, k: impl Into<OsString>, v: impl Into<OsString>) -> Self {
        self.set_env(k, v);
        self
    }

    pub fn set_env(&mut self, k: impl Into<OsString>, v: impl Into<OsString>) {
        let k = k.into();
        self.env.retain(|(name, _)| *name != k);
        self.env.push((k, Some(v.into())));
    }

    pub fn remove_env(&mut self, k: impl Into<OsString>) {
        let k = k.into();
        self.env.retain(|(name, _)| *name != k);
        self.env.push((k, None));
    }

    /// Effective value of `k` for the child (overrides, else our environment).
    pub fn get_env(&self, k: &str) -> Option<OsString> {
        match self.env.iter().rev().find(|(name, _)| name == k) {
            Some((_, v)) => v.clone(),
            None => std::env::var_os(k),
        }
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// File name of the program, e.g. `zsh` for `/usr/bin/zsh`.
    pub fn program_name(&self) -> &OsStr {
        self.program.file_name().unwrap_or(self.program.as_os_str())
    }
}

fn user_shell() -> PathBuf {
    if let Some(s) = std::env::var_os("SHELL").filter(|s| !s.is_empty()) {
        return s.into();
    }
    let uid = rustix::process::getuid().as_raw().to_string();
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    passwd
        .lines()
        .map(|l| l.split(':').collect::<Vec<_>>())
        .find(|f| f.len() >= 7 && f[2] == uid && !f[6].is_empty())
        .map(|f| PathBuf::from(f[6]))
        .unwrap_or_else(|| "/bin/sh".into())
}

/// The process in the foreground of the terminal (`tcgetpgrp`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundProcess {
    pub pid: i32,
    /// `/proc/<pid>/comm`.
    pub name: String,
    pub cwd: Option<PathBuf>,
}

/// A child process attached to a pseudo-terminal.
pub struct Pty {
    master: File,
    child: Child,
}

/// Open a master and its slave. Prefers `TIOCGPTPEER` (no path lookup race).
fn open_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let master = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)?;
    rustix::pty::grantpt(&master)?;
    rustix::pty::unlockpt(&master)?;
    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
    let slave = match rustix::pty::ioctl_tiocgptpeer(&master, flags) {
        Ok(fd) => fd,
        Err(_) => {
            let name = rustix::pty::ptsname(&master, Vec::new())?;
            rustix::fs::open(name.as_c_str(), OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC, Mode::empty())?
        }
    };
    // Line editing in the kernel must treat input as UTF-8.
    let mut tio = termios::tcgetattr(&slave)?;
    tio.input_modes.insert(InputModes::IUTF8);
    termios::tcsetattr(&slave, OptionalActions::Now, &tio)?;
    Ok((master, slave))
}

impl Pty {
    /// Spawn `spec` as session leader with the PTY as controlling terminal.
    /// `TERM`, `COLORTERM` and `TERM_PROGRAM*` are set unless `spec` overrides them.
    pub fn spawn(spec: &CommandSpec, size: WindowSize) -> io::Result<Pty> {
        let (master, slave) = open_pair()?;
        termios::tcsetwinsize(&slave, size.to_winsize())?;

        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave))
            .env("TERM", crate::TERM_FALLBACK)
            .env("COLORTERM", "truecolor")
            .env("TERM_PROGRAM", "proteus")
            .env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"))
            .env_remove("LINES")
            .env_remove("COLUMNS");
        for (k, v) in &spec.env {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            };
        }
        if let Some(dir) = &spec.cwd {
            cmd.current_dir(dir);
        }
        // SAFETY: only async-signal-safe syscalls run between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::setsid()?;
                rustix::process::ioctl_tiocsctty(rustix::stdio::stdin())?;
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        Ok(Pty { master: File::from(master), child })
    }

    /// Handle to read output from / write input to the child.
    pub fn master(&self) -> &File {
        &self.master
    }

    /// Independent handle (e.g. for a reader thread).
    pub fn try_clone_master(&self) -> io::Result<File> {
        self.master.try_clone()
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        rustix::io::ioctl_fionbio(&self.master, nonblocking)?;
        Ok(())
    }

    pub fn resize(&self, size: WindowSize) -> io::Result<()> {
        termios::tcsetwinsize(&self.master, size.to_winsize())?;
        Ok(())
    }

    pub fn child_pid(&self) -> i32 {
        self.child.id() as i32
    }

    /// Process group in the foreground of the terminal.
    pub fn foreground_pgrp(&self) -> Option<i32> {
        termios::tcgetpgrp(self.master.as_fd()).ok().map(Pid::as_raw_nonzero).map(|p| p.get())
    }

    /// Foreground process (name and cwd from `/proc`). Used for pane titles,
    /// cwd fallback without OSC 7, and close confirmation.
    pub fn foreground_process(&self) -> Option<ForegroundProcess> {
        let pid = self.foreground_pgrp()?;
        let proc = Path::new("/proc").join(pid.to_string());
        let name = std::fs::read_to_string(proc.join("comm")).ok()?.trim_end().to_string();
        let cwd = std::fs::read_link(proc.join("cwd")).ok();
        Some(ForegroundProcess { pid, name, cwd })
    }

    /// The application reads keys itself (canonical mode is off), as a
    /// shell's line editor does while it waits at the prompt. Typed-ahead
    /// input before that is echoed by the kernel and again by the editor.
    pub fn raw_input(&self) -> bool {
        termios::tcgetattr(&self.master).is_ok_and(|t| !t.local_modes.contains(termios::LocalModes::ICANON))
    }

    /// True when something other than the shell we started is in the foreground.
    pub fn has_foreground_job(&self) -> bool {
        self.foreground_pgrp().is_some_and(|p| p != self.child_pid())
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }

    /// Send SIGHUP to the session, as closing a terminal does.
    pub fn hangup(&self) {
        if let Some(pid) = Pid::from_raw(self.child_pid()) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::HUP);
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            self.hangup();
            // Reap if it exits promptly; otherwise leave it (no blocking in drop).
            for _ in 0..10 {
                if !matches!(self.child.try_wait(), Ok(None)) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
}

/// Reading the master after the child side is fully closed yields `EIO` on
/// Linux; treat that as end of file.
pub fn is_eof(err: &io::Error) -> bool {
    err.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error())
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    fn read_all(pty: &Pty) -> String {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        let mut f = pty.try_clone_master().unwrap();
        loop {
            match f.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if is_eof(&e) => break,
                Err(e) => panic!("{e}"),
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn spawn_reports_size_tty_and_env() {
        let spec = CommandSpec::new("/bin/sh")
            .arg("-c")
            .arg("stty size; tty >/dev/null && echo tty-ok; echo \"$TERM $FOO\"")
            .env("FOO", "bar");
        let mut pty = Pty::spawn(&spec, WindowSize::new(91, 37)).unwrap();
        let out = read_all(&pty);
        assert!(out.contains("37 91"), "{out:?}");
        assert!(out.contains("tty-ok"), "{out:?}");
        assert!(out.contains("xterm-256color bar"), "{out:?}");
        assert!(pty.wait().unwrap().success());
    }

    #[test]
    fn resize_and_foreground() {
        let spec = CommandSpec::new("/bin/sh").arg("-c").arg("sleep 0.3; stty size");
        let mut pty = Pty::spawn(&spec, WindowSize::new(80, 24)).unwrap();
        pty.resize(WindowSize::new(100, 30)).unwrap();
        let fg = pty.foreground_process();
        assert!(fg.is_some());
        let out = read_all(&pty);
        assert!(out.contains("30 100"), "{out:?}");
        pty.wait().unwrap();
    }

    #[test]
    fn detects_raw_input() {
        let spec = CommandSpec::new("/bin/sh").arg("-c").arg("stty -icanon; echo raw; sleep 0.3");
        let pty = Pty::spawn(&spec, WindowSize::new(80, 24)).unwrap();
        let mut f = pty.try_clone_master().unwrap();
        let mut buf = [0u8; 64];
        let mut out = Vec::new();
        while !String::from_utf8_lossy(&out).contains("raw") {
            let n = std::io::Read::read(&mut f, &mut buf).unwrap();
            out.extend_from_slice(&buf[..n]);
        }
        assert!(pty.raw_input());
    }
}
