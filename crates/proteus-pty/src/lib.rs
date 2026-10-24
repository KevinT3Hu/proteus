//! PTY spawning, resizing and foreground-process queries for Proteus, plus
//! automatic shell-integration injection for bash, zsh and fish.

pub mod budget;
pub mod persistence;
mod pty;
pub mod secure_fs;
mod writer;
pub use writer::{InputWriter, MAX_PENDING_INPUT};
pub mod shell;

pub use pty::{CommandSpec, ForegroundProcess, Pty, WindowSize, is_eof};

/// Value of `TERM` for children: our own entry when its terminfo is installed,
/// `xterm-256color` otherwise.
pub const TERM_FALLBACK: &str = "xterm-256color";
pub const TERM_NAME: &str = "proteus";

/// terminfo source for the `proteus` entry (compile with `tic -x`).
pub const TERMINFO_SRC: &str = include_str!("../terminfo/proteus.terminfo");
