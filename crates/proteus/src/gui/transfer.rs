//! Deadline and size limits for untrusted desktop selection pipes. These
//! run as tasks: they only wait for the descriptor.
use std::io;
use std::os::fd::OwnedFd;
use std::time::Duration;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

fn ready(fd: OwnedFd, interest: Interest) -> io::Result<AsyncFd<OwnedFd>> {
    rustix::io::ioctl_fionbio(&fd, true)?;
    // SAFETY: the `AsyncFd` owns `fd`, which stays open and the same until it is dropped.
    unsafe { AsyncFd::register_with_interest(fd, interest) }.map_err(|e| e.into_parts().1)
}

/// All of `fd` (at most `max` bytes) within `timeout`.
pub(super) async fn read(fd: OwnedFd, max: usize, timeout: Duration) -> io::Result<Vec<u8>> {
    let fd = ready(fd, Interest::READABLE)?;
    let read_all = async {
        let mut data = Vec::new();
        let mut buf = vec![0; 8192];
        loop {
            let mut guard = fd.readable().await?;
            match guard.try_io(|f| rustix::io::read(f.get_ref(), &mut buf).map_err(io::Error::from)) {
                Ok(Ok(0)) => return Ok(data),
                Ok(Ok(n)) if n <= max.saturating_sub(data.len()) => data.extend_from_slice(&buf[..n]),
                Ok(Ok(_)) => return Err(io::Error::new(io::ErrorKind::InvalidData, "selection exceeds size limit")),
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => {}
            }
        }
    };
    tokio::time::timeout(timeout, read_all).await.map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
}

/// Write all of `data` to `fd` within `timeout`.
pub(super) async fn write(fd: OwnedFd, mut data: &[u8], timeout: Duration) -> io::Result<()> {
    let fd = ready(fd, Interest::WRITABLE)?;
    let write_all = async {
        while !data.is_empty() {
            let mut guard = fd.writable().await?;
            match guard.try_io(|f| rustix::io::write(f.get_ref(), data).map_err(io::Error::from)) {
                Ok(Ok(0)) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(Ok(n)) => data = &data[n..],
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => {}
            }
        }
        Ok(())
    };
    tokio::time::timeout(timeout, write_all).await.map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Instant;

    fn run<T>(f: impl Future<Output = T>) -> T {
        proteus_tasks::handle().unwrap().block_on(f)
    }

    #[test]
    fn stalled_readers_time_out() {
        let (_reader, writer) = UnixStream::pair().unwrap();
        let start = Instant::now();
        let data = vec![0; 16 << 20];
        let e = run(write(writer.into(), &data, Duration::from_millis(50))).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
        let (mut reader, writer) = UnixStream::pair().unwrap();
        run(write(writer.into(), b"copied", Duration::from_secs(1))).unwrap();
        let mut got = Vec::new();
        reader.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"copied");
    }

    #[test]
    fn stalled_and_oversized_offers_fail_without_partial_paste() {
        let (reader, _writer) = UnixStream::pair().unwrap();
        let e = run(read(reader.into(), 10, Duration::from_millis(20))).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"12345").unwrap();
        let e = run(read(reader.into(), 4, Duration::from_secs(1))).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"safe").unwrap();
        drop(writer);
        assert_eq!(run(read(reader.into(), 4, Duration::from_secs(1))).unwrap(), b"safe");
    }
}
