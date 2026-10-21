//! Events the terminal raises for its host (GUI / multiplexer).

use std::time::Duration;

use crate::blocks::BlockId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardKind {
    Clipboard,
    Primary,
}

/// How an OSC string was terminated; replies use the same terminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminator {
    Bel,
    St,
}

impl Terminator {
    pub fn as_str(self) -> &'static str {
        match self {
            Terminator::Bel => "\x07",
            Terminator::St => "\x1b\\",
        }
    }
}

/// Working directory reported by OSC 7.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cwd {
    /// Host part of the `file://` URL; `None` or empty means local.
    pub host: Option<String>,
    pub path: String,
}

impl Cwd {
    /// True if the cwd is on this machine, given the local hostname.
    pub fn is_local(&self, hostname: &str) -> bool {
        match self.host.as_deref() {
            None | Some("") | Some("localhost") => true,
            Some(h) => h == hostname,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Bell,
    TitleChanged(String),
    CwdChanged(Cwd),
    /// OSC 52 write (allowed by policy).
    ClipboardStore { kind: ClipboardKind, text: String },
    /// OSC 52 read request: the host must ask the user, then call
    /// [`crate::Terminal::respond_clipboard`] (or ignore it).
    ClipboardLoad { kind: ClipboardKind, terminator: Terminator },
    /// OSC 9 / OSC 777 desktop notification (already rate-limited).
    Notification { title: Option<String>, body: String },
    PromptStarted { block: BlockId },
    CommandStarted { block: BlockId, command: Option<String> },
    CommandFinished { block: BlockId, exit_code: Option<i32>, duration: Option<Duration> },
}
