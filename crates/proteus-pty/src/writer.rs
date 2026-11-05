//! Bounded, cancellable stdin delivery. Callers enqueue whole messages;
//! one worker preserves their order and retries partial nonblocking writes.
use std::collections::VecDeque;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub const MAX_PENDING_INPUT: usize = 16 << 20;
const RETRY: Duration = Duration::from_millis(5);
/// How long `close` keeps trying to deliver queued input to a child that
/// stopped reading before giving up and releasing the descriptor.
const DRAIN: Duration = Duration::from_secs(5);

#[derive(Default)]
struct State {
    queue: VecDeque<Vec<u8>>,
    bytes: usize,
    closing: bool,
    drain_until: Option<std::time::Instant>,
    cancelled: bool,
    finished: bool,
    error: Option<String>,
}
#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
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
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        std::thread::Builder::new().name("stdin-writer".into()).spawn(move || {
            let result = (|| -> io::Result<()> {
                loop {
                    let mut state = worker.state.lock().unwrap();
                    while state.queue.is_empty() && !state.closing && !state.cancelled {
                        state = worker.wake.wait(state).unwrap();
                    }
                    if state.cancelled || state.queue.is_empty() {
                        return Ok(());
                    }
                    let data = state.queue.pop_front().expect("queued input");
                    drop(state);
                    let mut offset = 0;
                    while offset < data.len() {
                        if worker.state.lock().unwrap().cancelled {
                            return Ok(());
                        }
                        match output.write(&data[offset..]) {
                            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                            Ok(n) => {
                                offset += n;
                                worker.state.lock().unwrap().bytes -= n;
                            }
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                let state = worker.state.lock().unwrap();
                                if state.cancelled || state.drain_until.is_some_and(|t| std::time::Instant::now() >= t)
                                {
                                    return Ok(());
                                }
                                let _ = worker.wake.wait_timeout(state, RETRY).unwrap();
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
            })();
            drop(output);
            let mut state = worker.state.lock().unwrap();
            state.finished = true;
            state.error = result.err().map(|e| e.to_string());
            state.queue.clear();
            state.bytes = 0;
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
        if s.closing || s.cancelled || s.finished {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if bytes.len() > MAX_PENDING_INPUT.saturating_sub(s.bytes) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "stdin queue is full (16 MiB)"));
        }
        if !bytes.is_empty() {
            s.bytes += bytes.len();
            s.queue.push_back(bytes.to_vec());
            self.shared.wake.notify_all();
        }
        Ok(())
    }
    pub fn close(&mut self) {
        self.draining = true;
        let mut s = self.shared.state.lock().unwrap();
        s.closing = true;
        s.drain_until = Some(std::time::Instant::now() + DRAIN);
        drop(s);
        self.shared.wake.notify_all();
    }
    pub fn is_finished(&self) -> bool {
        self.shared.state.lock().unwrap().finished
    }
    pub fn cancel(&self) {
        self.shared.state.lock().unwrap().cancelled = true;
        self.shared.wake.notify_all();
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
    use std::time::Instant;

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
            std::thread::sleep(RETRY);
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
