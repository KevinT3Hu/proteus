//! Diagnostics through `tracing`: warnings and errors on stderr, and a
//! detailed log file in the state directory.
//!
//! `PROTEUS_LOG` takes an `EnvFilter` directive (`debug`,
//! `proteus_wasm=trace,info`, ...). Without it the file records `info` and
//! above from Proteus (`debug` with `PROTEUS_DEBUG` set) and warnings from
//! dependencies; stderr shows warnings and errors unless `PROTEUS_LOG` is set.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

/// The log file is moved to `proteus.log.old` at startup beyond this size.
const ROTATE_AT: u64 = 8 << 20;

const CRATES: [&str; 5] = ["proteus", "proteus_core", "proteus_pty", "proteus_wasm", "plugin"];

fn default_filter(level: &str) -> EnvFilter {
    let mut f = EnvFilter::new("warn");
    for c in CRATES {
        f = f.add_directive(format!("{c}={level}").parse().expect("valid directive"));
    }
    f
}

fn filter(level: &str) -> EnvFilter {
    match std::env::var("PROTEUS_LOG") {
        Ok(spec) if !spec.trim().is_empty() => EnvFilter::try_new(&spec).unwrap_or_else(|e| {
            eprintln!("proteus: PROTEUS_LOG: {e}; using defaults");
            default_filter(level)
        }),
        _ => default_filter(level),
    }
}

/// `$XDG_STATE_HOME/proteus/proteus.log` (default `~/.local/state/...`).
pub fn log_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(base.join("proteus").join("proteus.log"))
}

/// Appending, private, never through a symlink.
fn open_log(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().ok_or_else(|| io::Error::other("log path has no directory"))?;
    proteus_pty::secure_fs::state_dir(dir)?;
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.len() > ROTATE_AT) {
        std::fs::rename(path, path.with_extension("log.old"))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
}

fn tty() -> bool {
    use std::io::IsTerminal;
    io::stderr().is_terminal()
}

/// For command-line tools: warnings and errors on stderr only.
pub fn init_stderr() {
    let layer = fmt::layer().with_writer(io::stderr).with_ansi(tty()).with_target(false).without_time();
    let _ = tracing_subscriber::registry().with(layer.with_filter(filter("warn"))).try_init();
}

/// For the terminal itself: stderr plus the log file. Returns the file in
/// use, if it could be opened.
pub fn init() -> Option<PathBuf> {
    let level = if std::env::var_os("PROTEUS_DEBUG").is_some() { "debug" } else { "info" };
    let explicit = std::env::var("PROTEUS_LOG").is_ok_and(|s| !s.trim().is_empty());
    let stderr = fmt::layer()
        .with_writer(io::stderr)
        .with_ansi(tty())
        .with_target(explicit)
        .without_time()
        .with_filter(if explicit { filter(level) } else { filter("warn") });
    let (file, path, error) = match log_path().map(|p| (open_log(&p), p)) {
        Some((Ok(f), p)) => (Some(f), Some(p), None),
        Some((Err(e), p)) => (None, None, Some(format!("{}: {e}", p.display()))),
        None => (None, None, Some("no state directory (HOME is unset)".into())),
    };
    let file_layer = file.map(|f| {
        fmt::layer().with_writer(Mutex::new(f)).with_ansi(false).with_thread_names(true).with_filter(filter(level))
    });
    if tracing_subscriber::registry().with(stderr).with(file_layer).try_init().is_err() {
        return None;
    }
    install_panic_hook();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), pid = std::process::id(), "proteus started");
    if let Some(e) = error {
        tracing::warn!("log file unavailable: {e}");
    }
    path
}

/// Panics are logged with their thread, location and a backtrace.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(non-string payload)".into());
        let location = info.location().map_or_else(|| "unknown location".into(), |l| l.to_string());
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(
            thread = thread.name().unwrap_or("unnamed"),
            %location,
            "panic: {payload}\n{backtrace}"
        );
        let _ = io::stderr().flush();
    }));
}
