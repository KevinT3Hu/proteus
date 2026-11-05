//! HTTP(S) for plugins (permission `network`): only to the hosts the
//! manifest lists, no redirects followed, the system's certificates.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ureq::http;

#[path = "net_transport.rs"]
mod transport;

use crate::manifest::host_allowed;
use crate::runtime::Poster;
use crate::runtime::types::{Event, HttpHead, HttpRequest, HttpResponse};

pub const MAX_BODY: u64 = 64 << 20;
pub const MAX_STREAMS: usize = 16;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Check the URL against the allowed hosts.
pub fn check(allowed: &[String], url: &str) -> Result<http::Uri, String> {
    let uri: http::Uri = url.parse().map_err(|e| format!("{url}: {e}"))?;
    let port = match uri.scheme_str() {
        Some("https") => 443,
        Some("http") => 80,
        _ => return Err(format!("{url}: only http and https URLs")),
    };
    let host = uri.host().ok_or_else(|| format!("{url}: no host"))?;
    if uri.authority().is_some_and(|a| a.as_str().contains('@')) {
        return Err(format!("{url}: user names in URLs are not allowed"));
    }
    let port = uri.port_u16().unwrap_or(port);
    if !host_allowed(allowed, host, port) {
        return Err(format!("permission denied: network:{host}"));
    }
    Ok(uri)
}

