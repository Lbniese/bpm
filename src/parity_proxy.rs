//! Cross-tool network parity proxy.
//!
//! When `bpm bench --profile-parity` is set, this module runs a counting
//! HTTP/1.1 reverse proxy on an ephemeral loopback port. The bench harness
//! writes a work-dir `.npmrc` pointing npm/pnpm/bpm at the proxy, which
//! forwards every request to `https://registry.npmjs.org` and records
//! `{method, path, status, bytes, start_ns, end_ns}` per request. From the
//! per-tool record log the harness computes a [`NetworkShape`] so the cold-path
//! network footprint of all three managers is comparable on the same footing.
//!
//! Design notes:
//! - The proxy runs on its own multi-thread tokio runtime on a dedicated OS
//!   thread (the harness is otherwise synchronous). [`ParityProxy::start`]
//!   returns once the listener is bound; [`ParityProxy::drop`] signals shutdown
//!   and joins the thread.
//! - The upstream fetch reuses `reqwest`'s async client (rustls), so TLS to the
//!   registry is handled and the proxy only speaks HTTP/1.1 to the tools. This
//!   normalizes transport and measures network *shape* (counts/bytes/concurrency),
//!   not production wall-clock; production timing stays in `reference.json`.
//! - Response bodies are streamed back through a [`CountingBody`] whose `Drop`
//!   records the finalized byte count and end timestamp, so large tarballs are
//!   never buffered in memory.
//! - `peak_concurrent` is computed from the recorded `(start, end)` intervals
//!   by [`compute_network_shape`], which is pure and unit-testable.

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use futures_util::TryStreamExt;
use http_body::{Body, Frame};
use http_body_util::{Empty, StreamBody};
use serde::{Deserialize, Serialize};

/// The counted response body, type-erased so the streaming success path and the
/// static error path share one `Response` body type. Counting itself happens in
/// [`CountingBody`]; this alias only erases the concrete upstream body type
/// (`StreamBody` vs `Empty`).
type BoxedCountingBody = Pin<Box<dyn Body<Data = Bytes, Error = std::convert::Infallible> + Send>>;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;

/// One forwarded HTTP request observed by the proxy. Timestamps are monotonic
/// nanoseconds relative to a process-local baseline (see [`mono_now_ns`]).
#[derive(Debug, Clone)]
pub struct NetRecord {
    pub method: String,
    pub path: String,
    pub status: u16,
    pub bytes: u64,
    pub start_ns: u128,
    pub end_ns: u128,
}

/// Aggregated network shape for one tool's run, derived from its [`NetRecord`]s.
/// Serialized onto each `ToolResults` when `--profile-parity` is set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkShape {
    pub request_count: u64,
    pub response_bytes: u64,
    /// Maximum number of requests simultaneously in flight (interval scan over
    /// the recorded start/end timestamps). `0` when there are no records.
    pub peak_concurrent: u64,
    pub p50_latency_ms: f64,
    pub p95_latency_ms: f64,
}

/// Pure aggregation of a tool's observed records into a [`NetworkShape`].
///
/// `peak_concurrent` is the max overlap of the `[start_ns, end_ns]` intervals,
/// computed with the standard sweep over sorted `(+1 at start, -1 at end)`
/// events. This is independent of any live atomic counter so it is directly
/// unit-testable with synthetic records.
pub fn compute_network_shape(records: &[NetRecord]) -> NetworkShape {
    if records.is_empty() {
        return NetworkShape::default();
    }

    let request_count = records.len() as u64;
    let response_bytes: u64 = records.iter().map(|record| record.bytes).sum();

    // Peak concurrency via a running sweep over sorted interval endpoints.
    // Ties break with end-events (-1) before start-events (+1) so two requests
    // that only touch at a point do not count as overlapping.
    let mut events: Vec<(u128, i64)> = Vec::with_capacity(records.len() * 2);
    for record in records {
        events.push((record.start_ns, 1));
        events.push((record.end_ns, -1));
    }
    events.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut running: i64 = 0;
    let mut peak: i64 = 0;
    for (_, delta) in events {
        running += delta;
        if running > peak {
            peak = running;
        }
    }

    let mut latencies: Vec<f64> = records
        .iter()
        .map(|record| (record.end_ns.saturating_sub(record.start_ns)) as f64 / 1_000_000.0)
        .collect();
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p50_latency_ms = percentile(&latencies, 0.50);
    let p95_latency_ms = percentile(&latencies, 0.95);

    NetworkShape {
        request_count,
        response_bytes,
        peak_concurrent: peak.max(0) as u64,
        p50_latency_ms,
        p95_latency_ms,
    }
}

