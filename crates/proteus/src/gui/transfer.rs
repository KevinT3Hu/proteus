//! Deadline and size limits for untrusted desktop selection pipes.
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

pub(super) fn read(mut pipe: impl Read + AsFd, max: usize, timeout: Duration) -> io::Result<Vec<u8>> {
    rustix::io::ioctl_fionbio(pipe.as_fd(), true)?;
    let end = Instant::now() + timeout;
    let mut data = Vec::new();
    let mut buf = [0; 8192];
    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        match pipe.read(&mut buf) {
            Ok(0) => return Ok(data),
            Ok(n) if n <= max.saturating_sub(data.len()) => data.extend_from_slice(&buf[..n]),
            Ok(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "selection exceeds size limit")),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let mut fds = [rustix::event::PollFd::new(&pipe, rustix::event::PollFlags::IN)];
                let timeout = rustix::event::Timespec::try_from(remaining).map_err(io::Error::other)?;
                match rustix::event::poll(&mut fds, Some(&timeout)) {
                    Ok(_) | Err(rustix::io::Errno::INTR) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    #[test]
    fn stalled_and_oversized_offers_fail_without_partial_paste() {
        let (reader, _writer) = UnixStream::pair().unwrap();
        assert_eq!(read(reader, 10, Duration::from_millis(20)).unwrap_err().kind(), io::ErrorKind::TimedOut);
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"12345").unwrap();
        assert_eq!(read(reader, 4, Duration::from_secs(1)).unwrap_err().kind(), io::ErrorKind::InvalidData);
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"safe").unwrap();
        drop(writer);
        assert_eq!(read(reader, 4, Duration::from_secs(1)).unwrap(), b"safe");
    }
}
