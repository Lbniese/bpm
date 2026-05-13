//! Shared test helpers: build in-memory tar.gz archives and run a tiny local
//! HTTP server, so download/store/concurrency tests need no network.
// Not every helper is used by every test file.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use bpm::integrity::Sha512Digest;

/// Build a gzip-compressed tar in memory. `build` appends entries to the builder.
pub fn build_tgz<F>(build: F) -> Vec<u8>
where
    F: FnOnce(&mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>),
{
    let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);
    build(&mut builder);
    let enc = builder.into_inner().expect("tar into_inner");
    enc.finish().expect("gzip finish")
}

/// Append a regular file `path` with `mode` and contents `data`.
pub fn add_file(
    b: &mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>,
    path: &str,
    mode: u32,
    data: &[u8],
) {
    let mut h = tar::Header::new_gnu();
    h.set_path(path).unwrap();
    h.set_size(data.len() as u64);
    h.set_mode(mode);
    h.set_cksum();
    b.append(&h, data).unwrap();
}

/// Append a directory entry.
pub fn add_dir(b: &mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>, path: &str, mode: u32) {
    let mut h = tar::Header::new_gnu();
    h.set_path(path).unwrap();
    h.set_entry_type(tar::EntryType::Directory);
    h.set_size(0);
    h.set_mode(mode);
    h.set_cksum();
    b.append(&h, &[][..]).unwrap();
}

/// Append a symlink entry: `path` -> `target`.
pub fn add_symlink(
    b: &mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>,
    path: &str,
    target: &str,
) {
    let mut h = tar::Header::new_gnu();
    h.set_path(path).unwrap();
    h.set_entry_type(tar::EntryType::Symlink);
    h.set_link_name(target).unwrap();
    h.set_size(0);
    h.set_mode(0o777);
    h.set_cksum();
    b.append(&h, &[][..]).unwrap();
}

/// npm-style integrity string for `bytes`.
pub fn integrity_of(bytes: &[u8]) -> String {
    Sha512Digest::hash_bytes(bytes).to_npm_string()
}

/// A response body plus its content type, returned by a [`MiniServer`] route.
pub struct RouteBody(pub Vec<u8>, pub &'static str);

/// One request observed by [`MiniServer`]. Header names are normalized to
/// lowercase and repeated header values retain their wire order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedRequest {
    pub sequence: usize,
    /// Stable, one-based identifier for the accepted TCP connection.
    pub connection_id: usize,
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, Vec<String>>,
}

impl CapturedRequest {
    /// Return the first value for `name`, matching the name case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .and_then(|values| values.first())
            .map(String::as_str)
    }
}

/// A deterministic HTTP failure served before the routed response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransientFailure {
    pub status: u16,
    pub retry_after: Option<String>,
}

impl TransientFailure {
    pub fn new(status: u16) -> Self {
        Self {
            status,
            retry_after: None,
        }
    }

    pub fn with_retry_after(mut self, value: impl Into<String>) -> Self {
        self.retry_after = Some(value.into());
        self
    }
}

/// A single-endpoint HTTP/1.1 server returning fixed bytes for any GET.
pub struct MiniServer {
    addr: String,
    hits: Arc<AtomicUsize>,
    connections: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    _handle: thread::JoinHandle<()>,
}

type Responder = Arc<dyn Fn(&str) -> Option<RouteBody> + Send + Sync>;

impl MiniServer {
    /// Start serving `body` for every request. Counts every accepted request.
    pub fn start(body: Vec<u8>) -> Self {
        let body = Arc::new(body);
        Self::start_routed(move |_path: &str| Some(RouteBody((*body).clone(), "application/gzip")))
    }

    /// Start serving requests via a path-dispatching responder. The responder
    /// receives the request path (e.g. `/lodash`) and returns the body +
    /// content type, or `None` for a 404. Used by registry tests to serve a
    /// packument on `/<name>` and the tarball on the tarball path.
    pub fn start_routed<F>(responder: F) -> Self
    where
        F: Fn(&str) -> Option<RouteBody> + Send + Sync + 'static,
    {
        Self::start_routed_with_failures(Vec::new(), responder)
    }

