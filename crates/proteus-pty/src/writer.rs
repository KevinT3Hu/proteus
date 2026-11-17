//! Bounded, cancellable stdin delivery. Callers enqueue whole messages;
//! one worker preserves their order and retries partial nonblocking writes
//! whenever the descriptor becomes writable again.
use std::collections::VecDeque;
use std::io::{self, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec};

pub const MAX_PENDING_INPUT: usize = 16 << 20;
/// How long `close` keeps trying to deliver queued input to a child that
/// stopped reading before giving up and releasing the descriptor.
const DRAIN: Duration = Duration::from_secs(5);

#[derive(Default)]
struct State {
    queue: VecDeque<Vec<u8>>,
    closing: bool,
    drain_until: Option<Instant>,
    finished: bool,
    error: Option<String>,
}
struct Shared {
    state: Mutex<State>,
    /// Wakes the worker waiting for queued input.
    wake: Condvar,
    /// Wakes the worker waiting for the descriptor to become writable.
    kick: OwnedFd,
    cancelled: AtomicBool,
    /// Queued bytes not written yet.
    bytes: AtomicUsize,
}

impl Shared {
    /// Wake the worker wherever it waits, after a change to `state` or
    /// `cancelled`.
    fn notify(&self) {
        drop(self.state.lock().unwrap());
        self.wake.notify_all();
        let _ = rustix::io::write(&self.kick, &1u64.to_ne_bytes());
    }

    /// Wait until `output` accepts more bytes, [`Shared::notify`] is
    /// called, or `deadline` passes.
    fn wait_writable(&self, output: impl AsFd, deadline: Option<Instant>) -> io::Result<()> {
        let timeout = deadline
            .map(|t| Timespec::try_from(t.saturating_duration_since(Instant::now())))
            .transpose()
            .map_err(io::Error::other)?;
        let mut fds = [PollFd::new(&output, PollFlags::OUT), PollFd::new(&self.kick, PollFlags::IN)];
        match rustix::event::poll(&mut fds, timeout.as_ref()) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
        if fds[1].revents().contains(PollFlags::IN) {
            let _ = rustix::io::read(&self.kick, &mut [0u8; 8]);
        }
        Ok(())
    }
}

/// Dropping cancels pending input and releases the descriptor. `close` instead
/// drains queued input before closing, so EOF cannot overtake queued bytes.
/// Setting nonblocking affects duplicate descriptors too; PTY readers must
/// handle WouldBlock when they share the same open file description.
pub struct InputWriter {
    shared: Arc<Shared>,
    draining: bool,
}

impl InputWriter {
    pub fn new(mut output: impl AsFd + Write + Send + 'static) -> io::Result<Self> {
        rustix::io::ioctl_fionbio(output.as_fd(), true)?;
        let kick = rustix::event::eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            wake: Condvar::new(),
            kick,
            cancelled: AtomicBool::new(false),
            bytes: AtomicUsize::new(0),
        });
        let worker = shared.clone();
        std::thread::Builder::new().name("stdin-writer".into()).spawn(move || {
            let cancelled = || worker.cancelled.load(Ordering::Acquire);
            let result = (|| -> io::Result<()> {
                loop {
                    let mut state = worker.state.lock().unwrap();
                    while state.queue.is_empty() && !state.closing && !cancelled() {
                        state = worker.wake.wait(state).unwrap();
                    }
                    if cancelled() || state.queue.is_empty() {
                        return Ok(());
                    }
                    let data = state.queue.pop_front().expect("queued input");
                    drop(state);
                    let mut offset = 0;
                    while offset < data.len() {
                        if cancelled() {
                            return Ok(());
                        }
                        match output.write(&data[offset..]) {
                            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                            Ok(n) => {
                                offset += n;
                                worker.bytes.fetch_sub(n, Ordering::Relaxed);
                            }
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                let deadline = worker.state.lock().unwrap().drain_until;
                                if deadline.is_some_and(|t| Instant::now() >= t) {
                                    tracing::warn!(
                                        pending_bytes = worker.bytes.load(Ordering::Relaxed),
                                        timeout = ?DRAIN,
                                        "child stopped reading input; dropping queued input on close"
                                    );
                                    return Ok(());
                                }
                                worker.wait_writable(&output, deadline)?;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
            })();
            drop(output);
            let mut state = worker.state.lock().unwrap();
            let pending_bytes = worker.bytes.load(Ordering::Relaxed);
            if let Err(e) = &result {
                tracing::warn!(error = %e, pending_bytes, "writing input to the child failed");
            } else if cancelled() && pending_bytes > 0 {
                tracing::debug!(pending_bytes, "input writer cancelled with queued input");
            }
            state.finished = true;
            state.error = result.err().map(|e| e.to_string());
            state.queue.clear();
            worker.bytes.store(0, Ordering::Relaxed);
            worker.wake.notify_all();
        })?;
        Ok(Self { shared, draining: false })
    }

    /// Enqueue all bytes or reject the whole message. Never blocks on I/O.
    pub fn write(&self, bytes: &[u8]) -> io::Result<()> {
        let mut s = self.shared.state.lock().unwrap();
        if let Some(e) = &s.error {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, e.clone()));
        }
        if s.closing || s.finished || self.shared.cancelled.load(Ordering::Acquire) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let pending = self.shared.bytes.load(Ordering::Relaxed);
        if bytes.len() > MAX_PENDING_INPUT.saturating_sub(pending) {
            tracing::warn!(len = bytes.len(), pending_bytes = pending, "stdin queue full; input refused");
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "stdin queue is full (16 MiB)"));
        }
        if !bytes.is_empty() {
            self.shared.bytes.fetch_add(bytes.len(), Ordering::Relaxed);
            s.queue.push_back(bytes.to_vec());
            self.shared.wake.notify_all();
        }
        Ok(())
    }
    pub fn close(&mut self) {
        self.draining = true;
        let mut s = self.shared.state.lock().unwrap();
        s.closing = true;
        s.drain_until = Some(Instant::now() + DRAIN);
        drop(s);
        self.shared.notify();
    }
    pub fn is_finished(&self) -> bool {
        self.shared.state.lock().unwrap().finished
    }
    pub fn cancel(&self) {
        self.shared.cancelled.store(true, Ordering::Release);
        self.shared.notify();
    }
}
impl Drop for InputWriter {
    fn drop(&mut self) {
        if !self.draining {
            self.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    #[test]
    fn backpressure_is_bounded_and_cancellable() {
        let (output, _reader) = UnixStream::pair().unwrap();
        let mut writer = InputWriter::new(output).unwrap();
        let start = Instant::now();
        writer.write(&vec![1; MAX_PENDING_INPUT]).unwrap();
        assert_eq!(writer.write(&vec![2; MAX_PENDING_INPUT]).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert!(start.elapsed() < Duration::from_secs(1));
        writer.close();
        writer.cancel();
        let end = Instant::now() + Duration::from_secs(1);
        while !writer.is_finished() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(writer.is_finished());
    }
    #[test]
    fn close_delivers_input_in_order_before_eof() {
        let (output, mut reader) = UnixStream::pair().unwrap();
        let mut writer = InputWriter::new(output).unwrap();
        writer.write(b"first").unwrap();
        writer.write(b"second").unwrap();
        writer.close();
        drop(writer);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"firstsecond");
    }
}