/// Nearest-rank percentile of a pre-sorted, non-empty sample.
fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((sorted.len() as f64) * q).ceil() as usize;
    let index = rank.clamp(1, sorted.len()) - 1;
    sorted[index]
}

/// The `.npmrc` body that redirects a tool's registry to the proxy. Written
/// into each benchmark work dir so npm/pnpm/bpm all fetch through it.
pub fn parity_npmrc_content(addr: SocketAddr) -> String {
    format!("registry=http://{addr}\n")
}

// -----------------------------------------------------------------------
// Proxy process
// -----------------------------------------------------------------------

/// Shared upstream client + per-tool request log. Cloned per connection/task.
struct ProxyState {
    client: reqwest::Client,
    log: Arc<Mutex<Vec<NetRecord>>>,
}

/// Handle to a running parity proxy. Dropping it signals shutdown and joins the
/// proxy thread.
pub struct ParityProxy {
    addr: SocketAddr,
    log: Arc<Mutex<Vec<NetRecord>>>,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ParityProxy {
    /// Bind a proxy on an ephemeral loopback port and start serving. Returns
    /// once the listener is bound (so `addr()` is usable).
    pub fn start() -> io::Result<Self> {
        let log = Arc::new(Mutex::new(Vec::<NetRecord>::new()));
        let log_for_thread = log.clone();
        let (addr_tx, addr_rx) = std::sync::mpsc::channel::<io::Result<SocketAddr>>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let thread = std::thread::Builder::new()
            .name("bpm-parity-proxy".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = addr_tx.send(Err(io::Error::other(format!(
                            "proxy runtime build failed: {error}"
                        ))));
                        return;
                    }
                };
                runtime.block_on(proxy_main(addr_tx, log_for_thread, shutdown_rx));
            })?;

        let addr = addr_rx
            .recv()
            .map_err(|_| io::Error::other("parity proxy thread exited before binding"))??;

        Ok(Self {
            addr,
            log,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        })
    }

    /// The loopback address tools should be redirected to via a work-dir `.npmrc`.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Snapshot the accumulated records WITHOUT clearing (used for inspection).
    pub fn snapshot(&self) -> Vec<NetRecord> {
        self.log.lock().map(|log| log.clone()).unwrap_or_default()
    }

    /// Snapshot the accumulated records and clear the log. Called between tools
    /// so each tool's [`NetworkShape`] is attributed correctly.
    pub fn snapshot_and_clear(&self) -> Vec<NetRecord> {
        self.log
            .lock()
            .map(|mut log| {
                let records = log.clone();
                log.clear();
                records
            })
            .unwrap_or_default()
    }

    /// Drop the accumulated records without returning them.
    pub fn clear(&self) {
        if let Ok(mut log) = self.log.lock() {
            log.clear();
        }
    }
}

impl Drop for ParityProxy {
    fn drop(&mut self) {
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl fmt::Debug for ParityProxy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ParityProxy")
            .field("addr", &self.addr)
            .finish()
    }
}

