//! HTTP(S) for plugins (permission `network`) and plugin downloads, as tasks
//! on the shared runtime. Plugin requests go only to the hosts the manifest
//! lists and follow no redirects. No proxy is used; every request has its
//! own connection and checks certificates against the system's.

use std::collections::HashMap;
use std::io::Write;
use std::net::IpAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use http::{Method, Request, Uri, header};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::task::{AbortHandle, JoinHandle};

use crate::manifest::host_allowed;
use crate::runtime::Poster;
use crate::runtime::types::{Event, HttpHead, HttpRequest, HttpResponse};

pub const MAX_BODY: usize = 64 << 20;
pub const MAX_STREAMS: usize = 16;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TIMEOUT: Duration = Duration::from_secs(300);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_REDIRECTS: usize = 10;
/// Compressed input decoded at a time, and decoded output handed on at a
/// time: what a small compressed body can make the terminal hold.
const GZIP_IN: usize = 1024;
const GZIP_OUT: usize = 64 << 10;

/// Check the URL against the allowed hosts.
pub fn check(allowed: &[String], url: &str) -> Result<Uri, String> {
    let uri: Uri = url.parse().map_err(|e| format!("{url}: {e}"))?;
    let default_port = match uri.scheme_str() {
        Some("https") => 443,
        Some("http") => 80,
        _ => return Err(format!("{url}: only http and https URLs")),
    };
    let host = uri.host().ok_or_else(|| format!("{url}: no host"))?;
    if uri.authority().is_some_and(|a| a.as_str().contains('@')) {
        return Err(format!("{url}: user names in URLs are not allowed"));
    }
    let port = uri.port_u16().unwrap_or(default_port);
    if !host_allowed(allowed, host, port, default_port) {
        tracing::debug!(host, port, "permission denied: network");
        return Err(format!("permission denied: network:{host}"));
    }
    Ok(uri)
}

/// Logged by host only: paths and queries can carry tokens.
fn log_failure(uri: &Uri, e: &str) {
    let mut e = e.to_string();
    if let Some(pq) = uri.path_and_query().map(|p| p.as_str()).filter(|p| p.len() > 1) {
        e = e.replace(pq, "/…");
    }
    tracing::warn!(host = uri.host().unwrap_or_default(), error = %e, "plugin http request failed");
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Loopback, private, link-local, shared, reserved, multicast and
/// unspecified addresses, including IPv4 addresses embedded in IPv6.
fn local_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || (a == 100 && (64..128).contains(&b))
                || (a == 198 && (18..20).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || a == 0
                || a >= 240
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            let embedded = |hi: u16, lo: u16| IpAddr::V4(std::net::Ipv4Addr::from(((hi as u32) << 16) | lo as u32));
            if let Some(v4) = v6.to_ipv4_mapped() {
                return local_address(IpAddr::V4(v4));
            }
            if s[..6] == [0; 6] || (s[0] == 0x64 && s[1] == 0xff9b) {
                return local_address(embedded(s[6], s[7]));
            }
            if s[0] == 0x2002 {
                return local_address(embedded(s[1], s[2]));
            }
            let first = s[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || (first & 0xffc0) == 0xfec0
        }
    }
}

/// The host without the brackets of an IPv6 address.
fn bare_host(uri: &Uri) -> &str {
    uri.host().unwrap_or_default().trim_start_matches('[').trim_end_matches(']')
}

/// The URL itself names `localhost` or an IP address: it may reach local
/// addresses. Otherwise an allowed name could resolve to the user's local
/// services.
fn names_address(uri: &Uri) -> bool {
    let host = bare_host(uri);
    host.eq_ignore_ascii_case("localhost") || host.parse::<IpAddr>().is_ok()
}

/// Headers that would route the request elsewhere than the checked URL,
/// authenticate to a proxy, or change the framing the client produces.
const RESERVED_HEADERS: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "content-length",
    "te",
    "trailer",
    "upgrade",
    "expect",
    "forwarded",
    "via",
];

fn header_allowed(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    !RESERVED_HEADERS.contains(&name.as_str()) && !name.starts_with("proxy-") && !name.starts_with(':')
}

fn timeout(ms: u32) -> Duration {
    if ms == 0 { DEFAULT_TIMEOUT } else { Duration::from_millis(ms as u64).min(MAX_TIMEOUT) }
}