    /// Start a routed server that keeps HTTP/1.1 connections open. Use this
    /// variant when asserting that a pooled client sends multiple requests on
    /// one connection; [`MiniServer::connections`] and each request's
    /// `connection_id` provide the evidence.
    pub fn start_keep_alive_routed<F>(responder: F) -> Self
    where
        F: Fn(&str) -> Option<RouteBody> + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr").to_string();
        let hits = Arc::new(AtomicUsize::new(0));
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let responder: Responder = Arc::new(responder);
        let hits_for_thread = hits.clone();
        let connections_for_thread = connections.clone();
        let requests_for_thread = requests.clone();

        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let connection_id = connections_for_thread.fetch_add(1, Ordering::SeqCst) + 1;
                let responder = responder.clone();
                let hits = hits_for_thread.clone();
                let requests = requests_for_thread.clone();
                thread::spawn(move || {
                    serve_keep_alive(stream, connection_id, responder, hits, requests)
                });
            }
        });

        Self {
            addr,
            hits,
            connections,
            requests,
            _handle: handle,
        }
    }

    /// Start a routed server that returns each scripted failure once before
    /// dispatching subsequent requests to `responder`.
    pub fn start_routed_with_failures<F>(failures: Vec<TransientFailure>, responder: F) -> Self
    where
        F: Fn(&str) -> Option<RouteBody> + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr").to_string();
        let hits = Arc::new(AtomicUsize::new(0));
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let failures = Arc::new(failures);
        let responder: Responder = Arc::new(responder);
        let hits_for_thread = hits.clone();
        let connections_for_thread = connections.clone();
        let requests_for_thread = requests.clone();

        // Keep connections short so repeated fetches don't reuse a pooled one.
        let _ = listener.set_nonblocking(false);
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let connection_id = connections_for_thread.fetch_add(1, Ordering::SeqCst) + 1;
                let sequence = hits_for_thread.fetch_add(1, Ordering::SeqCst);
                let responder = responder.clone();
                let requests = requests_for_thread.clone();
                let failure = failures.get(sequence).cloned();
                thread::spawn(move || {
                    serve(
                        stream,
                        sequence,
                        connection_id,
                        responder,
                        failure,
                        requests,
                    )
                });
            }
        });

        Self {
            addr,
            hits,
            connections,
            requests,
            _handle: handle,
        }
    }

    pub fn url(&self, path: &str) -> String {
        let addr = &self.addr;
        format!("http://{addr}/{path}", path = path.trim_start_matches('/'))
    }

    pub fn url_for(&self) -> String {
        self.url("pkg.tgz")
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    /// Number of TCP connections accepted by the server.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// Return a snapshot ordered by server acceptance sequence.
    pub fn requests(&self) -> Vec<CapturedRequest> {
        let mut requests = self.requests.lock().expect("request capture lock").clone();
        requests.sort_by_key(|request| request.sequence);
        requests
    }
}

fn serve(
    mut stream: TcpStream,
    sequence: usize,
    connection_id: usize,
    responder: Responder,
    failure: Option<TransientFailure>,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let Some(request) = read_request(&mut stream, sequence, connection_id) else {
        return;
    };
    let path = request.path.clone();
    requests.lock().expect("request capture lock").push(request);

    if let Some(failure) = failure {
        write_failure(&mut stream, &failure);
        return;
    }

    match responder(&path) {
        Some(RouteBody(body, content_type)) => {
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(&body);
        }
        None => {
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    }
}

fn serve_keep_alive(
    mut stream: TcpStream,
    connection_id: usize,
    responder: Responder,
    hits: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    loop {
        let sequence = hits.load(Ordering::SeqCst);
        let Some(request) = read_request(&mut stream, sequence, connection_id) else {
            break;
        };
        let sequence = hits.fetch_add(1, Ordering::SeqCst);
        let mut request = request;
        request.sequence = sequence;
        let path = request.path.clone();
        requests.lock().expect("request capture lock").push(request);

        let Some(RouteBody(body, content_type)) = responder(&path) else {
            if stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n",
                )
                .is_err()
            {
                break;
            }
            continue;
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nConnection: keep-alive\r\n\r\n",
            body.len()
        );
        if stream.write_all(response.as_bytes()).is_err()
            || stream.write_all(&body).is_err()
            || stream.flush().is_err()
        {
            break;
        }
    }
}

const MAX_REQUEST_HEADERS: usize = 64 * 1024;

fn read_request(
    stream: &mut TcpStream,
    sequence: usize,
    connection_id: usize,
) -> Option<CapturedRequest> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    while bytes.len() < MAX_REQUEST_HEADERS {
        let Ok(read) = stream.read(&mut chunk) else {
            break;
        };
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.split("\r\n");
    let mut request_parts = lines.next().unwrap_or_default().split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_owned();
    let path = request_parts.next().unwrap_or("/").to_owned();
    let mut headers = BTreeMap::<String, Vec<String>>::new();
    for line in lines.take_while(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers
            .entry(name.trim().to_ascii_lowercase())
            .or_default()
            .push(value.trim().to_owned());
    }

    if bytes.is_empty() {
        return None;
    }

    Some(CapturedRequest {
        sequence,
        connection_id,
        method,
        path,
        headers,
    })
}

fn write_failure(stream: &mut TcpStream, failure: &TransientFailure) {
    let reason = match failure.status {
        408 => "Request Timeout",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Scripted Failure",
    };
    let retry_after = failure
        .retry_after
        .as_deref()
        .map(|value| format!("Retry-After: {value}\r\n"))
        .unwrap_or_default();
    let response = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Length: 0\r\n{retry_after}Connection: close\r\n\r\n",
        failure.status
    );
    let _ = stream.write_all(response.as_bytes());
}