fn agent(global: Option<Duration>, response: Option<Duration>) -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig};
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .allow_non_standard_methods(true)
        .timeout_global(global)
        .timeout_recv_response(response)
        .timeout_resolve(response)
        .timeout_connect(response)
        .timeout_send_request(response)
        .timeout_send_body(response)
        .tls_config(TlsConfig::builder().root_certs(RootCerts::PlatformVerifier).build())
        .user_agent(format!("Proteus/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent()
}

fn send(agent: &ureq::Agent, uri: http::Uri, req: HttpRequest) -> Result<http::Response<ureq::Body>, String> {
    let method = http::Method::from_bytes(req.method.to_ascii_uppercase().as_bytes()).map_err(|e| e.to_string())?;
    let mut b = http::Request::builder().method(method).uri(uri);
    for (k, v) in &req.headers {
        b = b.header(k, v);
    }
    let r = if req.body.is_empty() {
        agent.run(b.body(()).map_err(|e| e.to_string())?)
    } else {
        agent.run(b.body(req.body).map_err(|e| e.to_string())?)
    };
    r.map_err(|e| e.to_string())
}

fn headers(r: &http::Response<ureq::Body>) -> Vec<(String, String)> {
    r.headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect()
}

fn timeout(ms: u32) -> Duration {
    if ms == 0 { DEFAULT_TIMEOUT } else { Duration::from_millis(ms as u64) }
}

/// The whole response (blocking).
pub fn fetch(uri: http::Uri, req: HttpRequest) -> Result<HttpResponse, String> {
    let agent = guarded(agent(Some(timeout(req.timeout_ms)), None), &uri, Arc::default(), timeout(req.timeout_ms));
    let mut r = send(&agent, uri, req)?;
    let status = r.status().as_u16();
    let headers = headers(&r);
    let body = r.body_mut().with_config().limit(MAX_BODY).read_to_vec().map_err(|e| e.to_string())?;
    Ok(HttpResponse { status, headers, body })
}

/// `base` with the cancellable transport that also refuses loopback and
/// private addresses unless the URL names `localhost` or an IP address:
/// otherwise an allowed name could resolve to the user's local services.
fn guarded(base: ureq::Agent, uri: &http::Uri, cancel: Arc<AtomicBool>, idle: Duration) -> ureq::Agent {
    use ureq::unversioned::resolver::DefaultResolver;
    use ureq::unversioned::transport::{ConnectProxyConnector, Connector, RustlsConnector};
    let host = uri.host().unwrap_or_default().trim_start_matches('[').trim_end_matches(']');
    let allow_private = host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok();
    let config = base.config().clone();
    let connector =
        ().chain(ConnectProxyConnector::default()).chain(transport::StreamConnector { cancel, idle, allow_private });
    let connector = connector.chain(RustlsConnector::default());
    ureq::Agent::with_parts(config, connector, DefaultResolver::default())
}

/// Streaming requests of one plugin. Dropping it cancels them.
#[derive(Default)]
pub struct Streams {
    live: HashMap<u32, Arc<AtomicBool>>,
}

impl Streams {
    pub fn start(&mut self, id: u32, uri: http::Uri, req: HttpRequest, post: Poster) -> Result<(), String> {
        self.live.retain(|_, c| Arc::strong_count(c) > 1);
        if self.live.len() >= MAX_STREAMS {
            return Err(format!("at most {MAX_STREAMS} streams at a time"));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        if self.live.contains_key(&id) {
            return Err(format!("stream {id} is still running"));
        }
        self.live.insert(id, cancel.clone());
        std::thread::Builder::new()
            .name("plugin-http".into())
            .spawn(move || {
                let result = (|| {
                    let idle = timeout(req.timeout_ms);
                    let agent = guarded(agent(None, Some(idle)), &uri, cancel.clone(), idle);
                    let r = send(&agent, uri, req)?;
                    let head = HttpHead { id, status: r.status().as_u16(), headers: headers(&r) };
                    if !post.event(Event::HttpResponse(head)) {
                        return Ok(());
                    }
                    let mut body = r.into_body().into_reader();
                    let mut buf = vec![0u8; 16384];
                    loop {
                        if cancel.load(Ordering::Relaxed) {
                            return Err("cancelled".to_string());
                        }
                        match body.read(&mut buf) {
                            Ok(0) => return Ok(()),
                            Ok(n) => {
                                if !post.data_until(n, Event::HttpBody((id, buf[..n].to_vec())), &cancel) {
                                    return Ok(());
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(e) => return Err(e.to_string()),
                        }
                    }
                })();
                if !cancel.load(Ordering::Relaxed) {
                    post.event(Event::HttpDone((id, result.err())));
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn cancel(&mut self, id: u32) {
        // Keep cancelled workers counted until they actually release their
        // token. Repeated cancellation cannot bypass MAX_STREAMS.
        if let Some(c) = self.live.get(&id) {
            c.store(true, Ordering::Relaxed);
        }
    }
}

impl Drop for Streams {
    fn drop(&mut self) {
        for c in self.live.values() {
            c.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_checked() {
        let allowed = vec!["api.example.com".to_string(), "localhost:8080".into()];
        assert!(check(&allowed, "https://api.example.com/v1").is_ok());
        assert!(check(&allowed, "http://localhost:8080/x").is_ok());
        assert!(check(&allowed, "http://localhost/x").unwrap_err().contains("permission denied"));
        assert!(check(&allowed, "https://evil.com/").is_err());
        assert!(check(&allowed, "https://user@api.example.com/").is_err());
        assert!(check(&allowed, "ftp://api.example.com/").is_err());
        assert!(check(&allowed, "file:///etc/passwd").is_err());
    }

    #[test]
    fn names_may_not_lead_to_local_addresses() {
        use std::net::IpAddr;
        let local = [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ];
        for a in local {
            assert!(transport::local_address(a.parse::<IpAddr>().unwrap()), "{a}");
        }
        for a in ["93.184.216.34", "1.1.1.1", "2606:4700::1111", "172.32.0.1"] {
            assert!(!transport::local_address(a.parse::<IpAddr>().unwrap()), "{a}");
        }
    }

    #[test]
    fn cancelling_stalled_body_releases_worker_and_keeps_slot_until_exit() {
        use std::io::Write;
        use std::time::Instant;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0; 4096];
            let mut request = Vec::new();
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buf).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n").unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(3));
        });
        let url = format!("http://{addr}/");
        let req =
            HttpRequest { method: "GET".into(), url: url.clone(), headers: vec![], body: vec![], timeout_ms: 5_000 };
        let (post, events) = Poster::testing();
        let mut streams = Streams::default();
        streams.start(1, check(&[addr.to_string()], &url).unwrap(), req, post).unwrap();
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(1)).unwrap(),
            crate::runtime::ToPlugin::Event(Event::HttpResponse(_))
        ));
        // Ensure the worker is waiting for the body, not just checking its flag.
        std::thread::sleep(Duration::from_millis(20));
        streams.cancel(1);
        assert_eq!(streams.live.len(), 1, "a cancelled worker still consumes a slot until it exits");
        let end = Instant::now() + Duration::from_secs(1);
        while Arc::strong_count(streams.live.get(&1).unwrap()) > 1 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        let exited = Arc::strong_count(streams.live.get(&1).unwrap()) == 1;
        let _ = release.send(());
        server.join().unwrap();
        assert!(exited, "cancel must interrupt a body read independently of the server");
    }

    #[test]
    fn cancellation_interrupts_a_stalled_tls_handshake() {
        use std::time::Instant;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (accepted, wait_accept) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            accepted.send(()).unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(3));
            drop(socket);
        });
        let url = format!("https://{addr}/");
        let req =
            HttpRequest { method: "GET".into(), url: url.clone(), headers: vec![], body: vec![], timeout_ms: 5_000 };
        let (post, _events) = Poster::testing();
        let mut streams = Streams::default();
        streams.start(1, check(&[addr.to_string()], &url).unwrap(), req, post).unwrap();
        wait_accept.recv_timeout(Duration::from_secs(1)).unwrap();
        streams.cancel(1);
        let end = Instant::now() + Duration::from_secs(1);
        while Arc::strong_count(streams.live.get(&1).unwrap()) > 1 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        let exited = Arc::strong_count(streams.live.get(&1).unwrap()) == 1;
        let _ = release.send(());
        server.join().unwrap();
        assert!(exited, "cancellation must interrupt TLS I/O too");
    }

    #[test]
    fn stream_idle_timeout_is_reset_by_progress() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0; 4096];
            let mut request = Vec::new();
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buf).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\na").unwrap();
            for byte in [b"b", b"c"] {
                std::thread::sleep(Duration::from_millis(80));
                socket.write_all(byte).unwrap();
            }
        });
        let url = format!("http://{addr}/");
        let req =
            HttpRequest { method: "GET".into(), url: url.clone(), headers: vec![], body: vec![], timeout_ms: 140 };
        let (post, events) = Poster::testing();
        let mut streams = Streams::default();
        streams.start(1, check(&[addr.to_string()], &url).unwrap(), req, post).unwrap();
        let mut body = Vec::new();
        loop {
            match events.recv_timeout(Duration::from_secs(2)).unwrap() {
                crate::runtime::ToPlugin::Event(Event::HttpBody((_, data))) => body.extend(data),
                crate::runtime::ToPlugin::Event(Event::HttpDone((_, error))) => {
                    assert_eq!(error, None);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(body, b"abc");
        server.join().unwrap();
    }

    /// A request to a local server is answered; redirects are returned as is.
    #[test]
    fn local_request() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in listener.incoming().take(2) {
                let mut s = s.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Headers, then as much body as they announce.
                let req = loop {
                    let n = s.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_string())
                            })
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(0);
                        if body.len() >= len || n == 0 {
                            break text;
                        }
                    } else if n == 0 {
                        break text;
                    }
                };
                let reply = if req.starts_with("POST /echo") {
                    let body = req.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                    format!("HTTP/1.1 200 OK\r\nX-T: 1\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                } else {
                    "HTTP/1.1 302 Found\r\nLocation: http://evil.com/\r\nContent-Length: 0\r\n\r\n".into()
                };
                s.write_all(reply.as_bytes()).unwrap();
            }
        });
        let allowed = vec![format!("127.0.0.1:{port}")];
        let req = |method: &str, path: &str, body: &[u8]| HttpRequest {
            method: method.into(),
            url: format!("http://127.0.0.1:{port}{path}"),
            headers: vec![("content-type".into(), "text/plain".into())],
            body: body.to_vec(),
            timeout_ms: 5000,
        };
        let r = req("post", "/echo", b"hello");
        let resp = fetch(check(&allowed, &r.url).unwrap(), r).unwrap();
        assert_eq!((resp.status, &resp.body[..]), (200, &b"hello"[..]));
        assert!(resp.headers.iter().any(|(k, v)| k == "x-t" && v == "1"));
        let r = req("GET", "/moved", b"");
        let resp = fetch(check(&allowed, &r.url).unwrap(), r).unwrap();
        assert_eq!(resp.status, 302);
    }
}
