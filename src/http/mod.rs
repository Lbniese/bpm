//! Shared blocking HTTP client for registry metadata and artifact streams.
//!
//! A client wraps one clonable [`reqwest::blocking::Client`], so cloned clients
//! share its connection pool and negotiate HTTP/2 over TLS (ALPN). Concurrent
//! requests from cloned clients — for example the download worker pool —
//! therefore multiplex over a single connection per host. Requests apply npmrc
//! authentication only to the exact host/path selected by [`NpmConfig`], mark
//! the credential sensitive so reqwest never forwards it across a cross-host
//! redirect, and retry only transient failures within configured bounds.

use std::fmt;
use std::io::{self, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};

use reqwest::blocking::{Client, ClientBuilder, Response};
use reqwest::header::RETRY_AFTER;
use reqwest::Version;

/// Best-effort record of whether any response arrived over HTTP/2. The cold
/// resolver depends on HTTP/2 multiplexing for throughput; if this stays false
/// the TLS backend is not negotiating ALPN and metadata fetches serialize.
static SAW_HTTP2: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

use crate::config::NpmConfig;

pub(crate) mod retry;
use retry::{
    is_retryable_status, is_retryable_transport, parse_retry_after_at, retry_delay, transport_kind,
    RETRY_BODY_DRAIN_LIMIT,
};

const USER_AGENT: &str = concat!("bpm/", env!("CARGO_PKG_VERSION"));