/// Bind the listener, report the address, then accept connections until
/// shutdown. Runs on the proxy's dedicated runtime.
async fn proxy_main(
    addr_tx: std::sync::mpsc::Sender<io::Result<SocketAddr>>,
    log: Arc<Mutex<Vec<NetRecord>>>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(error) => {
            let _ = addr_tx.send(Err(error));
            return;
        }
    };
    let addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(error) => {
            let _ = addr_tx.send(Err(error));
            return;
        }
    };
    if addr_tx.send(Ok(addr)).is_err() {
        // Harness gave up; nothing to do.
        return;
    }

    let client = match reqwest::Client::builder()
        .pool_max_idle_per_host(64)
        .build()
    {
        Ok(client) => client,
        Err(_) => return,
    };
    let state = Arc::new(ProxyState { client, log });

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break };
                let io = TokioIo::new(stream);
                let state = state.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        let state = state.clone();
                        async move {
                            Ok::<_, std::convert::Infallible>(proxy_handle(request, state).await)
                        }
                    });
                    let _ = http1::Builder::new().serve_connection(io, service).await;
                });
            }
            _ = &mut shutdown => break,
        }
    }
}

/// Headers forwarded tool -> registry (everything but hop-by-hop / host).
fn forward_request_headers(headers: &http::HeaderMap) -> http::HeaderMap {
    forward_headers(headers, REQUEST_SKIP)
}

/// Headers forwarded registry -> tool.
fn forward_response_headers(headers: &http::HeaderMap) -> http::HeaderMap {
    forward_headers(headers, RESPONSE_SKIP)
}

const REQUEST_SKIP: &[&str] = &[
    "host",
    "connection",
    "proxy-connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "proxy-authorization",
    "proxy-authenticate",
];
const RESPONSE_SKIP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "transfer-encoding",
    "trailer",
    "upgrade",
];

fn forward_headers(headers: &http::HeaderMap, skip: &[&str]) -> http::HeaderMap {
    let mut out = http::HeaderMap::new();
    for (name, value) in headers.iter() {
        let name_str = name.as_str().to_ascii_lowercase();
        if skip.contains(&name_str.as_str()) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Handle one tool request: forward to the registry, stream the response back
/// through a [`CountingBody`] that records the finalized byte count + end time
/// on drop. Returns a boxed body so success (streaming) and error (static)
/// responses share one type.
async fn proxy_handle(
    request: Request<Incoming>,
    state: Arc<ProxyState>,
) -> Response<BoxedCountingBody> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let start_ns = mono_now_ns();

    let upstream_url = format!("https://registry.npmjs.org{path}");
    let forwarded = forward_request_headers(request.headers());

    let upstream_result = state
        .client
        .request(reqwest_method(&method), &upstream_url)
        .headers(forwarded)
        .send()
        .await;

    let (status, response_headers, body_stream) = match upstream_result {
        Ok(response) => {
            let status = response.status();
            let headers = forward_response_headers(response.headers());
            (status, headers, response.bytes_stream())
        }
        Err(_) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                start_ns,
                &method,
                &path,
                &state.log,
            )
        }
    };

    let body = CountingBody {
        inner: Box::pin(StreamBody::new(body_stream.map_ok(Frame::data))),
        bytes: 0,
        pending: PendingRecord {
            method: method.as_str().to_string(),
            path,
            status: status.as_u16(),
            start_ns,
        },
        log: state.log.clone(),
        finalized: AtomicBool::new(false),
    };

    let mut response = Response::new(Box::pin(body) as BoxedCountingBody);
    *response.status_mut() = status;
    merge_headers(response.headers_mut(), response_headers);
    response
}

fn reqwest_method(method: &Method) -> reqwest::Method {
    match *method {
        Method::GET => reqwest::Method::GET,
        Method::HEAD => reqwest::Method::HEAD,
        Method::POST => reqwest::Method::POST,
        Method::PUT => reqwest::Method::PUT,
        Method::DELETE => reqwest::Method::DELETE,
        _ => reqwest::Method::GET,
    }
}

/// Build a short static error response (still counted as one record on drop).
fn error_response(
    status: StatusCode,
    start_ns: u128,
    method: &Method,
    path: &str,
    log: &Arc<Mutex<Vec<NetRecord>>>,
) -> Response<BoxedCountingBody> {
    let body = CountingBody {
        inner: Box::pin(Empty::<Bytes>::new()),
        bytes: 0,
        pending: PendingRecord {
            method: method.as_str().to_string(),
            path: path.to_string(),
            status: status.as_u16(),
            start_ns,
        },
        log: log.clone(),
        finalized: AtomicBool::new(false),
    };
    let mut response = Response::new(Box::pin(body) as BoxedCountingBody);
    *response.status_mut() = status;
    response
}

