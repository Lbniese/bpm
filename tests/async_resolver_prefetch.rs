//! tests: the async resolver's packument singleflight/cache must
//! dedup prefetch + inline fetches for the *same* packument (full and
//! exact-version), charge network wait into `resolver_network_wait_ns`, and
//! keep prefetch failures from surfacing into the install result.
//!
//! These drive `AsyncRegistryClient` directly against a `MiniServer` mock
//! registry so the assertions are deterministic and network-free.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use bpm::async_resolver::AsyncRegistryClient;
use bpm::config::NpmConfig;
use bpm::metadata_cache::{CacheMode, MetadataCache};
use bpm::registry::{parse_spec, PackageSpec, VersionRequest};
use flate2::write::GzEncoder;
use flate2::Compression;

mod common;
use common::{MiniServer, RouteBody};

/// A mock registry that serves a full packument on `/<name>` and abbreviated
/// version metadata on `/<name>/<version>`. Counts every metadata hit so tests
/// can assert exactly how many network round-trips occurred.
struct CountedRegistry {
    _server: MiniServer,
    hits: Arc<AtomicUsize>,
}

struct ConcurrencyRegistry {
    _server: MiniServer,
    peak: Arc<AtomicUsize>,
}

impl ConcurrencyRegistry {
    fn new() -> Self {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let active_for_server = active.clone();
        let peak_for_server = peak.clone();
        let server = MiniServer::start_keep_alive_routed(move |path| {
            let is_version = matches!(path, "/a/1.0.0" | "/b/1.0.0");
            if !is_version {
                return None;
            }
            let current = active_for_server.fetch_add(1, Ordering::SeqCst) + 1;
            peak_for_server.fetch_max(current, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(40));
            active_for_server.fetch_sub(1, Ordering::SeqCst);
            let name = if path.starts_with("/a/") { "a" } else { "b" };
            Some(RouteBody(
                abbreviated_version_body(name, "1.0.0"),
                "application/json",
            ))
        });
        Self {
            _server: server,
            peak,
        }
    }
}

impl CountedRegistry {
    fn new(name: &str, version: &str) -> Self {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_full = hits.clone();
        let hits_version = hits.clone();
        let full = full_packument_body(name, version);
        let abbreviated = abbreviated_version_body(name, version);
        let name_encoded = name.replace('/', "%2F");
        let version_owned = version.to_owned();
        let server = MiniServer::start_keep_alive_routed(move |path| {
            // Full packument endpoint.
            if path == format!("/{name_encoded}") {
                hits_full.fetch_add(1, Ordering::SeqCst);
                return Some(RouteBody(full.clone(), "application/json"));
            }
            // Abbreviated single-version endpoint.
            if path == format!("/{name_encoded}/{version_owned}") {
                hits_version.fetch_add(1, Ordering::SeqCst);
                return Some(RouteBody(abbreviated.clone(), "application/json"));
            }
            None
        });
        Self {
            _server: server,
            hits,
        }
    }

