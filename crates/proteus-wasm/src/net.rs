//! HTTP(S) for plugins (permission `network`): only to the hosts the
//! manifest lists, no redirects followed, the system's certificates.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ureq::http;

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
    let agent = agent(Some(timeout(req.timeout_ms)), None);
    let mut r = send(&agent, uri, req)?;
    let status = r.status().as_u16();
    let headers = headers(&r);
    let body = r.body_mut().with_config().limit(MAX_BODY).read_to_vec().map_err(|e| e.to_string())?;
    Ok(HttpResponse { status, headers, body })
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
        self.live.insert(id, cancel.clone());
        std::thread::Builder::new()
            .name("plugin-http".into())
            .spawn(move || {
                let result = (|| {
                    let agent = agent(None, Some(timeout(req.timeout_ms)));
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
                                if !post.data(n, Event::HttpBody((id, buf[..n].to_vec()))) {
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
        if let Some(c) = self.live.remove(&id) {
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