/// `f`, or an error once `limit` passes.
async fn within<T>(limit: Duration, f: impl Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::time::timeout(limit, f).await.unwrap_or_else(|_| Err("network operation timed out".into()))
}

trait Connection: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Connection for T {}

fn tls() -> Result<tokio_rustls::TlsConnector, String> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        use rustls_platform_verifier::BuilderVerifierExt;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .and_then(|b| b.with_platform_verifier())
            .map_err(|e| format!("TLS: {e}"))?
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    });
    config.clone().map(tokio_rustls::TlsConnector::from)
}

/// A connection to `uri`'s host. Unless `any_address`, addresses on the
/// local machine or network are refused.
async fn connect(uri: &Uri, any_address: bool) -> Result<Box<dyn Connection>, String> {
    let https = uri.scheme_str() == Some("https");
    let host = bare_host(uri);
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
    let addrs = tokio::net::lookup_host((host, port)).await.map_err(|e| format!("{host}: {e}"))?;
    let mut last = None;
    let mut stream = None;
    for addr in addrs {
        if !any_address && local_address(addr.ip()) {
            last = Some(format!("{host} resolves to the local or private address {}", addr.ip()));
            continue;
        }
        match TcpStream::connect(addr).await {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = Some(format!("{host}: {e}")),
        }
    }
    let stream = stream.ok_or_else(|| last.unwrap_or_else(|| format!("{host}: no reachable address")))?;
    stream.set_nodelay(true).map_err(err)?;
    if !https {
        return Ok(Box::new(stream));
    }
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| format!("{host}: {e}"))?;
    let stream = tls()?.connect(name, stream).await.map_err(|e| format!("{host}: {e}"))?;
    Ok(Box::new(stream))
}

/// Aborts the task (the connection it drives) when dropped.
struct Abort(JoinHandle<()>);

impl Drop for Abort {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    location: Option<String>,
    body: Body,
}

/// A response body, decoded if we asked for gzip and got it.
struct Body {
    incoming: Incoming,
    gunzip: Option<flate2::write::GzDecoder<Vec<u8>>>,
    pending: Bytes,
    _connection: Abort,
}

impl Body {
    /// The next piece; `None` at the end.
    async fn next(&mut self) -> Result<Option<Vec<u8>>, String> {
        loop {
            match &mut self.gunzip {
                Some(g) => {
                    while !self.pending.is_empty() && g.get_ref().len() < GZIP_OUT {
                        let n = self.pending.len().min(GZIP_IN);
                        match g.write(&self.pending[..n]).map_err(|e| format!("gzip: {e}"))? {
                            // Past the end of the compressed data.
                            0 => self.pending = Bytes::new(),
                            used => self.pending = self.pending.slice(used..),
                        }
                    }
                    if !g.get_ref().is_empty() {
                        return Ok(Some(std::mem::take(g.get_mut())));
                    }
                }
                None if !self.pending.is_empty() => return Ok(Some(std::mem::take(&mut self.pending).to_vec())),
                None => {}
            }
            match self.incoming.frame().await {
                Some(frame) => {
                    if let Ok(data) = frame.map_err(err)?.into_data() {
                        self.pending = data;
                    }
                }
                None => {
                    let Some(g) = self.gunzip.take() else { return Ok(None) };
                    let rest = g.finish().map_err(|e| format!("gzip: {e}"))?;
                    return Ok((!rest.is_empty()).then_some(rest));
                }
            }
        }
    }

    async fn read_to_end(&mut self, max: usize) -> Result<Vec<u8>, String> {
        let mut all = Vec::new();
        while let Some(piece) = self.next().await? {
            if piece.len() > max - all.len() {
                return Err(format!("the response is larger than {} MiB", max >> 20));
            }
            all.extend_from_slice(&piece);
        }
        Ok(all)
    }
}

