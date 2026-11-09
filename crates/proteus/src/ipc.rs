//! Opt-in, same-user JSON control over a Unix socket. One request per connection.
use std::io::{self, Read, Write};
use std::os::unix::{
    fs::{FileTypeExt, MetadataExt, PermissionsExt},
    net::{UnixListener, UnixStream},
};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_REQUEST: usize = 1 << 20;
const MAX_RESPONSE: usize = 16 << 20;
const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, clap::Subcommand, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    /// List terminal panes as JSON.
    List {},
    /// Send text to a pane; --paste uses the application's paste protocol.
    SendText {
        #[arg(long)]
        pane: Option<u64>,
        #[arg(long)]
        #[serde(default)]
        paste: bool,
        text: String,
    },
    /// Read screen text and optional history.
    Read {
        #[arg(long)]
        pane: Option<u64>,
        #[arg(long, default_value_t = 0)]
        #[serde(default)]
        scrollback: usize,
    },
    Focus {
        pane: u64,
    },
    /// Request closing a pane; normal close confirmation still applies.
    Close {
        pane: u64,
    },
    Action {
        #[arg(long)]
        pane: Option<u64>,
        id: String,
    },
    Split {
        #[arg(long)]
        pane: Option<u64>,
        /// x: side by side; y: stacked. Omit to choose the longer side.
        #[arg(long, value_parser = ["x", "y"])]
        axis: Option<String>,
    },
    NewTab {
        #[arg(long)]
        cwd: Option<String>,
        #[arg(last = true)]
        #[serde(default)]
        command: Vec<String>,
    },
}

