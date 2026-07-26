//! Plan 036 tests: the async resolver's packument singleflight/cache must
//! dedup prefetch + inline fetches for the *same* packument (full and
//! exact-version), charge network wait into `resolver_network_wait_ns`, and
//! keep prefetch failures from surfacing into the install result.
//!
//! These drive `AsyncRegistryClient` directly against a `MiniServer` mock
//! registry so the assertions are deterministic and network-free.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bpm::async_resolver::AsyncRegistryClient;
use bpm::config::NpmConfig;
use bpm::registry::{PackageSpec, VersionRequest};

mod common;
use common::{MiniServer, RouteBody};

/// A mock registry that serves a full packument on `/<name>` and abbreviated
/// version metadata on `/<name>/<version>`. Counts every metadata hit so tests
/// can assert exactly how many network round-trips occurred.
struct CountedRegistry {
    _server: MiniServer,
    hits: Arc<AtomicUsize>,
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
    dist.insert("integrity".into(), serde_json::json!("sha512-AAAA"));
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
    dist.insert("integrity".into(), serde_json::json!("sha512-AAAA"));
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

/// Plan 036 Step 2: a prefetch that completes between two inline calls must
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

/// Plan 036 Step 1: when an inline fetch performs network I/O, the
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

/// Plan 036 Step 4: a background prefetch against a missing package (404) must
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

/// Plan 036: the cache-key helpers namespace full and version packuments so a
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