/// Send `req` to `uri` over a new connection.
async fn send(uri: Uri, req: HttpRequest, any_address: bool) -> Result<Reply, String> {
    let method = Method::from_bytes(req.method.to_ascii_uppercase().as_bytes()).map_err(err)?;
    let mut b = Request::builder().method(method).uri(uri.path_and_query().map_or("/", |p| p.as_str()));
    for (k, v) in &req.headers {
        if !header_allowed(k) {
            return Err(format!("header {k} is not allowed"));
        }
        b = b.header(k, v);
    }
    let host = uri.host().unwrap_or_default();
    b = b.header(header::HOST, uri.port().map_or_else(|| host.to_string(), |p| format!("{host}:{p}")));
    let named = |name: &str| req.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
    if !named("user-agent") {
        b = b.header(header::USER_AGENT, concat!("Proteus/", env!("CARGO_PKG_VERSION")));
    }
    let gzip = !named("accept-encoding");
    if gzip {
        b = b.header(header::ACCEPT_ENCODING, "gzip");
    }
    let request = b.body(Full::new(Bytes::from(req.body))).map_err(err)?;
    let io = connect(&uri, any_address).await?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await.map_err(err)?;
    let connection = Abort(proteus_tasks::spawn(async move {
        let _ = connection.await;
    })?);
    let (head, incoming) = sender.send_request(request).await.map_err(err)?.into_parts();
    let gzipped =
        gzip && head.headers.get(header::CONTENT_ENCODING).is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"gzip"));
    // A decoded body has neither the encoding nor the length sent.
    let headers = head
        .headers
        .iter()
        .filter(|(k, _)| !gzipped || (*k != header::CONTENT_ENCODING && *k != header::CONTENT_LENGTH))
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
    let location = head.headers.get(header::LOCATION).and_then(|v| v.to_str().ok()).map(str::to_string);
    let gunzip = gzipped.then(|| flate2::write::GzDecoder::new(Vec::new()));
    let body = Body { incoming, gunzip, pending: Bytes::new(), _connection: connection };
    Ok(Reply { status: head.status.as_u16(), headers, location, body })
}

/// The whole response. Dropping the future cancels the request.
pub async fn fetch(uri: Uri, req: HttpRequest) -> Result<HttpResponse, String> {
    let limit = timeout(req.timeout_ms);
    let checked = uri.clone();
    let any_address = names_address(&uri);
    let result = within(limit, async {
        let mut r = send(uri, req, any_address).await?;
        let body = r.body.read_to_end(MAX_BODY).await?;
        Ok(HttpResponse { status: r.status, headers: r.headers, body })
    })
    .await;
    if let Err(e) = &result {
        log_failure(&checked, e);
    }
    result
}

/// A plugin package the user configured at `url`: redirects are followed,
/// any address may be reached, and `https_only` holds for every hop.
/// Blocks; not for tasks.
pub fn download(url: &str, https_only: bool, max: usize) -> Result<Vec<u8>, String> {
    let mut uri: Uri = url.parse().map_err(|e| format!("{url}: {e}"))?;
    proteus_tasks::handle()?.block_on(within(DOWNLOAD_TIMEOUT, async move {
        for _ in 0..=MAX_REDIRECTS {
            match uri.scheme_str() {
                Some("https") => {}
                Some("http") if !https_only => {}
                _ => return Err(format!("{uri}: only https URLs")),
            }
            let get = HttpRequest {
                method: "GET".into(),
                url: uri.to_string(),
                headers: Vec::new(),
                body: Vec::new(),
                timeout_ms: 0,
            };
            let mut r = send(uri.clone(), get, true).await?;
            if (300..400).contains(&r.status)
                && let Some(location) = &r.location
            {
                uri = redirected(&uri, location)?;
                continue;
            }
            if !(200..300).contains(&r.status) {
                return Err(format!("http status {}", r.status));
            }
            return r.body.read_to_end(max).await;
        }
        Err(format!("more than {MAX_REDIRECTS} redirects"))
    }))
}

/// Where a redirect from `from` to `location` leads.
fn redirected(from: &Uri, location: &str) -> Result<Uri, String> {
    let to: Uri = location.parse().map_err(|e| format!("redirect to {location}: {e}"))?;
    if to.scheme().is_some() {
        return Ok(to);
    }
    let mut parts = to.into_parts();
    parts.scheme = from.scheme().cloned();
    parts.authority = from.authority().cloned();
    Uri::from_parts(parts).map_err(|e| format!("redirect to {location}: {e}"))
}

/// Streaming requests of one plugin. Dropping it cancels them.
#[derive(Default)]
pub struct Streams {
    live: HashMap<u32, AbortHandle>,
}