impl Request {
    /// The `op` name, for the log (the request itself may hold user text).
    pub fn op(&self) -> &'static str {
        match self {
            Request::List {} => "list",
            Request::SendText { .. } => "send-text",
            Request::Read { .. } => "read",
            Request::Focus { .. } => "focus",
            Request::Close { .. } => "close",
            Request::Action { .. } => "action",
            Request::Split { .. } => "split",
            Request::NewTab { .. } => "new-tab",
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct Client {
    /// Socket path; defaults to PROTEUS_SOCKET in terminal children.
    #[arg(long)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    request: Request,
}

impl Client {
    pub fn run(self) -> Result<(), String> {
        let path = self
            .socket
            .or_else(|| std::env::var_os("PROTEUS_SOCKET").map(PathBuf::from))
            .ok_or("specify --socket or run inside a terminal with IPC enabled")?;
        let mut stream = UnixStream::connect(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut bytes = serde_json::to_vec(&self.request).map_err(|e| format!("encode IPC request: {e}"))?;
        if bytes.len() > MAX_REQUEST {
            return Err("IPC request too large".into());
        }
        bytes.push(b'\n');
        write_message(&mut stream, &bytes, Instant::now() + TIMEOUT)
            .map_err(|e| format!("send request to {}: {e}", path.display()))?;
        // The server may spend TIMEOUT waiting for the terminal and TIMEOUT writing.
        let deadline = Instant::now() + 2 * TIMEOUT + Duration::from_secs(1);
        let reply = read_message(&mut stream, MAX_RESPONSE, deadline)
            .map_err(|e| format!("read reply from {}: {e}", path.display()))?;
        let value: Value = serde_json::from_slice(&reply).map_err(|e| format!("invalid IPC reply: {e}"))?;
        if let Some(error) = value.get("error").and_then(Value::as_str) {
            return Err(error.into());
        }
        println!("{}", serde_json::to_string_pretty(&value["result"]).map_err(|e| e.to_string())?);
        Ok(())
    }
}

pub struct Call {
    pub request: Request,
    pub deadline: Instant,
    pub reply: mpsc::SyncSender<Result<Value, String>>,
}

pub struct Server {
    pub path: PathBuf,
    inode: u64,
    wake: UnixStream,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    pub fn start(path: PathBuf, dispatch: impl Fn(Call) -> bool + Send + 'static) -> io::Result<Self> {
        let context = |e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", path.display()));
        let (wake, wake_read) = UnixStream::pair()?;
        let listener = bind(&path).map_err(context)?;
        let inode = std::fs::symlink_metadata(&path).map_err(context)?.ino();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        listener.set_nonblocking(true)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(context)?;
        let mut server = Self { path, inode, stop, wake, thread: None };
        let thread = std::thread::Builder::new().name("proteus-ipc".into()).spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                use rustix::event::{PollFd, PollFlags, poll};
                let mut fds = [PollFd::new(&listener, PollFlags::IN), PollFd::new(&wake_read, PollFlags::IN)];
                match poll(&mut fds, None) {
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(e) => {
                        tracing::error!("control socket poll failed; no longer accepting requests: {e}");
                        break;
                    }
                    Ok(_) => {}
                }
                if !fds[1].revents().is_empty() || stopping.load(Ordering::Acquire) {
                    break;
                }
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(e) => {
                        tracing::error!("control socket accept failed; no longer accepting requests: {e}");
                        break;
                    }
                };
                let me = rustix::process::geteuid();
                match rustix::net::sockopt::socket_peercred(&stream) {
                    Ok(cred) if cred.uid == me => {
                        tracing::debug!(
                            uid = cred.uid.as_raw(),
                            pid = cred.pid.as_raw_nonzero().get(),
                            "ipc connection"
                        )
                    }
                    Ok(cred) => {
                        tracing::warn!(uid = cred.uid.as_raw(), "ipc connection rejected: peer is another user");
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!("ipc connection rejected: cannot read peer credentials: {e}");
                        continue;
                    }
                }
                let result = (|| {
                    let bytes = read_message(&mut stream, MAX_REQUEST, Instant::now() + TIMEOUT).map_err(|e| {
                        tracing::warn!("reading ipc request: {e}");
                        format!("reading IPC request: {e}")
                    })?;
                    let request: Request = serde_json::from_slice(&bytes).map_err(|e| {
                        // The message can quote request values, which may be user text.
                        tracing::warn!(
                            category = ?e.classify(),
                            line = e.line(),
                            column = e.column(),
                            bytes = bytes.len(),
                            "invalid ipc request"
                        );
                        format!("invalid IPC request: {e}")
                    })?;
                    tracing::debug!(op = request.op(), "ipc request");
                    if stopping.load(Ordering::Acquire) {
                        return Err("terminal is shutting down".into());
                    }
                    let (reply, rx) = mpsc::sync_channel(1);
                    let deadline = Instant::now() + TIMEOUT;
                    if !dispatch(Call { request, deadline, reply }) {
                        return Err("terminal is shutting down".into());
                    }
                    rx.recv_timeout(TIMEOUT).map_err(|_| "terminal did not respond before the deadline".to_string())?
                })();
                let response = match result {
                    Ok(value) => serde_json::json!({"result": value}),
                    Err(error) => {
                        tracing::debug!("ipc request failed: {error}");
                        serde_json::json!({"error": error})
                    }
                };
                if let Ok(mut bytes) = serde_json::to_vec(&response) {
                    if bytes.len() > MAX_RESPONSE {
                        tracing::warn!(bytes = bytes.len(), "ipc response too large");
                        bytes = br#"{"error":"IPC response too large"}"#.to_vec();
                    }
                    bytes.push(b'\n');
                    if let Err(e) = write_message(&mut stream, &bytes, Instant::now() + TIMEOUT) {
                        tracing::warn!("ipc reply not delivered: {e}");
                    }
                }
            }
        })?;
        server.thread = Some(thread);
        Ok(server)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // A private wake pipe still works if the socket path was removed or replaced.
        if let Some(thread) = self.thread.take() {
            let _ = self.wake.write_all(&[1]);
            let _ = thread.join();
        }
        if std::fs::symlink_metadata(&self.path).is_ok_and(|m| m.ino() == self.inode && m.file_type().is_socket()) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Never removes a live server's socket, a symlink, or another user's file;
/// only a socket nobody listens on (left by a crash) is replaced.
fn bind(path: &std::path::Path) -> io::Result<UnixListener> {
    match UnixListener::bind(path) {
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            let m = std::fs::symlink_metadata(path)?;
            let stale = m.file_type().is_socket()
                && m.uid() == rustix::process::geteuid().as_raw()
                && UnixStream::connect(path).is_err_and(|e| e.kind() == io::ErrorKind::ConnectionRefused);
            if !stale {
                return Err(e);
            }
            std::fs::remove_file(path)?;
            UnixListener::bind(path)
        }
        result => result,
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    Some(deadline.saturating_duration_since(Instant::now()))
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "IPC deadline exceeded"))
}

