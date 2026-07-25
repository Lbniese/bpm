//! I/O abstraction for the placement core.
//!
//! [`PackumentSource`] is the single method a resolver needs from a registry
//! client: given a `PackageSpec`, return the full `Packument`.  The blocking
//! adapter wraps `RegistryClient`; the async adapter wraps `AsyncRegistryClient`
//! and drives async fetches to completion.
//!
//! Placement itself never touches a registry client directly — it calls
//! `PackumentSource` methods.  This makes the placement core I/O-agnostic.
//!
//! A [`CachedPackumentSource`] wrapper adds an in-memory LRU cache on top of
//! any [`PackumentSource`], avoiding repeated JSON parsing and network
//! round-trips when the same packument is requested multiple times during
//! graph expansion (Round A of Plan 018 — Cold-resolver optimization).

use std::num::NonZeroUsize;
use std::sync::Mutex;

use lru::LruCache;

use crate::http::HttpClient;
use crate::registry::{PackageSpec, Packument, RegistryClient, RegistryError};

/// Trait that provides packument data to the placement core.
///
/// The placement core is synchronous and deterministic: it reads packuments
/// from this source and never performs I/O itself.  A blocking implementation
/// delegates directly to `RegistryClient`; an async implementation must bridge
/// the sync↔async boundary (e.g. by pre-filling a cache or by driving async
/// fetches to completion in a way that does not re-enter a tokio runtime).
pub(crate) trait PackumentSource {
    /// Fetch the full packument for a given package+range spec.
    fn packument_for(&self, spec: &PackageSpec) -> Result<Packument, RegistryError>;

    /// Return the registry base URL for a package name.
    fn registry_for_package(&self, name: &str) -> &str;

    /// Best-effort hint that a packument will be needed soon.
    fn prefetch_packument(&self, name: &str, version_spec: Option<&str>);

    /// Optional HTTP client reference (needed for patch-resolution downloads).
    fn http(&self) -> Option<&HttpClient> {
        None
    }
}

// ── Blocking adapter ─────────────────────────────────────────────────────

/// Adapter that wraps `&RegistryClient` as a `PackumentSource`.
pub(crate) struct RegistrySource<'a> {
    pub(crate) client: &'a RegistryClient,
}

impl PackumentSource for RegistrySource<'_> {
    fn packument_for(&self, spec: &PackageSpec) -> Result<Packument, RegistryError> {
        self.client.packument_for(spec)
    }

    fn registry_for_package(&self, name: &str) -> &str {
        self.client.registry_for_package(name)
    }

    fn prefetch_packument(&self, name: &str, version_spec: Option<&str>) {
        self.client.prefetch_packument(name, version_spec);
    }

    fn http(&self) -> Option<&HttpClient> {
        Some(self.client.http())
    }
}

// ── Cached wrapper ────────────────────────────────────────────────────────

/// A `PackumentSource` wrapper that caches parsed packument results in an
/// LRU cache keyed by `{registry}\0{name}\0{request}`.
///
/// When the same packument is requested multiple times during graph expansion
/// — the same dependency in multiple branches, or the packument re-parsed
/// across placements — this avoids repeated JSON parsing (and for exact-
/// version requests that bypass `RegistryClient`'s own cache, repeated HTTP
/// round-trips).
///
/// Cache misses delegate to `inner`, and the result (success only) is stored
/// for the next request. Errors are never cached so transient failures are
/// retried. The default capacity of 256 entries covers the typical dependency
/// closure of most projects (< 500 unique packages in a large-frontend tree).
pub(crate) struct CachedPackumentSource<S> {
    inner: S,
    cache: Mutex<LruCache<String, Packument>>,
}

impl<S: PackumentSource> CachedPackumentSource<S> {
    pub fn new(inner: S) -> Self {
        Self::with_capacity(inner, 256)
    }

    pub fn with_capacity(inner: S, capacity: usize) -> Self {
        let capacity = NonZeroUsize::new(capacity.max(1)).expect("capacity >= 1");
        Self {
            inner,
            cache: Mutex::new(LruCache::new(capacity)),
        }
    }
}

impl<S: PackumentSource> PackumentSource for CachedPackumentSource<S> {
    fn packument_for(&self, spec: &PackageSpec) -> Result<Packument, RegistryError> {
        let registry = self.inner.registry_for_package(&spec.name);
        let key = format!(
            "{}\0{}\0{}",
            registry.trim_end_matches('/'),
            spec.name,
            version_request_to_key(&spec.req)
        );
        {
            let mut cache = self.cache.lock().unwrap();
            if let Some(packument) = cache.get(&key) {
                return Ok(packument.clone());
            }
        }
        let packument = self.inner.packument_for(spec)?;
        let mut cache = self.cache.lock().unwrap();
        cache.put(key, packument.clone());
        Ok(packument)
    }

    fn registry_for_package(&self, name: &str) -> &str {
        self.inner.registry_for_package(name)
    }

    fn prefetch_packument(&self, name: &str, version_spec: Option<&str>) {
        self.inner.prefetch_packument(name, version_spec);
    }

    fn http(&self) -> Option<&HttpClient> {
        self.inner.http()
    }
}

/// Convert a `VersionRequest` to a cache-key string segment.
fn version_request_to_key(req: &crate::registry::VersionRequest) -> String {
    match req {
        crate::registry::VersionRequest::Latest => "latest".to_string(),
        crate::registry::VersionRequest::Exact(version) => version.to_string(),
        crate::registry::VersionRequest::Range(range) => range.to_string(),
    }
}