impl Streams {
    pub fn start(&mut self, id: u32, uri: Uri, req: HttpRequest, post: Poster) -> Result<(), String> {
        // A cancelled stream keeps its slot until its task has ended, so
        // repeated cancellation cannot bypass MAX_STREAMS.
        self.live.retain(|_, task| !task.is_finished());
        if self.live.len() >= MAX_STREAMS {
            return Err(format!("at most {MAX_STREAMS} streams at a time"));
        }
        if self.live.contains_key(&id) {
            return Err(format!("stream {id} is still running"));
        }
        let task = proteus_tasks::spawn(stream(id, uri, req, post))?;
        self.live.insert(id, task.abort_handle());
        Ok(())
    }

    /// No `http-done` follows.
    pub fn cancel(&mut self, id: u32) {
        if let Some(task) = self.live.get(&id) {
            task.abort();
        }
    }
}

impl Drop for Streams {
    fn drop(&mut self) {
        for task in self.live.values() {
            task.abort();
        }
    }
}

/// `timeout_ms` bounds each step (connecting, the response head, each
/// piece of the body), not the whole stream.
async fn stream(id: u32, uri: Uri, req: HttpRequest, post: Poster) {
    let idle = timeout(req.timeout_ms);
    let checked = uri.clone();
    let any_address = names_address(&uri);
    let result: Result<(), String> = async {
        let mut r = within(idle, send(uri, req, any_address)).await?;
        let head = HttpHead { id, status: r.status, headers: std::mem::take(&mut r.headers) };
        if !post.event_async(Event::HttpResponse(head)).await {
            return Ok(());
        }
        while let Some(data) = within(idle, r.body.next()).await? {
            if !post.data_async(data.len(), Event::HttpBody((id, data))).await {
                return Ok(());
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = &result {
        log_failure(&checked, e);
    }
    post.event_async(Event::HttpDone((id, result.err()))).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::time::Instant;

    fn block_on<T>(f: impl Future<Output = T>) -> T {
        proteus_tasks::handle().unwrap().block_on(f)
    }

    /// Wait up to a second for stream `id` to end.
    fn ended(streams: &Streams, id: u32) -> bool {
        let end = Instant::now() + Duration::from_secs(1);
        while !streams.live[&id].is_finished() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        streams.live[&id].is_finished()
    }

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
            "::10.0.0.1",
            "64:ff9b::7f00:1",
            "2002:a00:1::1",
            "fec0::1",
            "ff02::1",
            "198.18.0.1",
            "192.0.0.8",
            "224.0.0.1",
            "240.0.0.1",
        ];
        for a in local {
            assert!(local_address(a.parse::<IpAddr>().unwrap()), "{a}");
        }
        for a in ["93.184.216.34", "1.1.1.1", "2606:4700::1111", "172.32.0.1", "64:ff9b::101:101", "2002:101:101::1"] {
            assert!(!local_address(a.parse::<IpAddr>().unwrap()), "{a}");
        }
        assert!(names_address(&"http://[::1]:80/".parse().unwrap()));
        assert!(names_address(&"http://LOCALHOST/".parse().unwrap()));
        assert!(!names_address(&"http://local.example/".parse().unwrap()));
    }

    #[test]
    fn a_name_resolving_to_a_local_address_is_refused() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let uri: Uri = format!("http://localhost:{port}/").parse().unwrap();
        let get =
            HttpRequest { method: "GET".into(), url: uri.to_string(), headers: vec![], body: vec![], timeout_ms: 0 };
        let e = block_on(send(uri, get, false)).err().unwrap();
        assert!(e.contains("local or private address"), "{e}");
    }

    #[test]
    fn routing_and_framing_headers_are_reserved() {
        for h in ["Host", "proxy-authorization", "Transfer-Encoding", "connection", "Content-Length", "Forwarded"] {
            assert!(!header_allowed(h), "{h}");
        }
        for h in ["Authorization", "Accept", "content-type", "user-agent", "x-api-key"] {
            assert!(header_allowed(h), "{h}");
        }
    }

    #[test]
    fn cancelling_stalled_body_releases_worker_and_keeps_slot_until_exit() {
        use std::io::Write;
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
        std::thread::sleep(Duration::from_millis(20));
        streams.cancel(1);
        assert_eq!(streams.live.len(), 1, "a cancelled stream still takes a slot until it ends");
        let exited = ended(&streams, 1);
        let _ = release.send(());
        server.join().unwrap();
        assert!(exited, "cancel must interrupt a body read independently of the server");
        assert!(events.try_recv().is_err(), "no http-done after cancelling");
    }

    #[test]
    fn cancellation_interrupts_a_stalled_tls_handshake() {
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
        let exited = ended(&streams, 1);
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

    /// Answers each connection with `reply(request)`.
    fn serve(connections: usize, reply: impl Fn(&str) -> Vec<u8> + Send + 'static) -> u16 {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in listener.incoming().take(connections) {
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
                s.write_all(&reply(&req)).unwrap();
            }
        });
        port
    }

    /// A request to a local server is answered; redirects are returned as is.
    #[test]
    fn local_request() {
        let port = serve(2, |req| {
            if req.starts_with("POST /echo") {
                let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
                let host = req.lines().find(|l| l.to_ascii_lowercase().starts_with("host:")).unwrap_or_default();
                format!("HTTP/1.1 200 OK\r\nX-T: 1\r\nX-Host: {host}\r\nContent-Length: {}\r\n\r\n{body}", body.len())
            } else {
                "HTTP/1.1 302 Found\r\nLocation: http://evil.com/\r\nContent-Length: 0\r\n\r\n".into()
            }
            .into_bytes()
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
        let resp = block_on(fetch(check(&allowed, &r.url).unwrap(), r)).unwrap();
        assert_eq!((resp.status, &resp.body[..]), (200, &b"hello"[..]));
        assert!(resp.headers.iter().any(|(k, v)| k == "x-t" && v == "1"));
        assert!(resp.headers.iter().any(|(k, v)| k == "x-host" && v.ends_with(&format!("127.0.0.1:{port}"))));
        let r = req("GET", "/moved", b"");
        let resp = block_on(fetch(check(&allowed, &r.url).unwrap(), r)).unwrap();
        assert_eq!(resp.status, 302);
    }

    #[test]
    fn gzip_bodies_are_decoded_within_the_limit() {
        let plain = b"hello ".repeat(100_000);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        std::io::Write::write_all(&mut gz, &plain).unwrap();
        let gz = gz.finish().unwrap();
        let port = serve(1, move |req| {
            assert!(req.to_ascii_lowercase().contains("accept-encoding: gzip"));
            let mut r = format!("HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n", gz.len())
                .into_bytes();
            r.extend_from_slice(&gz);
            r
        });
        let url = format!("http://127.0.0.1:{port}/");
        let r = HttpRequest { method: "GET".into(), url: url.clone(), headers: vec![], body: vec![], timeout_ms: 5000 };
        let resp = block_on(fetch(check(&[format!("127.0.0.1:{port}")], &url).unwrap(), r)).unwrap();
        assert_eq!(resp.body, plain);
        assert!(!resp.headers.iter().any(|(k, _)| k == "content-encoding" || k == "content-length"));
    }

    #[test]
    fn downloads_follow_redirects_and_keep_to_https_when_asked() {
        let port = serve(2, |req| {
            if req.starts_with("GET /old") {
                b"HTTP/1.1 301 Moved\r\nLocation: /new\r\nContent-Length: 0\r\n\r\n".to_vec()
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nnew".to_vec()
            }
        });
        assert_eq!(download(&format!("http://127.0.0.1:{port}/old"), false, 10).unwrap(), b"new");
        assert!(download(&format!("http://127.0.0.1:{port}/old"), true, 10).unwrap_err().contains("only https"));
    }

    #[test]
    fn fetch_is_cancelled_by_dropping_it_and_timeouts_are_capped() {
        assert_eq!(timeout(u32::MAX), MAX_TIMEOUT);
        assert_eq!(timeout(0), DEFAULT_TIMEOUT);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(1500));
            drop(socket);
        });
        let url = format!("http://{addr}/");
        let req = HttpRequest { method: "GET".into(), url: url.clone(), headers: vec![], body: vec![], timeout_ms: 0 };
        let t = Instant::now();
        let fetching = fetch(check(&[addr.to_string()], &url).unwrap(), req);
        assert!(block_on(async { tokio::time::timeout(Duration::from_millis(100), fetching).await }).is_err());
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        server.join().unwrap();
    }
}