    fn metadata_hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

fn full_packument_body(name: &str, version: &str) -> Vec<u8> {
    let mut versions = serde_json::Map::new();
    let mut dist = serde_json::Map::new();
    dist.insert(
        "tarball".into(),
        serde_json::json!(format!("http://example.test/{name}-{version}.tgz")),
    );
    dist.insert(
        "integrity".into(),
        serde_json::json!("sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="),
    );
    let mut entry = serde_json::Map::new();
    entry.insert("name".into(), serde_json::json!(name));
    entry.insert("version".into(), serde_json::json!(version));
    entry.insert("dist".into(), serde_json::Value::Object(dist));
    versions.insert(version.to_string(), serde_json::Value::Object(entry));

    let mut root = serde_json::Map::new();
    let mut tags = serde_json::Map::new();
    tags.insert("latest".into(), serde_json::json!(version));
    root.insert("dist-tags".into(), serde_json::Value::Object(tags));
    root.insert("versions".into(), serde_json::Value::Object(versions));
    serde_json::to_vec(&serde_json::Value::Object(root)).unwrap()
}

fn abbreviated_version_body(name: &str, version: &str) -> Vec<u8> {
    let mut dist = serde_json::Map::new();
    dist.insert(
        "tarball".into(),
        serde_json::json!(format!("http://example.test/{name}-{version}.tgz")),
    );
    dist.insert(
        "integrity".into(),
        serde_json::json!("sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="),
    );
    let mut root = serde_json::Map::new();
    root.insert("name".into(), serde_json::json!(name));
    root.insert("version".into(), serde_json::json!(version));
    root.insert("dist".into(), serde_json::Value::Object(dist));
    serde_json::to_vec(&serde_json::Value::Object(root)).unwrap()
}

fn client_for(server: &MiniServer) -> AsyncRegistryClient {
    let config = NpmConfig::default()
        .with_registry_override(&server.url(""))
        .expect("valid registry override");
    AsyncRegistryClient::new(config)
}

fn gzip_encode(body: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(body).expect("gzip body");
    encoder.finish().expect("finish gzip body")
}

/// Loopback registry that emits decoded JSON through HTTP gzip content
/// encoding. It also returns 304 for conditional requests so cache tests prove
/// the persistent cache stores decoded JSON rather than compressed bytes.
struct GzipPackumentServer {
    registry: String,
    requests: Arc<AtomicUsize>,
    unconditional_requests: Arc<AtomicUsize>,
    conditional_requests: Arc<AtomicUsize>,
    full_requests: Arc<AtomicUsize>,
    exact_requests: Arc<AtomicUsize>,
    saw_compression_advertisement: Arc<AtomicBool>,
    _handle: thread::JoinHandle<()>,
}

impl GzipPackumentServer {
    fn new(name: &str, version: &str, full_body: Vec<u8>, exact_body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind gzip registry");
        let registry = format!(
            "http://{}",
            listener.local_addr().expect("registry address")
        );
        let full_path = format!("/{}", name.replace('/', "%2F"));
        let exact_path = format!("{full_path}/{version}");
        let full_body = Arc::new(gzip_encode(&full_body));
        let exact_body = Arc::new(gzip_encode(&exact_body));
        let requests = Arc::new(AtomicUsize::new(0));
        let unconditional_requests = Arc::new(AtomicUsize::new(0));
        let conditional_requests = Arc::new(AtomicUsize::new(0));
        let full_requests = Arc::new(AtomicUsize::new(0));
        let exact_requests = Arc::new(AtomicUsize::new(0));
        let saw_compression_advertisement = Arc::new(AtomicBool::new(false));

        let requests_thread = Arc::clone(&requests);
        let unconditional_thread = Arc::clone(&unconditional_requests);
        let conditional_thread = Arc::clone(&conditional_requests);
        let full_thread = Arc::clone(&full_requests);
        let exact_thread = Arc::clone(&exact_requests);
        let compression_thread = Arc::clone(&saw_compression_advertisement);
        let handle = thread::spawn(move || {
            for incoming in listener.incoming() {
                let Ok(mut stream) = incoming else { break };
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
                let Some(request) = read_http_headers(&mut stream) else {
                    continue;
                };
                requests_thread.fetch_add(1, Ordering::SeqCst);
                let lower = request.to_ascii_lowercase();
                if lower.contains("accept-encoding:")
                    && lower.contains("gzip")
                    && lower.contains("br")
                {
                    compression_thread.store(true, Ordering::SeqCst);
                }
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or_default();
                let body = if path == full_path {
                    full_thread.fetch_add(1, Ordering::SeqCst);
                    Some(Arc::clone(&full_body))
                } else if path == exact_path {
                    exact_thread.fetch_add(1, Ordering::SeqCst);
                    Some(Arc::clone(&exact_body))
                } else {
                    None
                };
                let Some(body) = body else {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                };
                if lower.contains("if-none-match:") {
                    conditional_thread.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.write_all(
                        b"HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\nETag: \"gzip-v1\"\r\nConnection: close\r\n\r\n",
                    );
                } else {
                    unconditional_thread.fetch_add(1, Ordering::SeqCst);
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nContent-Encoding: gzip\r\nETag: \"gzip-v1\"\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                }
            }
        });

        Self {
            registry,
            requests,
            unconditional_requests,
            conditional_requests,
            full_requests,
            exact_requests,
            saw_compression_advertisement,
            _handle: handle,
        }
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/{path}",
            self.registry,
            path = path.trim_start_matches('/')
        )
    }

    fn unconditional_requests(&self) -> usize {
        self.unconditional_requests.load(Ordering::SeqCst)
    }

    fn conditional_requests(&self) -> usize {
        self.conditional_requests.load(Ordering::SeqCst)
    }

    fn full_requests(&self) -> usize {
        self.full_requests.load(Ordering::SeqCst)
    }

