//! ureq's pinned transport API, with cancellation at every socket operation.
//! Nonblocking sockets keep cancellation independent of a stalled peer, even
//! during CONNECT tunnelling and TLS handshakes. DNS uses ureq's resolve timeout.
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use ureq::unversioned::transport::{Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, Transport};

const POLL: Duration = Duration::from_millis(5);
#[derive(Debug, Clone)]
pub(super) struct StreamConnector {
    pub cancel: Arc<AtomicBool>,
    pub idle: Duration,
}

impl StreamConnector {
    fn check(&self, end: Instant) -> Result<(), ureq::Error> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "cancelled").into());
        }
        if Instant::now() >= end {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "network operation timed out").into());
        }
        Ok(())
    }
    fn end(&self, timeout: NextTimeout) -> Instant {
        let remaining = if timeout.after.is_not_happening() { self.idle } else { *timeout.after };
        Instant::now() + remaining.min(self.idle)
    }
}
impl<In: Transport> Connector<In> for StreamConnector {
    type Out = Box<dyn Transport>;
    fn connect(&self, details: &ConnectionDetails, chained: Option<In>) -> Result<Option<Self::Out>, ureq::Error> {
        if let Some(t) = chained {
            return Ok(Some(Box::new(t)));
        }
        let end = self.end(details.timeout);
        let mut last = None;
        for addr in &details.addrs {
            self.check(end)?;
            use rustix::net::{AddressFamily, SocketFlags, SocketType};
            let family = if addr.is_ipv4() { AddressFamily::INET } else { AddressFamily::INET6 };
            let fd = rustix::net::socket_with(
                family,
                SocketType::STREAM,
                SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
                None,
            )
            .map_err(io::Error::from)?;
            match rustix::net::connect(&fd, addr) {
                Ok(()) => {}
                Err(e) if e == rustix::io::Errno::INPROGRESS => {
                    loop {
                        self.check(end)?;
                        let mut polls = [rustix::event::PollFd::new(&fd, rustix::event::PollFlags::OUT)];
                        let wait = rustix::event::Timespec::try_from(POLL).expect("small timeout");
                        match rustix::event::poll(&mut polls, Some(&wait)) {
                            Ok(0) => continue,
                            Err(e) if e == rustix::io::Errno::INTR => continue,
                            Err(e) => return Err(io::Error::from(e).into()),
                            Ok(_) => break,
                        }
                    }
                    if let Err(e) = rustix::net::sockopt::socket_error(&fd).map_err(io::Error::from)? {
                        last = Some(io::Error::from(e));
                        continue;
                    }
                }
                Err(e) => {
                    last = Some(io::Error::from(e));
                    continue;
                }
            }
            let stream = TcpStream::from(fd);
            stream.set_nodelay(details.config.no_delay())?;
            let buffers = LazyBuffers::new(details.config.input_buffer_size(), details.config.output_buffer_size());
            return Ok(Some(Box::new(SocketTransport { stream, buffers, control: self.clone() })));
        }
        Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::ConnectionRefused, "no reachable address")).into())
    }
}
#[derive(Debug)]
struct SocketTransport {
    stream: TcpStream,
    buffers: LazyBuffers,
    control: StreamConnector,
}
impl Transport for SocketTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        let end = self.control.end(timeout);
        let mut offset = 0;
        while offset < amount {
            self.control.check(end)?;
            match self.stream.write(&self.buffers.output()[offset..amount]) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
                Ok(n) => offset += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(POLL),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        let end = self.control.end(timeout);
        loop {
            self.control.check(end)?;
            match self.stream.read(self.buffers.input_append_buf()) {
                Ok(n) => {
                    self.buffers.input_appended(n);
                    return Ok(n > 0);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(POLL),
                Err(e) => return Err(e.into()),
            }
        }
    }
    fn is_open(&mut self) -> bool {
        if self.control.cancel.load(Ordering::Relaxed) {
            return false;
        }
        let mut byte = [0];
        matches!(self.stream.peek(&mut byte), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
    }
}
