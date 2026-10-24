//! Opt-in, same-user JSON control over a Unix socket. One request per connection.
use std::io::{self, BufRead, BufReader, Read, Write};
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
        stream.set_read_timeout(Some(TIMEOUT + Duration::from_secs(1))).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(TIMEOUT)).map_err(|e| e.to_string())?;
        let mut bytes = serde_json::to_vec(&self.request).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_REQUEST {
            return Err("IPC request too large".into());
        }
        bytes.push(b'\n');
        stream.write_all(&bytes).map_err(|e| e.to_string())?;
        let reply = read_message(&mut stream, MAX_RESPONSE).map_err(|e| e.to_string())?;
        let value: Value = serde_json::from_slice(&reply).map_err(|e| e.to_string())?;
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
        // bind never removes an existing path, including a live server or symlink.
        let (wake, wake_read) = UnixStream::pair()?;
        let listener = UnixListener::bind(&path)?;
        let inode = std::fs::symlink_metadata(&path)?.ino();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let mut server = Self { path, inode, stop, wake, thread: None };
        listener.set_nonblocking(true)?;
        std::fs::set_permissions(&server.path, std::fs::Permissions::from_mode(0o600))?;
        let thread = std::thread::Builder::new().name("proteus-ipc".into()).spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                use rustix::event::{PollFd, PollFlags, poll};
                let mut fds = [PollFd::new(&listener, PollFlags::IN), PollFd::new(&wake_read, PollFlags::IN)];
                match poll(&mut fds, None) {
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(_) => break,
                    Ok(_) => {}
                }
                if !fds[1].revents().is_empty() || stopping.load(Ordering::Acquire) {
                    break;
                }
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(_) => break,
                };
                let allowed = rustix::net::sockopt::socket_peercred(&stream)
                    .is_ok_and(|cred| cred.uid == rustix::process::geteuid());
                if !allowed {
                    continue;
                }
                let _ = stream.set_read_timeout(Some(TIMEOUT));
                let _ = stream.set_write_timeout(Some(TIMEOUT));
                let result = (|| {
                    let bytes = read_message(&mut stream, MAX_REQUEST).map_err(|e| e.to_string())?;
                    let request: Request =
                        serde_json::from_slice(&bytes).map_err(|e| format!("invalid IPC request: {e}"))?;
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
                    Err(error) => serde_json::json!({"error": error}),
                };
                if let Ok(mut bytes) = serde_json::to_vec(&response) {
                    if bytes.len() > MAX_RESPONSE {
                        bytes = br#"{"error":"IPC response too large"}"#.to_vec();
                    }
                    bytes.push(b'\n');
                    let _ = stream.write_all(&bytes);
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

fn read_message(stream: &mut UnixStream, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    BufReader::new(stream).take((limit + 2) as u64).read_until(b'\n', &mut bytes)?;
    if bytes.last() != Some(&b'\n') || bytes.len() > limit + 1 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "IPC message too large or missing newline"));
    }
    bytes.pop();
    Ok(bytes)
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
        let value: Value = serde_json::from_slice(&read_message(&mut client, 1024).unwrap()).unwrap();
        assert_eq!(value["result"]["ok"], true);
        drop(client);
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_all(b"12345\n").unwrap();
        assert!(read_message(&mut b, 4).is_err());
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
}