/// One raw tar entry, written byte-for-byte (bypassing `tar::Builder`'s path
/// sanitizer) so hostile paths can be constructed for security tests.
pub struct RawEntry<'a> {
    pub name: &'a [u8],
    pub typeflag: u8,
    pub linkname: &'a [u8],
    pub mode: u32,
    pub data: &'a [u8],
}

fn write_octal(buf: &mut [u8], value: u64, width: usize) {
    let s = format!("{value:0width$o}");
    let body = s.as_bytes();
    // width+1 to leave room for a trailing NUL
    buf[..body.len()].copy_from_slice(body);
    buf[body.len()] = 0;
}

/// Compress a raw tar stream gzip-compressed from the given raw entries.
pub fn build_raw_tgz(entries: &[RawEntry<'_>]) -> Vec<u8> {
    let mut tar_bytes = Vec::new();
    for e in entries {
        let mut h = [0u8; 512];
        h[..e.name.len()].copy_from_slice(e.name);
        write_octal(&mut h[100..108], e.mode as u64, 7);
        write_octal(&mut h[108..116], 0, 7);
        write_octal(&mut h[116..124], 0, 7);
        write_octal(&mut h[124..136], e.data.len() as u64, 11);
        write_octal(&mut h[136..148], 0, 11);
        h[156] = e.typeflag;
        h[157..157 + e.linkname.len()].copy_from_slice(e.linkname);
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        // checksum with field as spaces
        for slot in &mut h[148..156] {
            *slot = b' ';
        }
        let sum: u32 = h.iter().map(|&b| b as u32).sum();
        let chk = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(chk.as_bytes());

        tar_bytes.extend_from_slice(&h);
        tar_bytes.extend_from_slice(e.data);
        let pad = (512 - (e.data.len() % 512)) % 512;
        tar_bytes.extend(std::iter::repeat_n(0u8, pad));
    }
    // two empty blocks terminate the archive
    tar_bytes.extend(std::iter::repeat_n(0u8, 1024));

    use std::io::Read;
    let mut enc = flate2::read::GzEncoder::new(
        std::io::Cursor::new(tar_bytes),
        flate2::Compression::default(),
    );
    let mut out = Vec::new();
    enc.read_to_end(&mut out).expect("gzip encode");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(server: &MiniServer, headers: &str) -> String {
        let mut stream = TcpStream::connect(&server.addr).expect("connect");
        write!(
            stream,
            "GET /package HTTP/1.1\r\nHost: test\r\n{headers}\r\n"
        )
        .expect("write request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read response");
        response
    }

    #[test]
    fn captures_normalized_and_repeated_headers() {
        let server = MiniServer::start_routed(|_| Some(RouteBody(Vec::new(), "text/plain")));

        let response = send(
            &server,
            "Authorization: Bearer secret\r\nX-Value: one\r\nX-Value: two\r\n",
        );

        assert!(response.starts_with("HTTP/1.1 200 OK"));
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].sequence, 0);
        assert_eq!(requests[0].connection_id, 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/package");
        assert_eq!(requests[0].header("AUTHORIZATION"), Some("Bearer secret"));
        assert_eq!(requests[0].headers["x-value"], ["one", "two"]);
    }

    #[test]
    fn consumes_scripted_failures_before_success() {
        let server = MiniServer::start_routed_with_failures(
            vec![
                TransientFailure::new(503).with_retry_after("0"),
                TransientFailure::new(429),
            ],
            |_| Some(RouteBody(b"ok".to_vec(), "text/plain")),
        );

        let first = send(&server, "");
        let second = send(&server, "");
        let third = send(&server, "");

        assert!(first.starts_with("HTTP/1.1 503 Service Unavailable"));
        assert!(first.contains("Retry-After: 0"));
        assert!(second.starts_with("HTTP/1.1 429 Too Many Requests"));
        assert!(third.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(server.hits(), 3);
        assert_eq!(
            server
                .requests()
                .into_iter()
                .map(|request| request.sequence)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(server.connections(), 3);
    }

    #[test]
    fn observes_keep_alive_connection_reuse() {
        use bpm::config::NpmConfig;
        use bpm::http::HttpClient;
        let server =
            MiniServer::start_keep_alive_routed(|_| Some(RouteBody(b"ok".to_vec(), "text/plain")));
        let client = HttpClient::new(NpmConfig::default());

        for path in ["one", "two"] {
            client
                .get(&server.url(path))
                .expect("request")
                .into_string()
                .expect("response body");
        }

        assert_eq!(server.hits(), 2);
        assert_eq!(server.connections(), 1);
        let requests = server.requests();
        assert_eq!(requests[0].connection_id, requests[1].connection_id);
        assert_eq!(
            requests
                .iter()
                .map(|request| request.path.as_str())
                .collect::<Vec<_>>(),
            ["/one", "/two"]
        );
    }
}
