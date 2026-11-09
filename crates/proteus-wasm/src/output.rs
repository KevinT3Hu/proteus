//! A plugin's stdout and stderr. Complete lines go to the log, limited in
//! length and rate so a chatty plugin cannot flood it, and the end of
//! stderr is kept to explain a crash (Rust guests print panics there).
//! Writes always succeed at once: the guest never waits or fails on them.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::AsyncWrite;
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};

/// Characters of one logged line.
const MAX_LINE: usize = 1024;
/// Bytes buffered for a line without a newline before it is cut.
const MAX_PARTIAL: usize = 4 * MAX_LINE;
/// Bytes per second of output logged; the rest of the second is dropped.
const RATE: usize = 16 << 10;
const TAIL_LINES: usize = 16;
const TAIL_BYTES: usize = 4 << 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stream {
    Stdout,
    Stderr,
}

/// One output stream of one plugin instance.
#[derive(Clone)]
pub(crate) struct GuestOutput(Arc<Mutex<Lines>>);

struct Lines {
    plugin: String,
    stream: Stream,
    partial: Vec<u8>,
    tail: VecDeque<String>,
    tail_bytes: usize,
    window: Instant,
    logged: usize,
    truncated: bool,
}

impl GuestOutput {
    pub fn new(plugin: &str, stream: Stream) -> Self {
        GuestOutput(Arc::new(Mutex::new(Lines {
            plugin: plugin.to_string(),
            stream,
            partial: Vec::new(),
            tail: VecDeque::new(),
            tail_bytes: 0,
            window: Instant::now(),
            logged: 0,
            truncated: false,
        })))
    }

    fn lines(&self) -> MutexGuard<'_, Lines> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn write(&self, bytes: &[u8]) {
        let mut l = self.lines();
        for piece in bytes.split_inclusive(|&b| b == b'\n') {
            let room = MAX_PARTIAL - l.partial.len();
            l.partial.extend_from_slice(&piece[..piece.len().min(room)]);
            if piece.ends_with(b"\n") || l.partial.len() >= MAX_PARTIAL {
                l.end_line();
            }
        }
    }

    /// The last lines written, including an unfinished one.
    pub fn tail(&self) -> String {
        let l = self.lines();
        let mut out: Vec<String> = l.tail.iter().cloned().collect();
        if !l.partial.is_empty() {
            out.push(printable(&String::from_utf8_lossy(&l.partial)).into_owned());
        }
        out.join("\n")
    }

    /// The guest's last panic, as `panicked at FILE:LINE:COL: MESSAGE`, or
    /// its failed allocation.
    pub fn panic(&self) -> Option<String> {
        let tail = self.tail();
        panic_summary(&tail).or_else(|| {
            tail.lines().rev().find(|l| l.starts_with("memory allocation of ") && l.ends_with(" failed")).map(short)
        })
    }
}

impl Lines {
    fn end_line(&mut self) {
        let raw = std::mem::take(&mut self.partial);
        let text = String::from_utf8_lossy(&raw);
        let text = text.trim_end_matches(['\n', '\r']);
        if text.trim().is_empty() {
            return;
        }
        let text = printable(text);
        let mut line: String = text.chars().take(MAX_LINE).collect();
        if text.chars().nth(MAX_LINE).is_some() {
            line.push('…');
        }
        if self.stream == Stream::Stderr {
            self.tail_bytes += line.len();
            self.tail.push_back(line.clone());
            while self.tail.len() > TAIL_LINES || (self.tail_bytes > TAIL_BYTES && self.tail.len() > 1) {
                if let Some(old) = self.tail.pop_front() {
                    self.tail_bytes -= old.len();
                }
            }
        }
        if self.window.elapsed() >= Duration::from_secs(1) {
            self.window = Instant::now();
            self.logged = 0;
            self.truncated = false;
        }
        if self.logged + line.len() > RATE {
            if !self.truncated {
                self.truncated = true;
                tracing::warn!(target: "plugin", plugin = %self.plugin, stream = ?self.stream, "output truncated");
            }
            return;
        }
        self.logged += line.len();
        match self.stream {
            Stream::Stderr => tracing::warn!(target: "plugin", plugin = %self.plugin, "{line}"),
            Stream::Stdout => tracing::info!(target: "plugin", plugin = %self.plugin, "{line}"),
        }
    }
}

/// `s` with control characters escaped, safe to put in a log line.
pub fn printable(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.chars().any(|c| c.is_control() && c != '\t') {
        return s.into();
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if c.is_control() && c != '\t' {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out.into()
}

/// The panic in std's report: `thread 'NAME' panicked at FILE:LINE:COL:`
/// then the message, up to the `note:` lines.
pub(crate) fn panic_summary(text: &str) -> Option<String> {
    const AT: &str = "panicked at ";
    let rest = &text[text.rfind(AT)? + AT.len()..];
    let (first, after) = rest.split_once('\n').unwrap_or((rest, ""));
    let first = first.trim_end();
    // Before Rust 1.73: `panicked at 'MESSAGE', FILE:LINE:COL`.
    if first.starts_with('\'') {
        return Some(format!("panicked at {}", short(first)));
    }
    let location = first.trim_end_matches(':');
    let message: Vec<&str> = after
        .lines()
        .take_while(|l| !l.starts_with("note: ") && !l.starts_with("stack backtrace:"))
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    Some(if message.is_empty() {
        format!("panicked at {location}")
    } else {
        format!("panicked at {location}: {}", short(&message.join(" ")))
    })
}

fn short(s: &str) -> String {
    const MAX: usize = 300;
    if s.chars().count() <= MAX { s.to_string() } else { s.chars().take(MAX).chain(['…']).collect() }
}

impl IsTerminal for GuestOutput {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for GuestOutput {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(self.clone())
    }
}

impl AsyncWrite for GuestOutput {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.write(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panics_are_found_in_std_reports() {
        let report = "starting\nthread '<unnamed>' panicked at src/lib.rs:1485:41:\nno pids on this platform\n\
                      note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n";
        assert_eq!(panic_summary(report).as_deref(), Some("panicked at src/lib.rs:1485:41: no pids on this platform"));
        let lines = "thread 'main' panicked at a.rs:1:2:\nfirst\nsecond";
        assert_eq!(panic_summary(lines).as_deref(), Some("panicked at a.rs:1:2: first second"));
        assert_eq!(
            panic_summary("thread 'x' panicked at 'boom', a.rs:3:4").as_deref(),
            Some("panicked at 'boom', a.rs:3:4")
        );
        assert_eq!(panic_summary("thread 'x' panicked at a.rs:3:4:").as_deref(), Some("panicked at a.rs:3:4"));
        assert_eq!(panic_summary("just output\n"), None);
    }

    #[test]
    fn stderr_tail_is_bounded_and_split_writes_join() {
        let out = GuestOutput::new("t", Stream::Stderr);
        for i in 0..100 {
            out.write(format!("line {i}\n").as_bytes());
        }
        out.write(b"thread '<unnamed>' panicked at src/");
        out.write(b"lib.rs:7:9:\nfixture \x1b[31mpanic\n");
        let tail = out.tail();
        assert!(!tail.contains("line 80"), "{tail}");
        assert!(tail.lines().count() <= TAIL_LINES + 1);
        assert_eq!(out.panic().as_deref(), Some("panicked at src/lib.rs:7:9: fixture \\u{1b}[31mpanic"));
        out.write(&vec![b'x'; 100_000]);
        assert!(out.tail().len() <= TAIL_BYTES + MAX_PARTIAL + 16);
    }
}