fn merge_headers(target: &mut http::HeaderMap, source: http::HeaderMap) {
    for (name, value) in source {
        if let Some(name) = name {
            target.append(name, value);
        }
    }
}

/// A response body that counts streamed bytes and, on drop, pushes a finalized
/// [`NetRecord`] (with the end timestamp) into the shared log.
///
/// `S` is the upstream body: [`StreamBody`] on the success path, [`Empty`] on
/// error paths. Both implement [`Body`] with `Data = Bytes`; this wrapper
/// normalizes the error to [`Infallible`] (mapping an upstream error to
/// end-of-stream) so it can be type-erased into [`BoxedCountingBody`].
struct CountingBody<S: Body<Data = Bytes>> {
    inner: Pin<Box<S>>,
    bytes: u64,
    pending: PendingRecord,
    log: Arc<Mutex<Vec<NetRecord>>>,
    finalized: AtomicBool,
}

struct PendingRecord {
    method: String,
    path: String,
    status: u16,
    start_ns: u128,
}

impl<S: Body<Data = Bytes>> Body for CountingBody<S> {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        // CountingBody is Unpin (every field is, and Pin<Box<S>> is always
        // Unpin), so we can borrow the inner body directly.
        let this = self.get_mut();
        match this.inner.as_mut().poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.bytes += data.len() as u64;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            // Upstream error or natural end: stop. The finalized record is
            // written by `Drop` once hyper releases this body.
            Poll::Ready(Some(Err(_))) | Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S: Body<Data = Bytes>> Drop for CountingBody<S> {
    fn drop(&mut self) {
        if self.finalized.swap(true, Ordering::SeqCst) {
            return;
        }
        let record = NetRecord {
            method: self.pending.method.clone(),
            path: self.pending.path.clone(),
            status: self.pending.status,
            bytes: self.bytes,
            start_ns: self.pending.start_ns,
            end_ns: mono_now_ns(),
        };
        if let Ok(mut log) = self.log.lock() {
            log.push(record);
        }
    }
}

/// Monotonic nanoseconds since a process-local baseline. Stable within a run
/// so interval overlaps and per-request latencies are internally consistent.
fn mono_now_ns() -> u128 {
    static BASE: OnceLock<Instant> = OnceLock::new();
    let base = BASE.get_or_init(Instant::now);
    Instant::now().duration_since(*base).as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_records_yield_default_shape() {
        let shape = compute_network_shape(&[]);
        assert_eq!(shape.request_count, 0);
        assert_eq!(shape.peak_concurrent, 0);
        assert_eq!(shape.response_bytes, 0);
    }

    #[test]
    fn peak_concurrent_counts_overlapping_intervals() {
        // Three fully overlapping requests, then two sequential ones.
        let records = vec![
            NetRecord {
                method: "GET".into(),
                path: "/a".into(),
                status: 200,
                bytes: 10,
                start_ns: 0,
                end_ns: 100,
            },
            NetRecord {
                method: "GET".into(),
                path: "/b".into(),
                status: 200,
                bytes: 20,
                start_ns: 10,
                end_ns: 90,
            },
            NetRecord {
                method: "GET".into(),
                path: "/c".into(),
                status: 200,
                bytes: 30,
                start_ns: 20,
                end_ns: 80,
            },
            NetRecord {
                method: "GET".into(),
                path: "/d".into(),
                status: 200,
                bytes: 40,
                start_ns: 200,
                end_ns: 300,
            },
            NetRecord {
                method: "GET".into(),
                path: "/e".into(),
                status: 200,
                bytes: 50,
                start_ns: 400,
                end_ns: 500,
            },
        ];
        let shape = compute_network_shape(&records);
        assert_eq!(shape.request_count, 5);
        assert_eq!(shape.response_bytes, 150);
        assert_eq!(shape.peak_concurrent, 3, "first three overlap");
    }

    #[test]
    fn touching_intervals_do_not_count_as_overlapping() {
        // end == next start at the same timestamp: end sorts before start, so
        // the running count never exceeds 1.
        let records = vec![
            NetRecord {
                method: "GET".into(),
                path: "/a".into(),
                status: 200,
                bytes: 1,
                start_ns: 0,
                end_ns: 50,
            },
            NetRecord {
                method: "GET".into(),
                path: "/b".into(),
                status: 200,
                bytes: 1,
                start_ns: 50,
                end_ns: 100,
            },
        ];
        let shape = compute_network_shape(&records);
        assert_eq!(shape.peak_concurrent, 1);
    }

    #[test]
    fn latency_percentiles_are_monotone() {
        let records: Vec<NetRecord> = (0..20)
            .map(|i| NetRecord {
                method: "GET".into(),
                path: format!("/{i}"),
                status: 200,
                bytes: 1,
                start_ns: 0,
                end_ns: (i + 1) as u128,
            })
            .collect();
        let shape = compute_network_shape(&records);
        assert!(shape.p50_latency_ms <= shape.p95_latency_ms);
        assert!(shape.p50_latency_ms > 0.0);
    }

    #[test]
    fn network_shape_serializes_with_expected_fields() {
        let shape = compute_network_shape(&[NetRecord {
            method: "GET".into(),
            path: "/x".into(),
            status: 200,
            bytes: 7,
            start_ns: 0,
            end_ns: 2_000_000,
        }]);
        let json = serde_json::to_string(&shape).unwrap();
        for field in [
            "request_count",
            "response_bytes",
            "peak_concurrent",
            "p50_latency_ms",
            "p95_latency_ms",
        ] {
            assert!(
                json.contains(&format!("\"{field}\"")),
                "missing {field}: {json}"
            );
        }
    }

    /// Network-dependent accuracy check: every request the proxy forwards must
    /// be recorded exactly once, including sequential requests on one keep-alive
    /// connection and concurrent requests. Ignored by default (hits the live
    /// registry); run with `cargo test --lib -- --ignored parity_proxy_live`.
    #[test]
    #[ignore]
    fn parity_proxy_live_records_every_forwarded_request() {
        let proxy = ParityProxy::start().expect("start proxy");
        let base = format!("http://{}", proxy.addr());
        // Distinct registry paths (mix of packuments + a tarball) so the proxy
        // forwards each and records one NetRecord per request.
        let paths = [
            "/left-pad",
            "/left-pad/-/left-pad-1.3.0.tgz",
            "/is-number",
            "/is-odd",
            "/react",
        ];
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            // Sequential: exercises keep-alive reuse on one client/connection.
            let client = reqwest::Client::new();
            for path in paths {
                let resp = client.get(format!("{base}{path}")).send().await.unwrap();
                resp.bytes().await.unwrap();
            }
            // Concurrent: exercises the peak-concurrency accounting.
            let concurrent = ["/react-dom", "/webpack", "/typescript"];
            let futures = concurrent
                .iter()
                .map(|path| {
                    let client = client.clone();
                    let url = format!("{base}{path}");
                    async move {
                        let resp = client.get(&url).send().await.unwrap();
                        resp.bytes().await.unwrap();
                    }
                })
                .collect::<Vec<_>>();
            futures_util::future::join_all(futures).await;
        });

        let records = proxy.snapshot();
        let expected = paths.len() + 3;
        assert_eq!(
            records.len(),
            expected,
            "expected {expected} recorded requests, got {}: {:?}",
            records.len(),
            records
                .iter()
                .map(|record| record.path.clone())
                .collect::<Vec<_>>(),
        );
        // The tarball path must be among them (proves tarball bytes captured).
        assert!(records.iter().any(|record| record.path.ends_with(".tgz")));
        let shape = compute_network_shape(&records);
        assert!(shape.peak_concurrent >= 1);
        assert!(shape.response_bytes > 0);
    }
}
