//! Starting the child process in a PTY.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

use proteus_core::Config;
use proteus_pty::shell::{self, Shell};
use proteus_pty::{CommandSpec, Pty, WindowSize};

/// Start the command (or the user's shell) in a new PTY with `TERM` and,
/// optionally, shell integration set up.
pub fn spawn_pty(opts: &SpawnOptions) -> io::Result<(Pty, Option<Shell>)> {
    let mut spec = match &opts.command {
        Some(argv) if !argv.is_empty() => CommandSpec::new(&argv[0]).args(argv[1..].iter().cloned()),
        _ => CommandSpec::default_shell(),
    };
    if let Some(dir) = opts.cwd.as_ref().filter(|d| d.is_dir()) {
        spec.cwd = Some(dir.clone());
    }
    let terminfo = shell::data_dir().and_then(|d| shell::install_terminfo(&d.join("terminfo")));
    shell::apply_term(&mut spec, terminfo.as_deref());
    if let Some(term) = &opts.term {
        spec.set_env("TERM", term);
    }
    // Do not inherit a parent terminal's control endpoint in a nested instance.
    if let Some(path) = &opts.ipc_socket {
        spec.set_env("PROTEUS_SOCKET", path);
    } else {
        spec.env.push(("PROTEUS_SOCKET".into(), None));
    }
    let mut sh = None;
    if opts.integration {
        let dir = shell::runtime_dir().join("shell-integration");
        match shell::install(&dir) {
            Ok(()) => sh = shell::inject(&mut spec, &dir),
            Err(e) => tracing::warn!(dir = %dir.display(), "shell integration disabled: {e}"),
        }
    }
    tracing::debug!(program = %spec.program.display(), args = ?spec.args, "spawning");
    let pty = Pty::spawn(&spec, window_size(opts.cols, opts.rows, opts.cell))?;
    tracing::info!(
        program = %spec.program.display(),
        cwd = %spec.cwd.as_deref().map(|d| d.display().to_string()).unwrap_or_else(|| "(inherited)".into()),
        pid = pty.child_pid(),
        "pane spawned"
    );
    Ok((pty, sh))
}

/// Emulation config for a local session.
pub fn local_config() -> Config {
    let mut config = Config::default();
    config.graphics.hostname = Some(rustix::system::uname().nodename().to_string_lossy().into_owned());
    config
}

pub fn window_size(cols: u16, rows: u16, cell: Option<(u16, u16)>) -> WindowSize {
    let mut ws = WindowSize::new(cols, rows);
    if let Some((w, h)) = cell {
        ws.pixel_width = cols.saturating_mul(w);
        ws.pixel_height = rows.saturating_mul(h);
    }
    ws
}

pub struct SpawnOptions {
    /// Program and arguments; `None` runs the user's shell.
    pub command: Option<Vec<OsString>>,
    pub integration: bool,
    pub ipc_socket: Option<PathBuf>,
    pub term: Option<String>,
    /// Working directory; `None` inherits ours.
    pub cwd: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
    /// Cell size in pixels, if known (reported to apps and used for images).
    pub cell: Option<(u16, u16)>,
}