/// The deadline bounds the whole message, not each read.
fn read_message(stream: &mut UnixStream, limit: usize, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buf = [0; 8192];
    while bytes.len() <= limit + 1 && bytes.last() != Some(&b'\n') {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Err(io::ErrorKind::TimedOut.into()),
            Err(e) => return Err(e),
        };
        let chunk = &buf[..n];
        let end = chunk.iter().position(|&b| b == b'\n').map_or(n, |i| i + 1);
        bytes.extend_from_slice(&chunk[..end]);
    }
    if bytes.last() != Some(&b'\n') || bytes.len() > limit + 1 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "IPC message too large or missing newline"));
    }
    bytes.pop();
    Ok(bytes)
}

fn write_message(stream: &mut UnixStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        match stream.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Err(io::ErrorKind::TimedOut.into()),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn socket_protocol_is_bounded_private_and_preserves_existing_paths() {
        let path = std::env::temp_dir().join(format!("proteus-ipc-test-{}.sock", std::process::id()));
        let server =
            Server::start(path.clone(), |call| call.reply.send(Ok(serde_json::json!({"ok":true}))).is_ok()).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(Server::start(path.clone(), |_| true).is_err());
        let mut client = UnixStream::connect(&path).unwrap();
        client.set_read_timeout(Some(TIMEOUT)).unwrap();
        client.write_all(b"{\"op\":\"list\"}\n").unwrap();
        let value: Value =
            serde_json::from_slice(&read_message(&mut client, 1024, Instant::now() + TIMEOUT).unwrap()).unwrap();
        assert_eq!(value["result"]["ok"], true);
        drop(client);
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_all(b"12345\n").unwrap();
        assert!(read_message(&mut b, 4, Instant::now() + TIMEOUT).is_err());
        drop(server);
        assert!(!path.exists());
        assert!(serde_json::from_str::<Request>(r#"{"op":"list","extra":true}"#).is_err());
        let server = Server::start(path.clone(), |_| true).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        drop(server);
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn op_names_match_the_wire_tag() {
        let requests = [
            Request::List {},
            Request::SendText { pane: None, paste: false, text: "x".into() },
            Request::Read { pane: None, scrollback: 0 },
            Request::Focus { pane: 1 },
            Request::Close { pane: 1 },
            Request::Action { pane: None, id: "x".into() },
            Request::Split { pane: None, axis: None },
            Request::NewTab { cwd: None, command: Vec::new() },
        ];
        for r in requests {
            assert_eq!(serde_json::to_value(&r).unwrap()["op"], r.op());
        }
    }

    #[test]
    fn stale_sockets_are_replaced_and_slow_clients_hit_one_deadline() {
        let path = std::env::temp_dir().join(format!("proteus-ipc-stale-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        drop(UnixListener::bind(&path).unwrap());
        let server = Server::start(path.clone(), |_| true).unwrap();
        drop(server);
        assert!(!path.exists());
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let trickle = std::thread::spawn(move || {
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(50));
                if a.write_all(b"x").is_err() {
                    break;
                }
            }
        });
        let start = Instant::now();
        let err = read_message(&mut b, 1024, Instant::now() + Duration::from_millis(300)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_millis(600));
        drop(b);
        trickle.join().unwrap();
    }
}