/// Maximum decoded bytes buffered from a control-plane response (registry
/// metadata, audit results, mutation responses). 64 MiB comfortably exceeds
/// the largest known npm packument while bounding memory if an endpoint serves
/// a malicious or broken oversized body. Artifact streams use a separate
/// larger streaming limit and are not routed through this cap.
pub(crate) const MAX_CONTROL_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// A pooled, configured HTTP client suitable for cloning between consumers.
///
/// Cloned clients share the same underlying [`reqwest::blocking::Client`] and
/// therefore the same connection pool and HTTP/2 stream concurrency.
#[derive(Clone)]
pub struct HttpClient {
    client: Client,
    config: NpmConfig,
    /// Cumulative count of outbound requests issued through this client. Held
    /// in an `Arc` so every clone (the registry client, the download worker
    /// pool, the prefetch workers) shares one counter, and the command-level
    /// metrics can read the true total once at the end.
    requests: Arc<AtomicU64>,
    /// Diagnostic gauges for resolver/download concurrency profiling:
    /// `in_flight` is the current number of requests awaiting a response, and
    /// `max_in_flight` is the peak observed across the client's lifetime. If
    /// the peak stays near 1 despite many prefetch workers, requests are
    /// serializing on the transport (e.g. HTTP/2 not negotiated).
    in_flight: Arc<AtomicU64>,
    max_in_flight: Arc<AtomicU64>,
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpClient")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl HttpClient {
    /// Build a client from effective npm configuration.
    ///
    /// The default redirect policy is retained (follow up to ten redirects),
    /// and any `Authorization` header set on a request is marked sensitive, so
    /// reqwest strips it on a cross-host redirect rather than leaking a
    /// registry credential to another origin.
    pub fn new(config: NpmConfig) -> Self {
        Self {
            client: build_client(config.network.fetch_timeout),
            config,
            requests: Arc::new(AtomicU64::new(0)),
            in_flight: Arc::new(AtomicU64::new(0)),
            max_in_flight: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Total number of outbound requests (GET/POST/PUT) issued through this
    /// client and every clone sharing its counter. One increment per logical
    /// request, before retries; retries are rare and irrelevant for resolver
    /// request-efficiency profiling.
    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// Peak number of requests that were awaiting a response at the same time,
    /// across this client's lifetime. A concurrency diagnostic: a value near 1
    /// despite many prefetch/download workers means requests are serializing on
    /// the transport rather than multiplexing.
    pub fn max_concurrent_requests(&self) -> u64 {
        self.max_in_flight.load(Ordering::Relaxed)
    }

    /// Whether any observed response used HTTP/2. False means the TLS backend
    /// did not negotiate ALPN and the client is on HTTP/1.1 (per-connection
    /// concurrency, no multiplexing).
    pub fn observed_http2(&self) -> bool {
        SAW_HTTP2.load(Ordering::Relaxed)
    }

    /// Record one request entering flight, returning a guard that decrements on
    /// drop and updates the peak-concurrency gauge. Cheap (relaxed atomics).
    fn track_in_flight(&self) -> InFlightGuard {
        let now = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        let mut current = self.max_in_flight.load(Ordering::Relaxed);
        while now > current {
            match self.max_in_flight.compare_exchange(
                current,
                now,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        InFlightGuard {
            counter: Arc::clone(&self.in_flight),
        }
    }

    /// Execute a GET request and return its response for string/JSON handling.
    pub fn get(&self, url: &str) -> Result<HttpResponse, HttpError> {
        self.get_with_headers(url, &[])
    }

    /// Execute a GET request with additional request headers.
    ///
    /// The body is read eagerly into [`HttpResponse`], which is appropriate for
    /// registry metadata (small JSON). Use [`HttpClient::stream`] for large
    /// bodies such as tarballs.
    pub fn get_with_headers(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse, HttpError> {
        let (mut response, _in_flight) = self.execute_get(url, headers)?;
        let status = response.status().as_u16();
        let collected = collect_headers(response.headers());
        check_content_length(&response, url, MAX_CONTROL_RESPONSE_BYTES)?;
        let body = read_bounded(&mut response, url, 1, MAX_CONTROL_RESPONSE_BYTES)?;
        Ok(HttpResponse {
            status,
            headers: collected,
            body,
        })
    }

    /// Execute a GET request and expose its body as a streaming reader.
    pub fn stream(&self, url: &str) -> Result<Box<dyn Read + Send + Sync + 'static>, HttpError> {
        let (response, in_flight) = self.execute_get(url, &[])?;
        Ok(Box::new(TrackedResponse {
            response,
            _in_flight: in_flight,
        }))
    }

    /// POST a JSON request body and return the response body as bytes.
    pub fn post_json(&self, url: &str, body: &[u8]) -> Result<Vec<u8>, HttpError> {
        self.request_json("POST", url, body, &[], true)
    }

    /// POST a JSON request with additional headers and return the response body as bytes.
    pub fn post_json_with_headers(
        &self,
        url: &str,
        body: &[u8],
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>, HttpError> {
        self.request_json("POST", url, body, headers, true)
    }

    /// POST a JSON request with additional headers, performing exactly one
    /// network attempt. Reserved for non-idempotent mutations (token creation)
    /// where a retry on a transient failure could duplicate a side effect the
    /// caller cannot observe. The response body is returned as bytes.
    pub fn post_json_with_headers_once(
        &self,
        url: &str,
        body: &[u8],
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>, HttpError> {
        self.request_json("POST", url, body, headers, false)
    }

    /// PUT a JSON request body and return the response body as bytes.
    pub fn put_json(&self, url: &str, body: &[u8]) -> Result<Vec<u8>, HttpError> {
        self.request_json("PUT", url, body, &[], true)
    }

    /// PUT a JSON request with additional headers and return the response body as bytes.
    pub fn put_json_with_headers(
        &self,
        url: &str,
        body: &[u8],
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>, HttpError> {
        self.request_json("PUT", url, body, headers, true)
    }

    /// Send a parameter-less DELETE request and return the response body.
    pub fn delete(&self, url: &str) -> Result<Vec<u8>, HttpError> {
        self.request_json("DELETE", url, &[], &[], true)
    }

    /// Send a parameter-less DELETE request with extra headers (e.g. `npm-otp`)
    /// and return the response body.
    pub fn delete_with_headers(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>, HttpError> {
        self.request_json("DELETE", url, &[], headers, true)
    }

    /// Send a GET (following redirects) honoring the retry policy.
    ///
    /// The returned [`Response`] is for any terminal status below 400
    /// (including `304 Not Modified`). Statuses at or above 400 are retried
    /// when transient and otherwise become [`HttpError::Status`].
    fn execute_get(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<(Response, InFlightGuard), HttpError> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let in_flight = self.track_in_flight();
        let display_url = redact_url(url);
        let network = &self.config.network;
        let attempts = network.retries.saturating_add(1);

        for attempt in 0..attempts {
            let request = self.build_get(url, headers);
            match request.send() {
                Ok(response) => {
                    if response.version() == Version::HTTP_2 {
                        SAW_HTTP2.store(true, Ordering::Relaxed);
                    }
                    let status = response.status().as_u16();
                    if status < 400 {
                        return Ok((response, in_flight));
                    }
                    let completed = attempt + 1;
                    if is_retryable_status(status) && completed < attempts {
                        let retry_after = retry_after_from(&response);
                        drain_response(response);
                        thread::sleep(retry_delay(network, attempt, retry_after));
                        continue;
                    }
                    return Err(HttpError::Status {
                        url: display_url,
                        code: status,
                        attempts: completed,
                    });
                }
                Err(error) => {
                    let completed = attempt + 1;
                    if is_retryable_transport(&error) && completed < attempts {
                        thread::sleep(retry_delay(network, attempt, None));
                        continue;
                    }
                    return Err(HttpError::Transport {
                        url: display_url,
                        kind: transport_kind(&error),
                        attempts: completed,
                    });
                }
            }
        }

        unreachable!("the configured attempt count is always at least one")
    }

    /// Build a GET request with npmrc auth and the caller's headers applied.
    fn build_get(&self, url: &str, headers: &[(&str, &str)]) -> reqwest::blocking::RequestBuilder {
        let mut request = self.client.get(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some(token) = self.config.auth_token_for_url(url) {
            request = request.bearer_auth(token);
        }
        request
    }

    fn request_json(
        &self,
        method: &str,
        url: &str,
        body: &[u8],
        headers: &[(&str, &str)],
        retry: bool,
    ) -> Result<Vec<u8>, HttpError> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let _in_flight = self.track_in_flight();
        let display_url = redact_url(url);
        let network = &self.config.network;
        // `retry = false` performs exactly one network attempt. Non-idempotent
        // mutations (e.g. token creation) opt out so a transient failure cannot
        // silently mint a second credential.
        let attempts = if retry {
            network.retries.saturating_add(1)
        } else {
            1
        };
        for attempt in 0..attempts {
            let request = match method {
                "POST" => self.client.post(url),
                "PUT" => self.client.put(url),
                "DELETE" => self.client.delete(url),
                _ => unreachable!(),
            }
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(body.to_vec());
            let mut request = if let Some(token) = self.config.auth_token_for_url(url) {
                request.bearer_auth(token)
            } else {
                request
            };
            for (name, value) in headers {
                request = request.header(*name, *value);
            }
            match request.send() {
                Ok(mut response) => {
                    let status = response.status().as_u16();
                    if status < 400 {
                        check_content_length(&response, url, MAX_CONTROL_RESPONSE_BYTES)?;
                        let body = read_bounded(
                            &mut response,
                            url,
                            attempt + 1,
                            MAX_CONTROL_RESPONSE_BYTES,
                        )?;
                        return Ok(body);
                    }
                    let completed = attempt + 1;
                    if is_retryable_status(status) && completed < attempts {
                        drain_response(response);
                        thread::sleep(retry_delay(network, attempt, None));
                        continue;
                    }
                    return Err(HttpError::Status {
                        url: display_url,
                        code: status,
                        attempts: completed,
                    });
                }
                Err(error) => {
                    let completed = attempt + 1;
                    if is_retryable_transport(&error) && completed < attempts {
                        thread::sleep(retry_delay(network, attempt, None));
                        continue;
                    }
                    return Err(HttpError::Transport {
                        url: display_url,
                        kind: transport_kind(&error),
                        attempts: completed,
                    });
                }
            }
        }
        unreachable!()
    }
}

/// A completed HTTP response owned by bpm, decoupled from the HTTP transport.
///
/// The body is read eagerly; headers are stored as owned strings so callers
/// never depend on `reqwest` types.
#[derive(Debug)]
pub struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    /// The response status code.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The first header value matching `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .rev()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Consume the response and return its body as a UTF-8 string.
    pub fn into_string(self) -> Result<String, std::string::FromUtf8Error> {
        String::from_utf8(self.body)
    }

    /// Consume the response and return its buffered body as an in-memory reader.
    pub fn into_reader(self) -> std::io::Cursor<Vec<u8>> {
        std::io::Cursor::new(self.body)
    }
}

/// A redacted, actionable HTTP failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    Status {
        url: String,
        code: u16,
        attempts: usize,
    },
    Transport {
        url: String,
        kind: String,
        attempts: usize,
    },
    /// A control-plane response body exceeded the configured decoded-byte
    /// limit. Carries only the redacted URL and the numeric limit — never
    /// response content, auth headers, query strings, or credentials.
    BodyTooLarge { url: String, limit: usize },
}

impl fmt::Display for HttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status {
                url,
                code,
                attempts,
            } => write!(
                formatter,
                "HTTP GET {url} returned status {code} after {attempts} attempt(s)"
            ),
            Self::Transport {
                url,
                kind,
                attempts,
            } => write!(
                formatter,
                "HTTP GET {url} failed with transport error {kind} after {attempts} attempt(s)"
            ),
            Self::BodyTooLarge { url, limit } => write!(
                formatter,
                "HTTP GET {url} response body exceeds the {limit} byte limit"
            ),
        }
    }
}

impl std::error::Error for HttpError {}

/// RAII guard that decrements the in-flight counter when dropped, so every
/// exit path of a request (success, error, retry) restores the gauge.
struct InFlightGuard {
    counter: Arc<AtomicU64>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Keeps HTTP in-flight tracking alive until a streaming response body has
/// been consumed or dropped. The artifact pipeline streams tarballs after the
/// response headers arrive, so tracking only `send()` undercounts concurrency.
struct TrackedResponse {
    response: Response,
    _in_flight: InFlightGuard,
}

impl Read for TrackedResponse {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.response.read(buffer)
    }
}

/// Build a pooled HTTP client.
///
/// HTTP/1.1 with a large idle connection pool is the default. The npm registry
/// (and registries behind CDNs like Fastly) commonly rate-limit per *connection*
/// rather than per stream. HTTP/2 multiplexes all streams over one connection,
/// so a single rate-limited link caps all concurrent requests. HTTP/1.1 with
/// `pool_max_idle_per_host(64)` lets each worker own its own connection,
/// achieving N-way concurrency for N workers.
///
/// HTTP/2 is enabled by default and negotiates via ALPN, allowing concurrent
/// artifact bodies to multiplex over one connection. Set `BPM_HTTP2=0` to
/// force the legacy HTTP/1.1 transport for compatibility diagnostics.
///
/// A static user agent and a valid timeout never produce an invalid builder in
/// practice, so a build failure falls back to the default client rather than
/// hard-failing configuration.
fn build_client(timeout: Duration) -> Client {
    let use_http2 = std::env::var("BPM_HTTP2")
        .ok()
        .and_then(|v| v.parse::<u8>().ok())
        .unwrap_or(1)
        != 0;

    let mut builder = ClientBuilder::new().user_agent(USER_AGENT).timeout(timeout);
    if use_http2 {
        builder = builder.pool_max_idle_per_host(64);
    } else {
        builder = builder.http1_only().pool_max_idle_per_host(64);
    }
    builder
        .build()
        .unwrap_or_else(|_| ClientBuilder::new().build().expect("default client builds"))
}

fn retry_after_from(response: &Response) -> Option<Duration> {
    response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_retry_after_at(value, SystemTime::now()))
}

/// Return [`HttpError::BodyTooLarge`] if the response declares a
/// Content-Length above the limit. The streamed count in [`read_bounded`]
/// remains authoritative; this is only an early rejection so a clearly
/// oversized body is never buffered.
fn check_content_length(response: &Response, url: &str, limit: usize) -> Result<(), HttpError> {
    if let Some(len) = response.content_length() {
        let limit_u64 = u64::try_from(limit).unwrap_or(u64::MAX);
        if len > limit_u64 {
            return Err(HttpError::BodyTooLarge {
                url: redact_url(url),
                limit,
            });
        }
    }
    Ok(())
}

/// Read at most `limit + 1` decoded bytes from `reader`. Returns the buffered
/// bytes when the body completes within `limit`; returns [`HttpError::BodyTooLarge`]
/// when it exceeds the limit. Reading one byte beyond the limit distinguishes
/// an exact-boundary body from an oversized one without allowing unbounded
/// work. A declared `Content-Length` is checked by the caller for an early
/// rejection, but this streamed count remains authoritative because the header
/// can be absent or false. Uses checked arithmetic throughout.
fn read_bounded(
    reader: &mut dyn Read,
    url: &str,
    attempts: usize,
    limit: usize,
) -> Result<Vec<u8>, HttpError> {
    let limit_u64 = u64::try_from(limit).unwrap_or(u64::MAX);
    // Cap the reader at limit + 1 so it stops as soon as the body is provably
    // oversized, never buffering more than limit + 1 bytes.
    let cap = limit_u64.saturating_add(1);
    let mut bounded = reader.take(cap);
    let mut out = Vec::new();
    let consumed = io::copy(&mut bounded, &mut out).map_err(|error| HttpError::Transport {
        url: redact_url(url),
        kind: format!("response read failed: {error}"),
        attempts,
    })?;
    if consumed > limit_u64 {
        return Err(HttpError::BodyTooLarge {
            url: redact_url(url),
            limit,
        });
    }
    Ok(out)
}

/// Drain a retryable-status response so its connection may return to the pool.
fn drain_response(response: Response) {
    let mut reader = response;
    let _ = drain_reader_for_retry(&mut reader);
}

/// Drain a retry response only while it remains small enough to pool safely.
///
/// Reading one byte beyond the limit distinguishes a complete 64 KiB body
/// from an oversized body without allowing unbounded work. Dropping an
/// oversized reader leaves bytes unread, causing the connection to close.
fn drain_reader_for_retry(reader: &mut dyn Read) -> io::Result<bool> {
    let limit = u64::try_from(RETRY_BODY_DRAIN_LIMIT).expect("drain limit fits in u64");
    let mut bounded = reader.take(limit + 1);
    let consumed = io::copy(&mut bounded, &mut io::sink())?;
    Ok(consumed <= limit)
}

/// Collect response headers into owned `(name, value)` pairs, skipping any
/// header whose value is not valid UTF-8.
fn collect_headers(map: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    map.iter()
        .filter_map(|(name, value)| {
            let value = value.to_str().ok()?;
            Some((name.as_str().to_owned(), value.to_owned()))
        })
        .collect()
}

/// Remove user information, query, and fragment from URLs included in errors.
/// Display-only URL redaction: strip userinfo, query, and fragment while
/// preserving scheme, host, port, and path for actionable diagnostics.
/// The original URL is **not** modified for requests, cache keys, or locks.
pub fn redact_url(url: &str) -> String {
    let without_suffix = url.split(['?', '#']).next().unwrap_or(url);
    let Some((scheme, remainder)) = without_suffix.split_once("://") else {
        return "<invalid-url>".to_string();
    };
    let authority_end = remainder.find('/').unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    let path = &remainder[authority_end..];
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    format!("{}://{}{}", scheme.to_ascii_lowercase(), host, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn retry_body_drain_is_bounded_and_detects_reusable_bodies() {
        for size in [0, RETRY_BODY_DRAIN_LIMIT - 1, RETRY_BODY_DRAIN_LIMIT] {
            let mut reader = Cursor::new(vec![0_u8; size]);
            assert!(drain_reader_for_retry(&mut reader).unwrap(), "size {size}");
            assert_eq!(reader.position(), size as u64);
        }

        let mut oversized = Cursor::new(vec![0_u8; RETRY_BODY_DRAIN_LIMIT + 100]);
        assert!(!drain_reader_for_retry(&mut oversized).unwrap());
        assert_eq!(oversized.position(), (RETRY_BODY_DRAIN_LIMIT + 1) as u64);
    }

    #[test]
    fn redacts_credentials_query_and_fragment() {
        assert_eq!(
            redact_url("https://user:secret@example.test/pkg?token=secret#private"),
            "https://example.test/pkg"
        );
        assert_eq!(redact_url("not a url"), "<invalid-url>");
    }

    #[test]
    fn redacts_query_only() {
        assert_eq!(
            redact_url("https://registry.example/pkg?abc=123"),
            "https://registry.example/pkg"
        );
    }

    #[test]
    fn redacts_fragment_only() {
        assert_eq!(
            redact_url("https://example.test/path#section"),
            "https://example.test/path"
        );
    }

    #[test]
    fn preserves_explicit_port() {
        assert_eq!(
            redact_url("https://registry.example:8443/pkg"),
            "https://registry.example:8443/pkg"
        );
    }

    #[test]
    fn handles_ipv6_authority() {
        assert_eq!(
            redact_url("https://[::1]:8080/path"),
            "https://[::1]:8080/path"
        );
    }

    #[test]
    fn handles_mixed_case_scheme() {
        assert_eq!(
            redact_url("HTTPS://user:pass@example.test/pkg"),
            "https://example.test/pkg"
        );
    }

    #[test]
    fn redacts_git_https_url() {
        assert_eq!(
            redact_url("git+https://token@github.com/user/repo.git#ref"),
            "git+https://github.com/user/repo.git"
        );
    }

    #[test]
    fn redacts_url_with_userinfo_only() {
        assert_eq!(
            redact_url("https://token@example.test/repo"),
            "https://example.test/repo"
        );
    }

    #[test]
    fn preserves_url_without_secrets() {
        assert_eq!(
            redact_url("https://registry.npmjs.org/lodash"),
            "https://registry.npmjs.org/lodash"
        );
    }

    #[test]
    fn handles_malformed_url() {
        assert_eq!(redact_url(""), "<invalid-url>");
        assert_eq!(redact_url("no-scheme"), "<invalid-url>");
        // "://" produces "://" since it has an empty scheme and authority.
        // This is harmless because no real URL has this shape.
        assert_eq!(redact_url("://"), "://");
    }

    #[test]
    fn http_response_header_lookup_is_case_insensitive() {
        let response = HttpResponse {
            status: 200,
            headers: vec![
                ("ETag".to_string(), "\"v1\"".to_string()),
                ("Content-Type".to_string(), "application/json".to_string()),
            ],
            body: b"{}".to_vec(),
        };
        assert_eq!(response.header("etag"), Some("\"v1\""));
        assert_eq!(response.header("ETAG"), Some("\"v1\""));
        assert_eq!(response.header("content-type"), Some("application/json"));
        assert_eq!(response.header("last-modified"), None);
        assert_eq!(response.into_string().unwrap(), "{}");
    }

    #[test]
    fn read_bounded_accepts_zero_below_exact_and_rejects_over() {
        // Empty body.
        let mut empty = Cursor::new(Vec::new());
        assert_eq!(
            read_bounded(&mut empty, "https://r.test/p", 1, 10).unwrap(),
            b""
        );
        // Below the limit.
        let mut small = Cursor::new(vec![b'a'; 5]);
        assert_eq!(
            read_bounded(&mut small, "https://r.test/p", 1, 10).unwrap(),
            vec![b'a'; 5]
        );
        // Exactly at the limit is allowed.
        let mut exact = Cursor::new(vec![b'a'; 10]);
        assert_eq!(
            read_bounded(&mut exact, "https://r.test/p", 1, 10).unwrap(),
            vec![b'a'; 10]
        );
    }

    #[test]
    fn read_bounded_rejects_one_byte_over_limit() {
        let mut over = Cursor::new(vec![b'a'; 11]);
        let err = read_bounded(&mut over, "https://r.test/p", 1, 10).unwrap_err();
        match err {
            HttpError::BodyTooLarge { url, limit } => {
                assert_eq!(url, "https://r.test/p");
                assert_eq!(limit, 10);
            }
            other => panic!("expected BodyTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn read_bounded_redacts_url_in_error_display() {
        let mut over = Cursor::new(vec![0u8; 5]);
        let err =
            read_bounded(&mut over, "https://user:secret@r.test/p?token=x", 1, 2).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("exceeds the 2 byte limit"), "{text}");
        // Credentials, query, and fragment must not appear.
        assert!(!text.contains("secret"), "userinfo leaked: {text}");
        assert!(!text.contains("token"), "query leaked: {text}");
        assert!(
            text.contains("https://r.test/p"),
            "redacted url missing: {text}"
        );
    }

    #[test]
    fn body_too_large_display_includes_limit_and_redacted_url() {
        let err = HttpError::BodyTooLarge {
            url: redact_url("https://r.test/pkg"),
            limit: 64 * 1024 * 1024,
        };
        let text = err.to_string();
        assert!(text.contains("response body exceeds"), "{text}");
        assert!(text.contains("67108864"), "limit value present: {text}");
        assert!(text.contains("https://r.test/pkg"), "{text}");
    }
}