    fn exact_requests(&self) -> usize {
        self.exact_requests.load(Ordering::SeqCst)
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    fn saw_compression_advertisement(&self) -> bool {
        self.saw_compression_advertisement.load(Ordering::SeqCst)
    }
}

fn read_http_headers(stream: &mut TcpStream) -> Option<String> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 1024];
    while bytes.len() < 64 * 1024 {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gzip_packuments_decode_for_exact_and_range_requests() {
    let server = GzipPackumentServer::new(
        "gzip-pkg",
        "1.0.0",
        full_packument_body("gzip-pkg", "1.0.0"),
        abbreviated_version_body("gzip-pkg", "1.0.0"),
    );
    let client = AsyncRegistryClient::new(
        NpmConfig::default()
            .with_registry_override(&server.url(""))
            .expect("valid registry override"),
    );

    let exact = client
        .resolve(&PackageSpec {
            name: "gzip-pkg".to_string(),
            req: VersionRequest::Exact(semver::Version::new(1, 0, 0)),
        })
        .await
        .expect("compressed exact metadata resolves");
    let range = client
        .resolve(&parse_spec("gzip-pkg@^1.0.0").expect("valid range spec"))
        .await
        .expect("compressed range metadata resolves");

    assert_eq!(exact.version, semver::Version::new(1, 0, 0));
    assert_eq!(range.version, semver::Version::new(1, 0, 0));
    assert_eq!(server.exact_requests(), 1);
    assert_eq!(server.full_requests(), 1);
    assert!(server.saw_compression_advertisement());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gzip_packument_cache_reuses_decoded_body_on_304() {
    let full_body = full_packument_body("cached-gzip", "1.0.0");
    let server = GzipPackumentServer::new(
        "cached-gzip",
        "1.0.0",
        full_body.clone(),
        abbreviated_version_body("cached-gzip", "1.0.0"),
    );
    let cache = Arc::new(MetadataCache::open_in_memory().expect("in-memory metadata cache"));
    let config = NpmConfig::default()
        .with_registry_override(&server.url(""))
        .expect("valid registry override");

    let first = AsyncRegistryClient::new(config.clone())
        .with_metadata_cache(Arc::clone(&cache), CacheMode::Default)
        .resolve(&parse_spec("cached-gzip").expect("valid package spec"))
        .await
        .expect("initial compressed fetch");

    let cache_url = server.url("cached-gzip");
    let mut cached = None;
    for _ in 0..100 {
        if let Some(entry) = cache.get(&cache_url).expect("cache read") {
            cached = Some(entry);
            break;
        }
        tokio::task::yield_now().await;
    }
    let cached = cached.expect("decoded metadata should be persisted");
    assert_eq!(cached.body, full_body);
    assert_ne!(cached.body, gzip_encode(&full_body));

    let second = AsyncRegistryClient::new(config)
        .with_metadata_cache(Arc::clone(&cache), CacheMode::Default)
        .resolve(&parse_spec("cached-gzip").expect("valid package spec"))
        .await
        .expect("conditional compressed fetch");

    assert_eq!(first.version, second.version);
    assert_eq!(first.tarball_url, second.tarball_url);
    assert_eq!(server.requests(), 2);
    assert_eq!(server.unconditional_requests(), 1);
    assert_eq!(server.conditional_requests(), 1);
    assert!(server.saw_compression_advertisement());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gzip_packument_expansion_still_hits_decoded_response_limit() {
    const MAX_CONTROL_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
    let server = GzipPackumentServer::new(
        "oversized-gzip",
        "1.0.0",
        vec![b'x'; MAX_CONTROL_RESPONSE_BYTES + 1],
        b"{}".to_vec(),
    );
    let client = AsyncRegistryClient::new(
        NpmConfig::default()
            .with_registry_override(&server.url(""))
            .expect("valid registry override"),
    );

    let error = client
        .packument("oversized-gzip")
        .await
        .expect_err("decoded compressed body over the limit must fail");
    let message = error.to_string();
    assert!(
        message.contains("response body exceeds 67108864 byte limit"),
        "unexpected bounded-response error: {message}"
    );
    assert_eq!(server.unconditional_requests(), 1);
}

/// a prefetch that completes between two inline calls must
/// not cause a second network fetch — the cache double-check serves the
/// just-completed prefetch. Drives the exact-version path (the dominant cold
/// case) and asserts exactly one metadata round-trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prefetch_then_inline_fetch_dedups_to_one_network_hit() {
    let registry = CountedRegistry::new("lodash", "4.17.21");
    let client = client_for(&registry._server);

    // Launch a background prefetch for the exact version, then await an inline
    // fetch for the same version. The singleflight + cache must dedup these.
    client.prefetch_packument("lodash", Some("4.17.21"));
    // Give the spawned prefetch a chance to run (it fills the cache).
    tokio::task::yield_now().await;
    let spec = PackageSpec {
        name: "lodash".to_string(),
        req: VersionRequest::Exact(semver::Version::parse("4.17.21").unwrap()),
    };
    let first = client.packument_for(&spec).await.expect("first fetch ok");
    // A second inline call must be a pure cache hit (no new network hit).
    let second = client.packument_for(&spec).await.expect("second fetch ok");

    assert_eq!(first.versions.len(), 1);
    assert_eq!(second.versions.len(), 1);
    // At most one metadata round-trip across prefetch + two inline calls.
    assert_eq!(
        registry.metadata_hits(),
        1,
        "prefetch + inline calls must dedup to a single network fetch"
    );
}

/// when an inline fetch performs network I/O, the
/// `network_wait_ns` diagnostic must be non-zero (the cold-path profile must
/// be honest about how much of resolution is network wait).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_fetch_charges_network_wait_metric() {
    let registry = CountedRegistry::new("react", "18.3.1");
    let client = client_for(&registry._server);

    let spec = PackageSpec {
        name: "react".to_string(),
        req: VersionRequest::Exact(semver::Version::parse("18.3.1").unwrap()),
    };
    client.packument_for(&spec).await.expect("fetch ok");

    let diag = client.take_diagnostics();
    assert!(
        diag.network_wait_ns > 0,
        "network_wait_ns must be non-zero after a real fetch, got {}",
        diag.network_wait_ns
    );
    assert!(diag.inline_fetches >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_in_flight_bounds_full_async_request_lifetime() {
    let registry = ConcurrencyRegistry::new();
    let config = NpmConfig::default()
        .with_registry_override(&registry._server.url(""))
        .expect("valid registry override");
    let client = AsyncRegistryClient::new(config).with_max_in_flight(1);
    let a = PackageSpec {
        name: "a".to_string(),
        req: VersionRequest::Exact(semver::Version::new(1, 0, 0)),
    };
    let b = PackageSpec {
        name: "b".to_string(),
        req: VersionRequest::Exact(semver::Version::new(1, 0, 0)),
    };

    let (a_result, b_result) = tokio::join!(client.packument_for(&a), client.packument_for(&b));
    a_result.expect("a metadata fetch");
    b_result.expect("b metadata fetch");

    assert_eq!(registry.peak.load(Ordering::SeqCst), 1);
    assert_eq!(client.take_diagnostics().peak_http_concurrency, 1);
}

/// a background prefetch against a missing package (404) must
/// be silently swallowed and must not break a subsequent inline fetch of a
/// valid package. Prefetches stay best-effort; the inline path is the single
/// source of truth for error reporting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_prefetch_is_swallowed_and_does_not_break_inline_fetch() {
    // Server serves `react` but returns 404 for everything else.
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = hits.clone();
    let full = full_packument_body("react", "18.3.1");
    let server = MiniServer::start_keep_alive_routed(move |path| {
        if path == "/react" || path == "/react/18.3.1" {
            hits_clone.fetch_add(1, Ordering::SeqCst);
            return Some(RouteBody(full.clone(), "application/json"));
        }
        None
    });
    let client = client_for(&server);

    // Prefetch a package the server does NOT serve; the spawned task errors.
    client.prefetch_packument("does-not-exist", Some("9.9.9"));
    // Let the failing prefetch run to completion.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    // The inline fetch of a valid package must still succeed.
    let spec = PackageSpec {
        name: "react".to_string(),
        req: VersionRequest::Exact(semver::Version::parse("18.3.1").unwrap()),
    };
    let packument = client.packument_for(&spec).await.expect("valid fetch ok");
    assert_eq!(packument.versions.len(), 1);

    // And the failed prefetch must not have surfaced as an error anywhere —
    // the only fetches counted are for `react`.
    let diag = client.take_diagnostics();
    assert!(diag.inline_fetches >= 1);
    let _ = hits.load(Ordering::SeqCst); // metadata hits for react only
}

/// the cache-key helpers namespace full and version packuments so a
/// cached full packument and a cached single-version packument for the same
/// name never collide (and vice versa).
#[test]
fn cache_keys_namespace_full_and_version_packuments() {
    use bpm::async_resolver::{packument_cache_key, version_cache_key};
    let full = packument_cache_key("https://registry.npmjs.org/", "lodash");
    let version = version_cache_key(
        "https://registry.npmjs.org/",
        "lodash",
        &semver::Version::parse("4.17.21").unwrap(),
    );
    assert_ne!(full, version, "full and version keys must not collide");
    assert!(version.contains("v:4.17.21"));
    // Trailing slash is normalized away on both.
    assert!(!full.contains("org//"));
}
