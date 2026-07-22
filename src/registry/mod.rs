//! Registry packument resolution: turn a package spec (`lodash`,
//! `lodash@4.17.21`, `@scope/pkg@^1.2.0`) into a concrete tarball URL and
//! integrity, the way `npm`/`bun` resolve a name before download.
//!
//! This is the small, self-contained end of dependency resolution. It does
//! *not* build a dependency graph — it resolves a single name to one tarball
//! and hands `(tarball_url, integrity)` to the existing immutable store, which
//! is unchanged.
//!
//! Behavior:
//! - `<name>`           -> `dist-tags.latest`
//! - `<name>@<version>` -> exact version (must exist in the packument)
//! - `<name>@<range>`   -> highest published version satisfying the range
//!   (`^`, `~`, `>=`, `x` ranges, `*`), via the `semver` crate
//!
//! Scoped names (`@scope/pkg`) are URL-encoded the way the npm registry
//! expects (`/` -> `%2F`) so the whole name is one path segment.

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;

use semver::{Version, VersionReq};
use serde::{Deserialize, Deserializer};
use thiserror::Error;

use crate::config::NpmConfig;
use crate::http::HttpClient;
use crate::integrity::Integrity;
use crate::metadata_cache::{CacheMode, MetadataCache};

/// The abbreviated install-metadata media type npm negotiates for graph
/// resolution. Requesting it avoids downloading each packument's full
/// publish-time history (multi-megabyte for popular packages).
pub(crate) const ABBREV_ACCEPT: &str = "application/vnd.npm.install-v1+json";

/// Per-client diagnostic accumulator shared by a `RegistryClient` and its
/// prefetch workers. All fields use saturating atomic adds so concurrent
/// increments from the resolver thread and workers never collide or overflow
/// silently.
#[derive(Debug)]
pub(crate) struct ResolverDiagnostics {
    /// Total packument fetch bytes across all threads.
    pub fetch_bytes: std::sync::atomic::AtomicU64,
    /// Cache hits in the in-memory packument cache.
    pub cache_hits: std::sync::atomic::AtomicU64,
    /// Cache waits (condvar waits on in-flight prefetches).
    pub cache_waits: std::sync::atomic::AtomicU64,
    /// Inline (synchronous) packument fetches.
    pub inline_fetches: std::sync::atomic::AtomicU64,
    /// Prefetch worker fetches.
    pub prefetch_fetches: std::sync::atomic::AtomicU64,
    /// Batch-prefetch closure fetches (one per packument in the BFS phase).
    pub batch_prefetch_fetches: std::sync::atomic::AtomicU64,
    /// Accumulated resolver fetch nanoseconds (thread-local, migrated to
    /// client-local accumulator updated by `packument_for`).
    pub resolver_fetch_nanos: std::sync::atomic::AtomicU64,
}

impl ResolverDiagnostics {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            fetch_bytes: std::sync::atomic::AtomicU64::new(0),
            cache_hits: std::sync::atomic::AtomicU64::new(0),
            cache_waits: std::sync::atomic::AtomicU64::new(0),
            inline_fetches: std::sync::atomic::AtomicU64::new(0),
            prefetch_fetches: std::sync::atomic::AtomicU64::new(0),
            batch_prefetch_fetches: std::sync::atomic::AtomicU64::new(0),
            resolver_fetch_nanos: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Snapshot and reset all counters in this accumulator.
    pub(crate) fn take(&self) -> ResolverDiagnosticsSnapshot {
        ResolverDiagnosticsSnapshot {
            fetch_bytes: self
                .fetch_bytes
                .swap(0, std::sync::atomic::Ordering::Relaxed),
            cache_hits: self
                .cache_hits
                .swap(0, std::sync::atomic::Ordering::Relaxed),
            cache_waits: self
                .cache_waits
                .swap(0, std::sync::atomic::Ordering::Relaxed),
            inline_fetches: self
                .inline_fetches
                .swap(0, std::sync::atomic::Ordering::Relaxed),
            prefetch_fetches: self
                .prefetch_fetches
                .swap(0, std::sync::atomic::Ordering::Relaxed),
            batch_prefetch_fetches: self
                .batch_prefetch_fetches
                .swap(0, std::sync::atomic::Ordering::Relaxed),
            resolver_fetch_nanos: self
                .resolver_fetch_nanos
                .swap(0, std::sync::atomic::Ordering::Relaxed),
        }
    }
}

/// Immutable snapshot of per-client resolver diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolverDiagnosticsSnapshot {
    pub fetch_bytes: u64,
    pub cache_hits: u64,
    pub cache_waits: u64,
    pub inline_fetches: u64,
    pub prefetch_fetches: u64,
    pub batch_prefetch_fetches: u64,
    pub resolver_fetch_nanos: u64,
}

// Thread-local resolver fetch nanoseconds accumulator used by the legacy
// public `take_resolver_fetch_nanos` bridge. New code uses
// `ResolverDiagnostics` passed through `RegistryClient`.
std::thread_local! {
    static RESOLVER_FETCH_NANOS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Read and reset the current thread's accumulated `packument_for` wall time,
/// in nanoseconds. Call from the thread that ran resolution to learn how much
/// of `dependency_resolution` was network wait versus CPU.
pub fn take_resolver_fetch_nanos() -> u64 {
    RESOLVER_FETCH_NANOS.with(|cell| {
        let value = cell.get();
        cell.set(0);
        value
    })
}

/// How a spec asks for a version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRange(Vec<VersionReq>);

impl VersionRange {
    pub fn parse(value: &str) -> Result<Self, semver::Error> {
        let mut ranges = Vec::new();
        for part in value.split("||") {
            let part = part.trim();
            if part.is_empty() {
                return Err(VersionReq::parse(part).unwrap_err());
            }
            ranges.push(VersionReq::parse(part)?);
        }
        Ok(Self(ranges))
    }

    pub fn matches(&self, version: &Version) -> bool {
        self.0.iter().any(|range| range.matches(version))
    }

    pub fn requirements(&self) -> &[VersionReq] {
        &self.0
    }
}

impl std::fmt::Display for VersionRange {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let joined = self
            .0
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" || ");
        formatter.write_str(&joined)
    }
}

#[derive(Debug, Clone)]
pub enum VersionRequest {
    /// No version given: use `dist-tags.latest`.
    Latest,
    /// An exact version (`lodash@4.17.21`).
    Exact(Version),
    /// A semver range (`lodash@^4.17.0`).
    Range(VersionRange),
}

/// A parsed package spec: a name plus a version request.
#[derive(Debug, Clone)]
pub struct PackageSpec {
    pub name: String,
    pub req: VersionRequest,
}

/// Deterministic package metadata returned by an npm packument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packument {
    pub name: String,
    pub dist_tags: BTreeMap<String, String>,
    pub versions: BTreeMap<String, VersionMetadata>,
}

/// Metadata needed to resolve and install one concrete package version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionMetadata {
    pub name: String,
    pub version: Version,
    pub deprecated: Option<String>,
    pub dependencies: BTreeMap<String, String>,
    pub optional_dependencies: BTreeMap<String, String>,
    pub peer_dependencies: BTreeMap<String, String>,
    pub peer_dependencies_meta: BTreeMap<String, PeerMeta>,
    pub bin: BTreeMap<String, String>,
    pub dist: Dist,
    pub engines: BTreeMap<String, String>,
    pub os: Vec<String>,
    pub cpu: Vec<String>,
    pub libc: Vec<String>,
    pub has_install_script: bool,
    pub has_shrinkwrap: bool,
}

/// Additional semantics for one peer dependency.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct PeerMeta {
    #[serde(default)]
    pub optional: bool,
}

/// Registry distribution data required by the immutable artifact store.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Dist {
    #[serde(default)]
    pub tarball: String,
    #[serde(default)]
    pub integrity: String,
    #[serde(default)]
    pub shasum: Option<String>,
}

#[derive(Deserialize)]
struct WirePackument {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "dist-tags")]
    dist_tags: BTreeMap<String, String>,
    #[serde(default)]
    versions: BTreeMap<String, WireVersionMetadata>,
}

#[derive(Default, Deserialize)]
pub(crate) struct WireVersionMetadata {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    deprecated: Option<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "peerDependencies")]
    peer_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "peerDependenciesMeta")]
    peer_dependencies_meta: BTreeMap<String, PeerMeta>,
    #[serde(default)]
    bin: WireBin,
    #[serde(default)]
    dist: Dist,
    #[serde(default)]
    engines: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    os: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    cpu: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    libc: Vec<String>,
    #[serde(default, rename = "hasInstallScript")]
    has_install_script: bool,
    #[serde(default, rename = "_hasShrinkwrap")]
    has_shrinkwrap: bool,
}

#[derive(Default, Deserialize)]
#[serde(untagged)]
enum WireBin {
    #[default]
    Missing,
    Single(String),
    Map(BTreeMap<String, String>),
}

impl<'de> Deserialize<'de> for Packument {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WirePackument::deserialize(deserializer)?;
        let package_name = wire.name;
        let mut versions = BTreeMap::new();
        for (version_key, metadata) in wire.versions {
            if let Some(metadata) = version_metadata(&package_name, &version_key, metadata) {
                versions.insert(version_key, metadata);
            }
        }
        Ok(Self {
            name: package_name,
            dist_tags: wire.dist_tags,
            versions,
        })
    }
}

fn deserialize_string_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringList {
        One(String),
        Many(Vec<String>),
    }

    Ok(match StringList::deserialize(deserializer)? {
        StringList::One(value) => vec![value],
        StringList::Many(values) => values,
    })
}

fn normalize_list(mut values: Vec<String>) -> Vec<String> {
    values.retain(|value| !value.is_empty());
    values.sort();
    values.dedup();
    values
}

pub(crate) fn version_metadata(
    package_name: &str,
    version_key: &str,
    metadata: WireVersionMetadata,
) -> Option<VersionMetadata> {
    let version_text = if metadata.version.is_empty() {
        version_key
    } else {
        metadata.version.as_str()
    };
    let version = Version::parse(version_text).ok()?;
    let name = if metadata.name.is_empty() {
        package_name.to_owned()
    } else {
        metadata.name
    };
    let mut bin = match metadata.bin {
        WireBin::Missing => BTreeMap::new(),
        WireBin::Single(path) => {
            BTreeMap::from([(name.rsplit('/').next().unwrap_or(&name).to_owned(), path)])
        }
        WireBin::Map(bin) => bin,
    };
    bin.retain(|command, path| !command.is_empty() && !path.is_empty());

    Some(VersionMetadata {
        name,
        version,
        deprecated: metadata.deprecated,
        dependencies: metadata.dependencies,
        optional_dependencies: metadata.optional_dependencies,
        peer_dependencies: metadata.peer_dependencies,
        peer_dependencies_meta: metadata.peer_dependencies_meta,
        bin,
        dist: metadata.dist,
        engines: metadata.engines,
        os: normalize_list(metadata.os),
        cpu: normalize_list(metadata.cpu),
        libc: normalize_list(metadata.libc),
        has_install_script: metadata.has_install_script,
        has_shrinkwrap: metadata.has_shrinkwrap,
    })
}

/// A fully resolved single package and all metadata needed by the resolver.
#[derive(Debug, Clone)]
pub struct ResolvedArtifact {
    pub name: String,
    pub version: Version,
    pub tarball_url: String,
    /// npm-style `sha512-<base64>` integrity from the registry `dist` block.
    pub integrity: String,
    pub metadata: VersionMetadata,
}

/// State of an in-process packument cache slot.
///
/// `Ready` holds an immutable packument shared across every placement that
/// needs the same package. `InFlight` marks a fetch (prefetched or inline)
/// already underway so concurrent callers deduplicate to a single network
/// request instead of racing a double fetch.
enum PackumentEntry {
    Ready(Packument),
    InFlight,
}

/// In-process packument memoization shared by the resolver and an optional
/// prefetch worker pool. The condvar wakes callers that arrived while a fetch
/// was `InFlight`.
struct PackumentCache {
    map: Mutex<BTreeMap<String, PackumentEntry>>,
    cv: Condvar,
}

impl PackumentCache {
    fn new() -> Self {
        PackumentCache {
            map: Mutex::new(BTreeMap::new()),
            cv: Condvar::new(),
        }
    }
}

/// Configured registry facade sharing one pooled HTTP client across requests.
#[derive(Clone)]
pub struct RegistryClient {
    config: NpmConfig,
    http: HttpClient,
    /// Packuments are immutable for the lifetime of one resolution. Sharing
    /// this small cache avoids fetching the same transitive package once per
    /// physical placement (common with peer and nested dependency graphs).
    /// Slots are either `Ready` or `InFlight` so concurrent prefetchers dedup.
    packument_cache: Arc<PackumentCache>,
    /// Optional persistent response cache shared across runs. When present,
    /// packument fetches revalidate over the network with conditional
    /// requests (`If-None-Match` / `If-Modified-Since`) and reuse the stored
    /// body verbatim on a `304`. `None` preserves the legacy uncached path.
    metadata_cache: Option<Arc<MetadataCache>>,
    cache_mode: CacheMode,
    /// Number of background prefetch worker threads. `0` disables prefetching
    /// entirely (the historical behavior); the resolver's trigger calls become
    /// cheap no-ops. When `> 0`, a lazily-started [`PrefetchPool`] overlaps
    /// sibling packument fetches during graph expansion over the shared
    /// HTTP/2 connection pool.
    prefetch_workers: usize,
    /// Lazily-started worker pool, shared across clones of this client.
    prefetch_pool: Arc<OnceLock<PrefetchPool>>,
    /// Per-client diagnostic accumulator shared with clones and workers.
    diagnostics: Arc<ResolverDiagnostics>,
}

impl std::fmt::Debug for RegistryClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RegistryClient")
            .field("config", &self.config)
            .field("cache_mode", &self.cache_mode)
            .field("persistent_cache", &self.metadata_cache.is_some())
            .finish_non_exhaustive()
    }
}

impl RegistryClient {
    /// Return the effective registry for a package name.
    pub fn registry_for_package(&self, package: &str) -> &str {
        self.config.registry_for_package(package)
    }

    /// Return the pooled HTTP client backing this registry facade.
    pub fn http(&self) -> &HttpClient {
        &self.http
    }

    /// Construct a registry facade with a newly allocated HTTP pool.
    ///
    /// This compatibility constructor is intended for callers that do not
    /// need to share the pool with artifact retrieval. Production pipelines
    /// should use [`Self::with_client`] with their caller-owned client.
    pub fn new(config: NpmConfig) -> Self {
        let http = HttpClient::new(config.clone());
        Self {
            config,
            http,
            packument_cache: Arc::new(PackumentCache::new()),
            metadata_cache: None,
            cache_mode: CacheMode::Default,
            prefetch_workers: 0,
            prefetch_pool: Arc::new(OnceLock::new()),
            diagnostics: ResolverDiagnostics::new(),
        }
    }

    /// Construct a registry facade using the caller's pooled HTTP client.
    ///
    /// Cloning `http` before passing it here lets metadata and artifact
    /// requests share the same underlying agent and connection pool.
    pub fn with_client(config: NpmConfig, http: HttpClient) -> Self {
        Self {
            config,
            http,
            packument_cache: Arc::new(PackumentCache::new()),
            metadata_cache: None,
            cache_mode: CacheMode::Default,
            prefetch_workers: 0,
            prefetch_pool: Arc::new(OnceLock::new()),
            diagnostics: ResolverDiagnostics::new(),
        }
    }

    /// Snapshot and reset all diagnostics accumulated by this client and its
    /// shared workers. Returns zeroes on a second call until new work occurs.
    pub fn take_diagnostics(&self) -> ResolverDiagnosticsSnapshot {
        self.diagnostics.take()
    }

    /// Enable background packument prefetching with `workers` threads.
    ///
    /// The pool starts lazily on the first prefetch request and is shared
    /// across clones of this client. Passing `0` (the default) disables
    /// prefetching, preserving the historical sequential fetch behavior.
    pub fn with_prefetch(mut self, workers: usize) -> Self {
        self.prefetch_workers = workers;
        self
    }

    /// Attach a persistent packument cache and select how it may be reused.
    ///
    /// When `cache_mode` forbids network access ([`CacheMode::Offline`]) the
    /// client resolves only against bodies already present in `cache`.
    pub fn with_metadata_cache(mut self, cache: Arc<MetadataCache>, cache_mode: CacheMode) -> Self {
        self.metadata_cache = Some(cache);
        self.cache_mode = cache_mode;
        self
    }

    /// The active cache reuse policy (`Default` when no cache is attached).
    pub fn cache_mode(&self) -> CacheMode {
        self.cache_mode
    }

    /// Fetch the configured scoped/default registry and resolve one package.
    pub fn resolve(&self, spec: &PackageSpec) -> Result<ResolvedArtifact, RegistryError> {
        let registry = self.config.registry_for_package(&spec.name);
        let packument = self.packument_for(spec)?;
        resolve_packument(spec, &packument, registry)
    }

    /// Fetch the smallest metadata document that can resolve `spec`.
    ///
    /// Exact versions have a dedicated registry endpoint and do not need the
    /// package's complete multi-megabyte version history. Ranges and tags use
    /// the abbreviated install metadata packument so dependency fields remain
    /// available without downloading npm's publish-time metadata.
    pub fn packument_for(&self, spec: &PackageSpec) -> Result<Packument, RegistryError> {
        let started = std::time::Instant::now();
        let outcome = match &spec.req {
            VersionRequest::Exact(version) => {
                let registry = self.config.registry_for_package(&spec.name);
                fetch_version_packument(
                    &self.http,
                    &spec.name,
                    version,
                    registry,
                    self.metadata_cache.as_deref(),
                    self.cache_mode,
                    &self.diagnostics,
                )
            }
            VersionRequest::Latest | VersionRequest::Range(_) => self.packument(&spec.name),
        };
        self.diagnostics.resolver_fetch_nanos.fetch_add(
            started.elapsed().as_nanos() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        outcome
    }

    /// Fetch a typed packument for use by dependency-graph resolution.
    ///
    /// Coordinates with the prefetch pool via the `InFlight` cache slot: if a
    /// background worker is already fetching this packument, the caller blocks
    /// on the condvar until it is `Ready` rather than issuing a duplicate
    /// request. A miss claims the slot and fetches inline. On error the slot is
    /// cleared so a later call (or prefetch) can retry.
    pub fn packument(&self, name: &str) -> Result<Packument, RegistryError> {
        let registry = self.config.registry_for_package(name);
        let key = format!("{}\0{name}", registry.trim_end_matches('/'));
        // Claim the slot or wait for an in-flight fetch. We never hold the
        // guard across the network fetch below.
        loop {
            let mut map = self.packument_cache.map.lock().unwrap();
            match map.get(&key) {
                Some(PackumentEntry::Ready(packument)) => {
                    self.diagnostics
                        .cache_hits
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return Ok(packument.clone());
                }
                Some(PackumentEntry::InFlight) => {
                    self.diagnostics
                        .cache_waits
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    map = self.packument_cache.cv.wait(map).unwrap();
                }
                None => {
                    self.diagnostics
                        .inline_fetches
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    map.insert(key.clone(), PackumentEntry::InFlight);
                    break;
                }
            }
        }
        let result = fetch_packument(
            &self.http,
            name,
            registry,
            self.metadata_cache.as_deref(),
            self.cache_mode,
            &self.diagnostics,
        );
        let mut map = self.packument_cache.map.lock().unwrap();
        match result {
            Ok(packument) => {
                map.insert(key, PackumentEntry::Ready(packument.clone()));
                self.packument_cache.cv.notify_all();
                Ok(packument)
            }
            Err(error) => {
                // Clear the in-flight marker so a subsequent attempt can retry;
                // the inline path is the single source of error reporting.
                map.remove(&key);
                self.packument_cache.cv.notify_all();
                Err(error)
            }
        }
    }

    /// Best-effort, non-blocking prefetch of one package's packument.
    ///
    /// Called by the resolver as soon as a node's dependency list is known so
    /// sibling packument fetches overlap during depth-first graph expansion.
    /// Idempotent: a no-op when prefetching is disabled, when the slot is
    /// already `Ready`, or when a fetch is already `InFlight`. Fetch failures
    /// are swallowed here; the synchronous [`Self::packument`] path reports
    /// the real error when the resolver reaches that package.
    /// Best-effort, non-blocking prefetch of one package's packument.
    ///
    /// Called by the resolver as soon as a node's dependency list is known so
    /// sibling packument fetches overlap during depth-first graph expansion.
    /// Idempotent: a no-op when prefetching is disabled, when the slot is
    /// already `Ready`, or when a fetch is already `InFlight`. Fetch failures
    /// are swallowed here; the synchronous [`Self::packument`] path reports
    /// the real error when the resolver reaches that package.
    ///
    /// `version_spec` (e.g. `^4.0.0`) is carried into the worker so a
    /// successful fetch can select the version the resolver will place and
    /// recursively prefetch *its* registry children (see [`prefetch_child_specs`]).
    /// That turns the pool from a one-level lookahead into a concurrent
    /// closure fetcher, so the resolver thread mostly hits cache and runs at
    /// CPU speed instead of serializing on the dependency-tree depth.
    pub fn prefetch_packument(&self, name: &str, version_spec: Option<&str>) {
        if self.prefetch_workers == 0 {
            return;
        }
        let pool = self.prefetch_pool.get_or_init(|| {
            PrefetchPool::start(
                self.prefetch_workers,
                self.http.clone(),
                self.config.clone(),
                self.metadata_cache.clone(),
                self.packument_cache.clone(),
                self.cache_mode,
                self.diagnostics.clone(),
            )
        });
        enqueue_prefetch(
            &self.packument_cache,
            &pool.sender_slot,
            &self.config,
            name,
            version_spec,
        );
    }

    /// Prefetch the dependency closure for root dependencies up to `max_depth`
    /// BFS levels before the resolver's DFS traversal begins.
    ///
    /// **How it works** (pnpm's approach — separates metadata fetch from graph
    /// traversal):
    ///
    /// 1. Scan the root manifest's dependency declarations for registry-typed
    ///    specs (skipping `file:`, `git:`, `workspace:`, etc.).
    /// 2. At each BFS level, submit **all** packument fetches to the prefetch
    ///    pool simultaneously so they run in parallel across workers.
    /// 3. Block until every packument at this level is ready (the existing
    ///    `packument()` condvar wait reuses the same cache coordination the
    ///    resolver uses during DFS).
    /// 4. For each completed packument, select the version the resolver will
    ///    place and extract its registry-typed children, which become the next
    ///    BFS level.
    /// 5. Repeat up to `max_depth` levels or until no new registry deps are
    ///    discovered.
    ///
    /// After this call, the in-memory packument cache is populated for the
    /// first N levels of the dependency tree. The resolver's DFS will hit
    /// cache for those packages instead of blocking on the network for each
    /// new level's packuments — the dominant factor in the large-frontend
    /// 4.9× benchmark gap.
    ///
    /// **Error handling**: root-level fetch failures propagate as errors.
    /// Failures at deeper levels are silently skipped (the resolver will
    /// re-fetch those packuments inline and surface the real error there).
    ///
    /// Returns the total number of packuments fetched during the batch phase.
    pub fn prefetch_batch_closure(
        &self,
        root_deps: &std::collections::BTreeMap<String, String>,
        max_depth: u32,
    ) -> Result<u64, RegistryError> {
        if self.prefetch_workers == 0 && !self.cache_mode.allows_network() {
            return Ok(0);
        }
        // Level 0: extract registry-typed root dependencies, skipping
        // workspace:, npm:, and other non-registry spec types.
        let mut current: Vec<(String, String)> = root_deps
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .filter(|(_, spec)| {
                // Re-check workspace: explicitly — published packuments never
                // carry workspace: specs (so looks_like_registry_spec does not
                // filter them) but root manifests can declare them.
                looks_like_registry_spec(spec) && !spec.starts_with("workspace:")
            })
            .collect();
        let mut total: u64 = 0;
        let mut seen: std::collections::BTreeSet<String> =
            current.iter().map(|(n, _)| n.clone()).collect();

        for depth in 0..max_depth {
            if current.is_empty() {
                break;
            }
            // Phase A: submit every dep at this level to the prefetch pool.
            for (name, spec) in &current {
                self.prefetch_packument(name, Some(spec));
            }
            // Phase B: block until each is ready, extract children.
            let mut next: Vec<(String, String)> = Vec::new();
            for (name, spec) in &current {
                let packument = match self.packument(name) {
                    Ok(p) => p,
                    Err(error) => {
                        if depth == 0 {
                            return Err(error);
                        }
                        // Deeper levels are best-effort: the resolver will
                        // re-fetch inline and surface the real error.
                        continue;
                    }
                };
                total += 1;
                self.diagnostics
                    .batch_prefetch_fetches
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                for (child, child_spec) in prefetch_child_specs(&packument, name, Some(spec)) {
                    if seen.insert(child.clone()) {
                        next.push((child, child_spec));
                    }
                }
            }
            current = next;
        }
        Ok(total)
    }
}

/// Claim an `InFlight` cache slot for `name` (if unclaimed) and enqueue a
/// background fetch. The slot claim deduplicates: a name already `Ready` or
/// `InFlight` is never re-enqueued, which bounds total prefetch work to the
/// dependency closure size and makes cycles a no-op.
///
/// The sender is borrowed transiently from the shared `sender_slot` (cloned
/// only for the one `send`). Workers never hold a persistent sender clone, so
/// taking the slot (`None`) on pool drop leaves no live senders and every
/// worker's `recv()` returns `Disconnected` — clean shutdown without timeouts.
fn enqueue_prefetch(
    cache: &Arc<PackumentCache>,
    sender_slot: &PrefetchSender,
    config: &crate::config::NpmConfig,
    name: &str,
    version_spec: Option<&str>,
) {
    let registry = config.registry_for_package(name).to_owned();
    let key = format!("{}\0{name}", registry.trim_end_matches('/'));
    let claimed = {
        let mut map = cache.map.lock().unwrap();
        if map.contains_key(&key) {
            false
        } else {
            map.insert(key.clone(), PackumentEntry::InFlight);
            true
        }
    };
    if !claimed {
        return;
    }
    let Some(sender) = sender_slot.lock().unwrap().clone() else {
        // Pool is shutting down: release the marker so the synchronous path can
        // still fetch this name inline if the resolver reaches it.
        cache.map.lock().unwrap().remove(&key);
        return;
    };
    let _ = sender.send(PrefetchJob {
        key,
        name: name.to_owned(),
        registry,
        version_spec: version_spec.map(str::to_owned),
    });
}

/// Heuristic mirroring `resolver::DependencySource::parse` (kept duplicated
/// here to avoid the registry layer reaching up into the resolver): true when
/// `spec` is a plain registry version/range rather than a source, git, tarball,
/// or alias spec. Published packuments never carry `workspace:` specs, so no
/// workspace filtering is needed for lookahead over registry metadata.
fn looks_like_registry_spec(spec: &str) -> bool {
    let lower = spec.to_ascii_lowercase();
    !(spec.starts_with("patch:")
        || spec.starts_with("file:")
        || spec.starts_with("link:")
        || lower.starts_with("git+")
        || lower.starts_with("git:")
        || lower.starts_with("github:")
        || lower.starts_with("http:")
        || lower.starts_with("https:")
        || lower.starts_with("npm:")
        || spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with('/'))
}

/// Build the [`VersionRequest`] the resolver will use for `name`/`version_spec`,
/// so the lookahead reads the *same* version's dependencies the resolver will
/// place. `None` (prefetch with no range) falls back to `Latest`.
fn lookahead_version_request(name: &str, version_spec: Option<&str>) -> Option<VersionRequest> {
    match version_spec {
        None | Some("") | Some("latest") => Some(VersionRequest::Latest),
        Some(spec) => parse_spec(&format!("{name}@{spec}"))
            .ok()
            .map(|parsed| parsed.req),
    }
}

/// Registry-typed runtime + optional dependencies of the version the resolver
/// will place for `name`/`version_spec`, for recursive prefetch lookahead.
/// Returns an empty vec when the version cannot be selected (the resolver will
/// fetch those children inline).
fn prefetch_child_specs(
    packument: &Packument,
    name: &str,
    version_spec: Option<&str>,
) -> Vec<(String, String)> {
    let Some(request) = lookahead_version_request(name, version_spec) else {
        return Vec::new();
    };
    let Ok(version) = select_version(name, &request, packument) else {
        return Vec::new();
    };
    let Some(metadata) = packument.versions.get(version.to_string().as_str()) else {
        return Vec::new();
    };
    let mut children = Vec::new();
    for (child, spec) in &metadata.dependencies {
        if looks_like_registry_spec(spec) {
            children.push((child.clone(), spec.clone()));
        }
    }
    for (child, spec) in &metadata.optional_dependencies {
        if looks_like_registry_spec(spec) {
            children.push((child.clone(), spec.clone()));
        }
    }
    children
}

/// One unit of background packument work.
struct PrefetchJob {
    key: String,
    name: String,
    registry: String,
    /// The version range the resolver requested for this package, so a worker
    /// can select the matching version and prefetch its registry children.
    version_spec: Option<String>,
}

/// A fixed-size pool of worker threads that resolve prefetched packuments.
///
/// Workers share one `mpsc` receiver (guarded by a mutex, the standard
/// `mpsc` multi-consumer pattern) and close over clones of the pooled HTTP
/// client and the in-process cache, so prefetches multiplex over the same
/// HTTP/2 connection pool as synchronous fetches. The pool is joined on drop,
/// which happens when the last `RegistryClient` clone is dropped.
/// Shared, takeable sender so the pool can shut the channel down by clearing
/// it. Workers borrow the sender transiently (clone-per-send) and never hold a
/// persistent clone, so clearing the slot leaves no live senders and every
/// worker's `recv()` returns `Disconnected`.
type PrefetchSender = Arc<Mutex<Option<mpsc::Sender<PrefetchJob>>>>;

/// A fixed-size pool of worker threads that resolve prefetched packuments.
///
/// Workers share one `mpsc` receiver (guarded by a mutex, the standard
/// `mpsc` multi-consumer pattern) and close over clones of the pooled HTTP
/// client and the in-process cache, so prefetches multiplex over the same
/// HTTP/2 connection pool as synchronous fetches. The pool is joined on drop,
/// which happens when the last `RegistryClient` clone is dropped.
struct PrefetchPool {
    sender_slot: PrefetchSender,
    handles: Vec<JoinHandle<()>>,
}

impl PrefetchPool {
    fn start(
        workers: usize,
        http: HttpClient,
        config: crate::config::NpmConfig,
        metadata_cache: Option<Arc<MetadataCache>>,
        cache: Arc<PackumentCache>,
        cache_mode: CacheMode,
        diagnostics: Arc<ResolverDiagnostics>,
    ) -> PrefetchPool {
        let (sender, receiver) = mpsc::channel::<PrefetchJob>();
        let receiver = Arc::new(Mutex::new(receiver));
        let sender_slot: PrefetchSender = Arc::new(Mutex::new(Some(sender)));
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let receiver = Arc::clone(&receiver);
            let http = http.clone();
            let config = config.clone();
            let metadata_cache = metadata_cache.clone();
            let cache = Arc::clone(&cache);
            let sender_slot = Arc::clone(&sender_slot);
            let diag = Arc::clone(&diagnostics);
            handles.push(std::thread::spawn(move || {
                loop {
                    // Hold the receiver guard only long enough to dequeue, so
                    // other workers are not blocked while this one fetches.
                    let job = {
                        let guard = receiver.lock().unwrap();
                        match guard.recv() {
                            Ok(job) => job,
                            Err(_) => break, // channel closed: pool shutting down
                        }
                    };
                    diag.prefetch_fetches
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let result = fetch_packument_for_spec(
                        &http,
                        &job.name,
                        &job.registry,
                        job.version_spec.as_deref(),
                        metadata_cache.as_deref(),
                        cache_mode,
                        &diag,
                    );
                    {
                        let mut map = cache.map.lock().unwrap();
                        match &result {
                            Ok(packument) => {
                                map.insert(
                                    job.key.clone(),
                                    PackumentEntry::Ready(packument.clone()),
                                );
                            }
                            Err(_) => {
                                // Clear the in-flight marker; the synchronous path
                                // will re-fetch and surface the real error.
                                map.remove(&job.key);
                            }
                        }
                        cache.cv.notify_all();
                    }
                    // Recursive lookahead: now that this packument is Ready,
                    // select the version the resolver will place and prefetch
                    // its registry children so the closure fans out
                    // concurrently instead of one DFS level at a time. Slot
                    // claiming deduplicates, bounding work to the closure and
                    // making cycles a no-op.
                    if let Ok(packument) = &result {
                        for (child, child_spec) in
                            prefetch_child_specs(packument, &job.name, job.version_spec.as_deref())
                        {
                            enqueue_prefetch(
                                &cache,
                                &sender_slot,
                                &config,
                                &child,
                                Some(&child_spec),
                            );
                        }
                    }
                }
            }));
        }
        PrefetchPool {
            sender_slot,
            handles,
        }
    }
}

impl Drop for PrefetchPool {
    fn drop(&mut self) {
        // Take the sender so no new jobs can be enqueued. Because workers never
        // hold a persistent sender clone (they borrow it transiently via the
        // slot), once this is `None` the channel has no live senders and every
        // worker's `recv()` returns `Disconnected`, letting them exit before we
        // join. Joining guarantees no worker outlives the client, which matters
        // for deterministic test teardown.
        self.sender_slot.lock().unwrap().take();
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("invalid package spec '{0}': {1}")]
    InvalidSpec(String, String),
    #[error("registry request for {package} failed")]
    Network {
        package: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("registry returned status {code} for {package}")]
    BadStatus { package: String, code: u16 },
    #[error("registry response for {package} was not valid JSON")]
    BadJson {
        package: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("packument for {package} has no versions")]
    NoVersions { package: String },
    #[error("no version of {package} satisfies {req}")]
    VersionNotFound { package: String, req: String },
    #[error("packument for {package}@{version} is missing a tarball URL or integrity")]
    MissingDist { package: String, version: String },
    #[error("packument for {package}@{version} declared an unsupported tarball source (scheme '{scheme}'); only HTTP/HTTPS or registry-relative URLs are accepted")]
    UnsupportedTarballSource {
        package: String,
        version: String,
        scheme: String,
    },
    #[error("packument for {package}@{version} has malformed integrity: {detail}")]
    InvalidIntegrity {
        package: String,
        version: String,
        detail: String,
    },
    #[error("no cached metadata for {url}; --offline refused to contact the registry")]
    OfflineMiss { url: String },
}

/// Parse a package spec string into a name + version request.
///
/// The version separator is the last `@` that is not the leading scope marker
/// of a scoped name. So `@scope/pkg` has no version, but `@scope/pkg@1.2.3`
/// and `pkg@1.2.3` do.
pub fn parse_spec(spec: &str) -> Result<PackageSpec, RegistryError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(RegistryError::InvalidSpec(
            spec.to_string(),
            "spec is empty".to_string(),
        ));
    }

    let (name, req_str) = match spec.rfind('@') {
        // `@scope/pkg` (the only `@` is the leading scope marker) or bare `pkg`.
        Some(0) | None => (spec, None),
        // `<name>@<req>` or `@scope/name@<req>`.
        Some(i) => (&spec[..i], Some(&spec[i + 1..])),
    };

    if !is_valid_npm_name(name) {
        return Err(RegistryError::InvalidSpec(
            spec.to_string(),
            format!("'{name}' is not a valid npm package name"),
        ));
    }

    let req = match req_str.map(str::trim) {
        None | Some("") | Some("latest") => VersionRequest::Latest,
        Some(s) if s.starts_with(['^', '~', '>', '<', '=', '*']) => {
            VersionRequest::Range(VersionRange::parse(s).map_err(|e| {
                RegistryError::InvalidSpec(spec.to_string(), format!("bad range '{s}': {e}"))
            })?)
        }
        Some(s) => {
            // A bare version like `1.2.3` is exact; anything else (e.g. `1.x`)
            // is treated as a range.
            match Version::parse(s) {
                Ok(v) => VersionRequest::Exact(v),
                Err(_) => VersionRequest::Range(VersionRange::parse(s).map_err(|e| {
                    RegistryError::InvalidSpec(spec.to_string(), format!("bad version '{s}': {e}"))
                })?),
            }
        }
    };

    Ok(PackageSpec {
        name: name.to_string(),
        req,
    })
}

/// Compatibility API resolving `spec` against `registry` (a base URL like
/// `https://registry.npmjs.org`) by fetching the packument and selecting a
/// version with a default-config HTTP client.
///
/// Configured production callers should use [`RegistryClient::with_client`]
/// so metadata and artifact retrieval share one caller-owned pool.
pub fn resolve(spec: &PackageSpec, registry: &str) -> Result<ResolvedArtifact, RegistryError> {
    let http = HttpClient::new(NpmConfig::default());
    let diagnostics = ResolverDiagnostics::new();
    let packument = fetch_packument(
        &http,
        &spec.name,
        registry,
        None,
        CacheMode::Default,
        &diagnostics,
    )?;
    resolve_packument(spec, &packument, registry)
}

/// Select one version from an already-fetched packument.
pub fn resolve_packument(
    spec: &PackageSpec,
    packument: &Packument,
    registry: &str,
) -> Result<ResolvedArtifact, RegistryError> {
    let version = select_version(&spec.name, &spec.req, packument)?;
    let mut metadata = packument
        .versions
        .get(version.to_string().as_str())
        .ok_or_else(|| RegistryError::VersionNotFound {
            package: spec.name.clone(),
            req: version.to_string(),
        })?
        .clone();
    if metadata.name.is_empty() {
        metadata.name.clone_from(&spec.name);
    }
    if metadata.dist.tarball.is_empty() || metadata.dist.integrity.is_empty() {
        return Err(RegistryError::MissingDist {
            package: spec.name.clone(),
            version: version.to_string(),
        });
    }

    // Reject malformed or unsupported integrity before any tarball request.
    let _integrity = Integrity::parse(&metadata.dist.integrity).map_err(|error| {
        RegistryError::InvalidIntegrity {
            package: spec.name.clone(),
            version: version.to_string(),
            detail: error.to_string(),
        }
    })?;

    let tarball_url = resolve_tarball_url(
        registry,
        &metadata.dist.tarball,
        &spec.name,
        &version.to_string(),
    )?;

    Ok(ResolvedArtifact {
        name: metadata.name.clone(),
        version,
        tarball_url,
        integrity: metadata.dist.integrity.clone(),
        metadata,
    })
}

/// Resolve a registry packument's `dist.tarball` into the artifact URL BPM will
/// download, validating its provenance first.
///
/// Only absolute `http://`/`https://` URLs (cross-origin allowed, query
/// strings preserved for signed CDNs) and paths relative to the configured
/// registry base are accepted. Every other form — `file:`/bare local paths,
/// `ftp:`/`gopher:`/etc. — is rejected before any artifact request, because a
/// registry must not redirect a client to an unintended local source. The
/// error carries the scheme and package/version, never the raw (possibly
/// credential-bearing) URL.
fn resolve_tarball_url(
    registry: &str,
    tarball: &str,
    package: &str,
    version: &str,
) -> Result<String, RegistryError> {
    match reqwest::Url::parse(tarball) {
        Ok(url) => match url.scheme() {
            "http" | "https" => Ok(tarball.to_string()),
            scheme => Err(RegistryError::UnsupportedTarballSource {
                package: package.to_string(),
                version: version.to_string(),
                scheme: scheme.to_string(),
            }),
        },
        Err(_) => {
            // No absolute scheme: a registry-relative path. Resolve it against
            // the configured registry base, preserving the established
            // same-registry behavior.
            Ok(format!(
                "{}/{}",
                registry.trim_end_matches('/'),
                tarball.trim_start_matches('/')
            ))
        }
    }
}

/// Fetch and parse the packument JSON for `name` through the shared client.
/// Fetch the smallest metadata document for `name`/`version_spec`, mirroring
/// the resolver's `packument_for`: an exact version uses the per-version
/// endpoint (a few KB even for packages whose full packument is megabytes);
/// a range or tag uses the abbreviated packument. This keeps prefetch from
/// downloading multi-megabyte packuments for pinned-exact dependencies.
fn fetch_packument_for_spec(
    http: &HttpClient,
    name: &str,
    registry: &str,
    version_spec: Option<&str>,
    cache: Option<&MetadataCache>,
    mode: CacheMode,
    diagnostics: &Arc<ResolverDiagnostics>,
) -> Result<Packument, RegistryError> {
    match exact_version_from_spec(name, version_spec) {
        Some(version) => {
            fetch_version_packument(http, name, &version, registry, cache, mode, diagnostics)
        }
        None => fetch_packument(http, name, registry, cache, mode, diagnostics),
    }
}

/// If `version_spec` pins an exact version, return it. `None` for ranges, tags,
/// or absent specs (which need the full version list to resolve).
fn exact_version_from_spec(name: &str, version_spec: Option<&str>) -> Option<Version> {
    let spec = version_spec?;
    let parsed = parse_spec(&format!("{name}@{spec}")).ok()?;
    match parsed.req {
        VersionRequest::Exact(version) => Some(version),
        _ => None,
    }
}

fn fetch_packument(
    http: &HttpClient,
    name: &str,
    registry: &str,
    cache: Option<&MetadataCache>,
    mode: CacheMode,
    diagnostics: &Arc<ResolverDiagnostics>,
) -> Result<Packument, RegistryError> {
    let base = registry.trim_end_matches('/');
    // npm encodes scoped names so the whole name is one path segment.
    let encoded = name.replace('/', "%2F");
    let url = format!("{base}/{encoded}");

    let body = fetch_with_cache(http, &url, name, cache, mode, true, diagnostics)?;
    serde_json::from_str(&body).map_err(|source| RegistryError::BadJson {
        package: name.to_string(),
        source,
    })
}

fn fetch_version_packument(
    http: &HttpClient,
    name: &str,
    version: &Version,
    registry: &str,
    cache: Option<&MetadataCache>,
    mode: CacheMode,
    diagnostics: &Arc<ResolverDiagnostics>,
) -> Result<Packument, RegistryError> {
    let base = registry.trim_end_matches('/');
    let encoded = name.replace('/', "%2F");
    let url = format!("{base}/{encoded}/{version}");
    let body = fetch_with_cache(http, &url, name, cache, mode, false, diagnostics)?;
    let wire: WireVersionMetadata =
        serde_json::from_str(&body).map_err(|source| RegistryError::BadJson {
            package: name.to_string(),
            source,
        })?;
    let metadata = version_metadata(name, &version.to_string(), wire).ok_or_else(|| {
        RegistryError::NoVersions {
            package: name.to_string(),
        }
    })?;
    Ok(Packument {
        name: name.to_string(),
        dist_tags: BTreeMap::new(),
        versions: BTreeMap::from([(version.to_string(), metadata)]),
    })
}

/// Retrieve the response body for `url`, consulting and updating the optional
/// persistent cache according to `mode`.
///
/// Revalidation is npm-compatible: a cached entry's `ETag` / `Last-Modified`
/// are sent as `If-None-Match` / `If-Modified-Since`, a `304 Not Modified`
/// reuses the stored body verbatim, and a `200` refreshes the cache. The
/// stored body is byte-for-byte what the registry last sent, so resolution
/// output stays deterministic regardless of whether a request was served from
/// the cache or the network.
///
/// `send_abbreviated_accept` negotiates npm's abbreviated install-metadata
/// media type for packuments; per-version endpoints omit it.
fn fetch_with_cache(
    http: &HttpClient,
    url: &str,
    package: &str,
    cache: Option<&MetadataCache>,
    mode: CacheMode,
    send_abbreviated_accept: bool,
    diagnostics: &Arc<ResolverDiagnostics>,
) -> Result<String, RegistryError> {
    // A persistent-cache read failure degrades to a miss for every mode: the
    // online modes fall back to a network fetch, and offline mode fails on the
    // resulting missing entry below.
    let cached = cache.and_then(|store| store.get(url).ok()).flatten();

    if !mode.allows_network() {
        return cached
            .map(|entry| entry.body)
            .map(|bytes| String::from_utf8(bytes).unwrap_or_default())
            .ok_or_else(|| RegistryError::OfflineMiss {
                url: url.to_string(),
            });
    }

    // PreferOffline may serve a still-cached body without any round-trip.
    if mode.serves_stale() {
        if let Some(entry) = cached.as_ref() {
            return Ok(String::from_utf8_lossy(&entry.body).into_owned());
        }
    }

    let mut headers: Vec<(&str, &str)> = Vec::new();
    if send_abbreviated_accept {
        headers.push(("Accept", ABBREV_ACCEPT));
    }
    if let Some(entry) = cached.as_ref() {
        if let Some(etag) = entry.etag.as_deref() {
            headers.push(("If-None-Match", etag));
        }
        if let Some(last_modified) = entry.last_modified.as_deref() {
            headers.push(("If-Modified-Since", last_modified));
        }
    }

    let response =
        http.get_with_headers(url, &headers)
            .map_err(|source| RegistryError::Network {
                package: package.to_string(),
                source: Box::new(source),
            })?;

    if response.status() == 304 {
        // The registry confirmed the cached body is still current. A validator
        // must have been sent for the registry to answer 304, so a cached entry
        // exists; otherwise treat the unexpected 304 as a protocol error.
        return cached
            .map(|entry| String::from_utf8_lossy(&entry.body).into_owned())
            .ok_or_else(|| RegistryError::BadStatus {
                package: package.to_string(),
                code: 304,
            });
    }

    let etag = response.header("ETag").map(str::to_owned);
    let last_modified = response.header("Last-Modified").map(str::to_owned);
    let body = response
        .into_string()
        .map_err(|error| RegistryError::Network {
            package: package.to_string(),
            source: Box::new(error),
        })?;
    diagnostics
        .fetch_bytes
        .fetch_add(body.len() as u64, std::sync::atomic::Ordering::Relaxed);

    // Best-effort refresh: a write failure must never fail an install.
    if let Some(store) = cache {
        let _ = store.put(
            url,
            body.as_bytes(),
            etag.as_deref(),
            last_modified.as_deref(),
        );
    }

    Ok(body)
}

/// Pick the target version string from a packument for a version request.
pub(crate) fn select_version(
    name: &str,
    req: &VersionRequest,
    packument: &Packument,
) -> Result<Version, RegistryError> {
    if packument.versions.is_empty() {
        return Err(RegistryError::NoVersions {
            package: name.to_string(),
        });
    }

    match req {
        VersionRequest::Latest => {
            let tag = packument
                .dist_tags
                .get("latest")
                .map(String::as_str)
                .ok_or_else(|| RegistryError::NoVersions {
                    package: name.to_string(),
                })?;
            Version::parse(tag).map_err(|_| RegistryError::VersionNotFound {
                package: name.to_string(),
                req: format!("latest ({tag})"),
            })
        }
        VersionRequest::Exact(v) => {
            if packument.versions.contains_key(v.to_string().as_str()) {
                Ok(v.clone())
            } else {
                Err(RegistryError::VersionNotFound {
                    package: name.to_string(),
                    req: format!("={v}"),
                })
            }
        }
        VersionRequest::Range(r) => {
            // Deterministic max: parse all, filter, take the greatest (prereleases
            // excluded by `semver` unless the range explicitly opts in).
            let mut matching: Vec<Version> = packument
                .versions
                .keys()
                .filter_map(|k| Version::parse(k).ok())
                .filter(|v| r.matches(v))
                .collect();
            matching.sort();
            matching
                .pop()
                .ok_or_else(|| RegistryError::VersionNotFound {
                    package: name.to_string(),
                    req: r.to_string(),
                })
        }
    }
}

/// Validate a package name per npm rules: `(@scope/)?name`, ASCII, <=214 chars,
/// each segment starts with a lowercase letter or digit and otherwise contains
/// only `[a-z0-9._-]`.
pub fn is_valid_npm_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 214 || !name.is_ascii() {
        return false;
    }
    match name.strip_prefix('@') {
        Some(rest) => match rest.split_once('/') {
            Some((scope, pkg)) => valid_segment(scope.as_bytes()) && valid_segment(pkg.as_bytes()),
            None => false,
        },
        None => valid_segment(name.as_bytes()),
    }
}

fn valid_segment(seg: &[u8]) -> bool {
    if seg.is_empty() {
        return false;
    }
    let first = seg[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    seg.iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::sync::Arc;

    use crate::metadata_cache::{CacheMode, MetadataCache};

    fn packument(value: serde_json::Value) -> Packument {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn parses_bare_name_as_latest() {
        let s = parse_spec("lodash").unwrap();
        assert_eq!(s.name, "lodash");
        assert!(matches!(s.req, VersionRequest::Latest));
    }

    #[test]
    fn parses_scoped_name_without_version_as_latest() {
        let s = parse_spec("@scope/pkg").unwrap();
        assert_eq!(s.name, "@scope/pkg");
        assert!(matches!(s.req, VersionRequest::Latest));
    }

    #[test]
    fn parses_exact_version() {
        let s = parse_spec("lodash@4.17.21").unwrap();
        assert_eq!(s.name, "lodash");
        match s.req {
            VersionRequest::Exact(v) => assert_eq!(v, Version::parse("4.17.21").unwrap()),
            other => panic!("expected exact, got {other:?}"),
        }
    }

    #[test]
    fn parses_scoped_exact_version() {
        let s = parse_spec("@scope/pkg@1.2.3").unwrap();
        assert_eq!(s.name, "@scope/pkg");
        assert!(matches!(s.req, VersionRequest::Exact(_)));
    }

    #[test]
    fn parses_caret_and_tilde_as_range() {
        for spec in [
            "lodash@^4.17.0",
            "lodash@~4.17.0",
            "lodash@>=4.0.0",
            "lodash@*",
            "lodash@4.x",
        ] {
            let s = parse_spec(spec).unwrap_or_else(|e| panic!("parse {spec}: {e}"));
            assert_eq!(s.name, "lodash");
            assert!(matches!(s.req, VersionRequest::Range(_)), "{spec}");
        }
    }

    #[test]
    fn scoped_single_bin_uses_unscoped_command_name() {
        let packument = packument(serde_json::json!({
            "name": "@scope/pkg",
            "versions": {
                "1.0.0": { "version": "1.0.0", "bin": "cli.js" }
            }
        }));
        let metadata = packument.versions.get("1.0.0").unwrap();
        assert_eq!(metadata.bin.keys().collect::<Vec<_>>(), vec!["pkg"]);
    }

    #[test]
    fn disjunctive_ranges_match_any_requirement() {
        let spec = parse_spec("js-tokens@^3.0.0 || ^4.0.0").unwrap();
        let VersionRequest::Range(range) = spec.req else {
            panic!("expected range");
        };
        assert!(range.matches(&Version::new(3, 0, 2)));
        assert!(range.matches(&Version::new(4, 0, 1)));
        assert!(!range.matches(&Version::new(2, 0, 0)));
        assert_eq!(range.to_string(), "^3.0.0 || ^4.0.0");
    }

    #[test]
    fn rejects_empty_spec() {
        assert!(parse_spec("").is_err());
        assert!(parse_spec("   ").is_err());
    }

    #[test]
    fn rejects_uppercase_and_invalid_names() {
        assert!(parse_spec("Lodash").is_err());
        assert!(parse_spec("has space").is_err());
        assert!(parse_spec("@noslash").is_err());
        assert!(parse_spec("@scope/").is_err());
    }

    #[test]
    fn rejects_bad_version() {
        assert!(parse_spec("lodash@not-a-version!").is_err());
    }

    #[test]
    fn name_validation_examples() {
        assert!(is_valid_npm_name("lodash"));
        assert!(is_valid_npm_name("@scope/pkg"));
        assert!(!is_valid_npm_name("Lodash"));
        assert!(!is_valid_npm_name(""));
        assert!(!is_valid_npm_name("@scope"));
        assert!(!is_valid_npm_name("has space"));
    }

    #[test]
    fn select_version_picks_latest_from_dist_tags() {
        let packument = packument(serde_json::json!({
            "name": "lodash",
            "dist-tags": { "latest": "4.17.21" },
            "versions": { "1.0.0": {}, "4.17.21": {} }
        }));
        let v = select_version("lodash", &VersionRequest::Latest, &packument).unwrap();
        assert_eq!(v, Version::parse("4.17.21").unwrap());
    }

    #[test]
    fn select_version_range_picks_highest_match() {
        let packument = packument(serde_json::json!({
            "name": "lodash",
            "versions": { "1.0.0": {}, "4.0.0": {}, "4.17.20": {}, "4.17.21": {}, "5.0.0": {} }
        }));
        let req = VersionRequest::Range(VersionRange::parse("^4.0.0").unwrap());
        let v = select_version("lodash", &req, &packument).unwrap();
        assert_eq!(v, Version::parse("4.17.21").unwrap());
    }

    #[test]
    fn select_version_exact_missing_errors() {
        let packument = packument(serde_json::json!({
            "name": "p",
            "versions": { "1.0.0": {} }
        }));
        let req = VersionRequest::Exact(Version::parse("2.0.0").unwrap());
        let err = select_version("p", &req, &packument).unwrap_err();
        assert!(matches!(err, RegistryError::VersionNotFound { .. }));
    }

    #[test]
    fn resolve_reads_tarball_and_integrity() {
        let valid_integrity = "sha512-abababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababab";
        let packument = packument(serde_json::json!({
            "name": "p",
            "dist-tags": { "latest": "1.2.3" },
            "versions": {
                "1.2.3": {
                    "dist": {
                        "tarball": "https://example.test/p/-/p-1.2.3.tgz",
                        "integrity": valid_integrity,
                    }
                }
            }
        }));
        let spec = parse_spec("p").unwrap();
        let resolved = resolve_packument(&spec, &packument, "https://example.test/").unwrap();
        assert_eq!(resolved.tarball_url, "https://example.test/p/-/p-1.2.3.tgz");
        assert_eq!(resolved.integrity, valid_integrity);
    }

    /// Plan 012: registry `dist.tarball` provenance matrix.
    fn resolve_tarball(registry: &str, tarball: &str) -> Result<String, RegistryError> {
        super::resolve_tarball_url(registry, tarball, "p", "1.2.3")
    }

    #[test]
    fn registry_tarball_relative_path_resolves_against_registry() {
        let url = resolve_tarball("https://example.test", "p/-/p-1.2.3.tgz").unwrap();
        assert_eq!(url, "https://example.test/p/-/p-1.2.3.tgz");
    }

    #[test]
    fn registry_tarball_accepts_same_and_cross_origin_http_https() {
        assert_eq!(
            resolve_tarball("https://reg.test", "http://127.0.0.1:9/x.tgz").unwrap(),
            "http://127.0.0.1:9/x.tgz"
        );
        // Cross-origin HTTPS with a signed query string is preserved.
        let signed = "https://cdn.other.test/p.tgz?token=secret";
        assert_eq!(resolve_tarball("https://reg.test", signed).unwrap(), signed);
    }

    #[test]
    fn registry_tarball_rejects_file_and_non_http_schemes() {
        let err = resolve_tarball("https://reg.test", "file:///etc/passwd").unwrap_err();
        assert!(
            format!("{err}").contains("unsupported tarball source"),
            "expected unsupported-source rejection; got: {err}"
        );
        assert!(
            format!("{err}").contains("file"),
            "scheme must be reported; got: {err}"
        );
        assert!(
            !format!("{err}").contains("/etc/passwd"),
            "the raw URL/path must not appear in the error"
        );
        // Bare non-http scheme.
        resolve_tarball("https://reg.test", "gopher://x/y").unwrap_err();
        // file: without //
        resolve_tarball("https://reg.test", "file:rel/x").unwrap_err();
    }

    #[test]
    fn registry_tarball_joins_a_relative_local_path() {
        // A bare relative path (no scheme, no host) is treated as
        // registry-relative and joined; it is NOT accepted as an absolute
        // local source.
        let url = resolve_tarball("https://reg.test", "local.tgz").unwrap();
        assert_eq!(url, "https://reg.test/local.tgz");
    }

    #[test]
    fn exact_resolution_uses_version_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let registry = format!("http://{address}");
        let (request_tx, request_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            request_tx
                .send(String::from_utf8_lossy(&request[..read]).into_owned())
                .unwrap();
            let body = r#"{"name":"p","version":"1.2.3","dist":{"tarball":"https://example.test/p.tgz","integrity":"sha512-abababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababab"}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });

        let config = NpmConfig::default()
            .with_registry_override(&registry)
            .unwrap();
        let client = RegistryClient::new(config);
        let resolved = client.resolve(&parse_spec("p@1.2.3").unwrap()).unwrap();
        server.join().unwrap();

        assert_eq!(resolved.version, Version::new(1, 2, 3));
        assert!(request_rx
            .recv()
            .unwrap()
            .starts_with("GET /p/1.2.3 HTTP/1.1"));
    }

    #[test]
    fn concurrent_prefetch_and_packument_deduplicate_to_one_request() {
        // A prefetch in flight for a package must make a concurrent packument()
        // call wait for the same result instead of issuing a second request.
        // The server counts accepted connections to prove the dedup.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let registry = format!("http://{address}");
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_count = Arc::clone(&count);
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_shutdown = Arc::clone(&shutdown);
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            while !server_shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    Err(_) => break,
                };
                server_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Hold the response briefly so the prefetch is still in flight
                // when the main thread calls packument().
                std::thread::sleep(std::time::Duration::from_millis(40));
                let mut request = [0_u8; 2048];
                let _ = stream.read(&mut request);
                let body = r#"{"name":"pkg","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"pkg","version":"1.0.0","dist":{"tarball":"/pkg.tgz","integrity":"sha512-78000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });

        let config = NpmConfig::default()
            .with_registry_override(&registry)
            .unwrap();
        let client = RegistryClient::new(config).with_prefetch(1);
        client.prefetch_packument("pkg", None);
        // Let the worker claim the slot and reach the in-flight fetch.
        std::thread::sleep(std::time::Duration::from_millis(15));
        // This must wait on the in-flight prefetch instead of fetching again.
        let packument = client.packument("pkg").unwrap();
        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().unwrap();

        assert_eq!(packument.name, "pkg");
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "prefetch and packument should deduplicate to a single request"
        );
    }

    #[test]
    fn configured_client_uses_the_caller_owned_http_client() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let registry = format!("http://{address}");
        let (request_tx, request_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            request_tx
                .send(String::from_utf8_lossy(&request[..read]).into_owned())
                .unwrap();
            let body = r#"{
                "name":"p",
                "dist-tags":{"latest":"1.0.0"},
                "versions":{"1.0.0":{"dist":{"tarball":"https://example.test/p.tgz","integrity":"sha512-abababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababab"}}}
            }"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });

        let directory = tempfile::tempdir().unwrap();
        let npmrc = directory.path().join("configured.npmrc");
        fs::write(
            &npmrc,
            format!("registry={registry}\n//{address}/:_authToken=configured-token\n"),
        )
        .unwrap();
        let client_config = NpmConfig::load_paths(None, Some(&npmrc)).unwrap();
        let routing_config = NpmConfig::default()
            .with_registry_override(&registry)
            .unwrap();
        let client = RegistryClient::with_client(routing_config, HttpClient::new(client_config));

        let resolved = client.resolve(&parse_spec("p").unwrap()).unwrap();
        server.join().unwrap();
        let request = request_rx.recv().unwrap();

        assert_eq!(resolved.version, Version::new(1, 0, 0));
        // reqwest/hyper lowercases header field names on the wire; HTTP header
        // names are case-insensitive, so assert against a lowercased capture.
        let request_lc = request.to_ascii_lowercase();
        assert!(request_lc.contains("authorization: bearer configured-token\r\n"));
        assert!(request_lc.contains("accept: application/vnd.npm.install-v1+json\r\n"));
    }

    /// A minimal HTTP/1.1 test server returning 200 + `ETag` for an
    /// unconditional request and `304` for a request carrying `If-None-Match`.
    /// It records every request line over `requests` for assertion.
    fn conditional_server(
        body: &'static str,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let registry = format!("http://{address}");
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let recorded = requests.clone();
        let server = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = [0_u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                recorded.lock().unwrap().push(request.clone());
                if request.to_ascii_lowercase().contains("if-none-match:") {
                    write!(
                        stream,
                        "HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                } else {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                }
            }
        });
        (registry, requests, server)
    }

    #[test]
    fn persistent_cache_revalidates_and_serves_identical_packument() {
        let body = r#"{"name":"p","dist-tags":{"latest":"1.4.0"},"versions":{"1.4.0":{"dist":{"tarball":"https://example.test/p.tgz","integrity":"sha512-abababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababababab"}}}}"#;
        let (registry, requests, _server) = conditional_server(body);
        let config = NpmConfig::default()
            .with_registry_override(&registry)
            .unwrap();
        let cache = Arc::new(MetadataCache::open_in_memory().unwrap());

        // First resolve: uncached, so an unconditional GET answered with 200.
        let client1 = RegistryClient::with_client(config.clone(), HttpClient::new(config.clone()))
            .with_metadata_cache(cache.clone(), CacheMode::Default);
        let first = client1.resolve(&parse_spec("p").unwrap()).unwrap();
        assert_eq!(first.version, Version::new(1, 4, 0));

        // Second resolve with a brand-new client sharing only the persistent
        // cache: a conditional GET (`If-None-Match`) answered with 304, which
        // must reuse the stored body byte-for-byte (identical resolution).
        let client2 = RegistryClient::with_client(config.clone(), HttpClient::new(config.clone()))
            .with_metadata_cache(cache.clone(), CacheMode::Default);
        let second = client2.resolve(&parse_spec("p").unwrap()).unwrap();
        assert_eq!(second.version, Version::new(1, 4, 0));
        assert_eq!(second.tarball_url, first.tarball_url);
        assert_eq!(second.integrity, first.integrity);

        let captured = requests.lock().unwrap().clone();
        assert_eq!(captured.len(), 2);
        assert!(!captured[0].to_ascii_lowercase().contains("if-none-match:"));
        assert!(captured[1]
            .to_ascii_lowercase()
            .contains("if-none-match: \"v1\""));
    }

    #[test]
    fn prefer_offline_serves_stale_without_revalidation() {
        let body = r#"{"name":"p","dist-tags":{"latest":"2.0.0"},"versions":{"2.0.0":{"dist":{"tarball":"https://example.test/p.tgz","integrity":"sha512-ff000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#;
        let (registry, requests, _server) = conditional_server(body);
        let config = NpmConfig::default()
            .with_registry_override(&registry)
            .unwrap();
        let cache = Arc::new(MetadataCache::open_in_memory().unwrap());

        // Warm the cache with one full fetch.
        RegistryClient::with_client(config.clone(), HttpClient::new(config.clone()))
            .with_metadata_cache(cache.clone(), CacheMode::Default)
            .resolve(&parse_spec("p").unwrap())
            .unwrap();

        // PreferOffline must serve the cached body without any network contact.
        let resolved = RegistryClient::with_client(config.clone(), HttpClient::new(config.clone()))
            .with_metadata_cache(cache, CacheMode::PreferOffline)
            .resolve(&parse_spec("p").unwrap())
            .unwrap();
        assert_eq!(resolved.version, Version::new(2, 0, 0));

        // Exactly one request reached the server (the warm-up fetch).
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    // ── Recursive prefetch lookahead unit tests ────────────────────────

    #[test]
    fn looks_like_registry_spec_accepts_plain_versions_and_ranges() {
        assert!(looks_like_registry_spec("^1.0.0"));
        assert!(looks_like_registry_spec("~2.3.4"));
        assert!(looks_like_registry_spec(">=1.0.0 <2.0.0"));
        assert!(looks_like_registry_spec("*"));
        assert!(looks_like_registry_spec("1.2.3"));
        assert!(looks_like_registry_spec("latest"));
    }

    #[test]
    fn looks_like_registry_spec_rejects_non_registry_sources() {
        assert!(!looks_like_registry_spec("file:./local.tgz"));
        assert!(!looks_like_registry_spec("link:../other"));
        assert!(!looks_like_registry_spec(
            "git+https://github.com/user/repo.git"
        ));
        assert!(!looks_like_registry_spec("github:user/repo"));
        assert!(!looks_like_registry_spec("https://example.test/pkg.tgz"));
        assert!(!looks_like_registry_spec("./relative/path"));
        assert!(!looks_like_registry_spec("../sibling"));
        assert!(!looks_like_registry_spec("/absolute/path"));
        assert!(!looks_like_registry_spec("npm:@scope/pkg"));
        assert!(!looks_like_registry_spec("patch:some-patch"));
    }

    #[test]
    fn exact_version_from_spec_returns_some_for_exact_versions() {
        assert_eq!(
            exact_version_from_spec("lodash", Some("4.17.21")),
            Some(Version::new(4, 17, 21))
        );
        assert_eq!(
            exact_version_from_spec("@scope/pkg", Some("1.0.0")),
            Some(Version::new(1, 0, 0))
        );
    }

    #[test]
    fn exact_version_from_spec_returns_none_for_ranges_and_tags() {
        assert_eq!(exact_version_from_spec("lodash", Some("^4.0.0")), None);
        assert_eq!(exact_version_from_spec("lodash", Some("~4.0.0")), None);
        assert_eq!(exact_version_from_spec("lodash", Some("latest")), None);
        assert_eq!(exact_version_from_spec("lodash", None::<&str>), None);
        assert_eq!(exact_version_from_spec("lodash", Some("")), None);
    }

    fn version_meta(name: &str, version: &str) -> VersionMetadata {
        VersionMetadata {
            name: name.to_string(),
            version: Version::parse(version).unwrap(),
            deprecated: None,
            dependencies: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
            peer_dependencies_meta: BTreeMap::new(),
            bin: BTreeMap::new(),
            dist: Dist::default(),
            engines: BTreeMap::new(),
            os: Vec::new(),
            cpu: Vec::new(),
            libc: Vec::new(),
            has_install_script: false,
            has_shrinkwrap: false,
        }
    }

    #[test]
    fn prefetch_child_specs_extracts_registry_deps_only() {
        let mut v1 = version_meta("pkg", "1.0.0");
        v1.dependencies = BTreeMap::from([
            ("registry-child".to_string(), "^1.0.0".to_string()),
            (
                "git-child".to_string(),
                "git+https://example.test/repo.git".to_string(),
            ),
            ("file-child".to_string(), "file:./local".to_string()),
        ]);
        v1.optional_dependencies = BTreeMap::from([
            ("opt-child".to_string(), "^2.0.0".to_string()),
            (
                "http-child".to_string(),
                "https://example.test/tgz".to_string(),
            ),
        ]);

        let mut versions = BTreeMap::new();
        versions.insert("1.0.0".to_string(), v1);

        let packument = Packument {
            name: "pkg".to_string(),
            dist_tags: BTreeMap::from([("latest".to_string(), "1.0.0".to_string())]),
            versions,
        };

        let children = prefetch_child_specs(&packument, "pkg", Some("^1.0.0"));
        // Only registry-typed deps & optionalDeps: "registry-child" and "opt-child"
        assert_eq!(children.len(), 2);
        assert!(children.contains(&("registry-child".to_string(), "^1.0.0".to_string())));
        assert!(children.contains(&("opt-child".to_string(), "^2.0.0".to_string())));
        // git-child, file-child, and http-child are excluded
        assert!(!children.iter().any(|(n, _)| n == "git-child"));
        assert!(!children.iter().any(|(n, _)| n == "file-child"));
        assert!(!children.iter().any(|(n, _)| n == "http-child"));
    }

    #[test]
    fn prefetch_child_specs_returns_empty_on_unresolvable_spec() {
        let packument = Packument {
            name: "empty".to_string(),
            dist_tags: BTreeMap::new(),
            versions: BTreeMap::new(),
        };
        assert!(prefetch_child_specs(&packument, "empty", Some("^1.0.0")).is_empty());
    }

    #[test]
    fn prefetch_child_specs_handles_no_version_spec_as_latest() {
        let mut meta = version_meta("pkg", "2.0.0");
        meta.dependencies = BTreeMap::from([("child".to_string(), "^1.0.0".to_string())]);

        let mut versions = BTreeMap::new();
        versions.insert("2.0.0".to_string(), meta);

        let packument = Packument {
            name: "pkg".to_string(),
            dist_tags: BTreeMap::from([("latest".to_string(), "2.0.0".to_string())]),
            versions,
        };
        let children = prefetch_child_specs(&packument, "pkg", None);
        assert_eq!(children.len(), 1);
        assert_eq!(children[0], ("child".to_string(), "^1.0.0".to_string()));
    }

    #[test]
    fn prefetch_batch_closure_fetches_root_children_and_grandchildren() {
        // A three-level chain: root -> a -> b, root -> c -> d.
        // Each BFS level discovers unique children so dedup never
        // short-circuits the expansion.  Batch closure must discover
        // and prefetch all 4 packages across 3 BFS levels.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let registry = format!("http://{address}");
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_count = Arc::clone(&count);
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_shutdown = Arc::clone(&shutdown);
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let responses: BTreeMap<&str, &str> = BTreeMap::from([
                (
                    "a",
                    r#"{"name":"a","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"a","version":"1.0.0","dependencies":{"b":"^1.0.0"},"dist":{"tarball":"/a.tgz","integrity":"sha512-61000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#,
                ),
                (
                    "c",
                    r#"{"name":"c","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"c","version":"1.0.0","dependencies":{"d":"^1.0.0"},"dist":{"tarball":"/c.tgz","integrity":"sha512-63000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#,
                ),
                (
                    "b",
                    r#"{"name":"b","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"b","version":"1.0.0","dist":{"tarball":"/b.tgz","integrity":"sha512-62000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#,
                ),
                (
                    "d",
                    r#"{"name":"d","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"d","version":"1.0.0","dist":{"tarball":"/d.tgz","integrity":"sha512-64000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#,
                ),
            ]);
            while !server_shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    Err(_) => break,
                };
                server_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(10));
                let mut request = [0_u8; 2048];
                let _ = stream.read(&mut request);
                let request_str = String::from_utf8_lossy(&request);
                let path = request_str
                    .lines()
                    .next()
                    .and_then(|line| {
                        line.strip_prefix("GET /")
                            .and_then(|rest| rest.split(' ').next())
                    })
                    .unwrap_or("");
                let body = responses.get(path).copied().unwrap_or("{}");
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });

        let config = NpmConfig::default()
            .with_registry_override(&registry)
            .unwrap();
        let client = RegistryClient::new(config).with_prefetch(4);

        let mut root_deps = BTreeMap::new();
        root_deps.insert("a".to_string(), "*".to_string());
        root_deps.insert("c".to_string(), "*".to_string());

        // Reset the per-client batch counter before measuring.
        let _ = client.take_diagnostics();

        let total = client
            .prefetch_batch_closure(&root_deps, 3)
            .expect("batch closure should succeed");

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().unwrap();

        // Must have fetched 4 unique packuments (a, c, b, d):
        //   Level 0: a, c
        //   Level 1: b (child of a), d (child of c)
        //   Level 2: b, d have no further registry children
        assert_eq!(total, 4, "batch closure should prefetch 4 packuments");

        // The per-client batch counter must reflect the same total.
        let diag = client.take_diagnostics();
        assert_eq!(
            diag.batch_prefetch_fetches,
            4,
            "batch counter should match total ({batch_count} != 4)",
            batch_count = diag.batch_prefetch_fetches
        );

        // Exactly 4 server requests: one per unique packument.
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "must issue exactly 4 requests for the 4 unique packages"
        );
    }

    #[test]
    fn prefetch_batch_closure_skips_non_registry_specs() {
        // Workspace and file specs must be filtered out so the batch
        // prefetch never issues requests for non-registry sources.
        let config = NpmConfig::default();
        let client = RegistryClient::new(config).with_prefetch(2);

        let mut root_deps = BTreeMap::new();
        root_deps.insert("workspace-pkg".to_string(), "workspace:*".to_string());
        root_deps.insert("file-pkg".to_string(), "file:./local".to_string());
        root_deps.insert("registry-pkg".to_string(), "^1.0.0".to_string());

        // This will fail for "registry-pkg" since no server is listening,
        // proving the non-registry specs were filtered out (they would
        // have been enqueued and failed with a different error).
        let result = client.prefetch_batch_closure(&root_deps, 1);
        assert!(
            result.is_err(),
            "batch closure should fail on registry-pkg (no server), \
             proving workspace:/file: were skipped"
        );
    }

    #[test]
    fn prefetch_batch_closure_is_no_op_with_zero_depth() {
        let config = NpmConfig::default();
        let client = RegistryClient::new(config).with_prefetch(2);
        let mut root_deps = BTreeMap::new();
        root_deps.insert("anything".to_string(), "*".to_string());
        let total = client
            .prefetch_batch_closure(&root_deps, 0)
            .expect("zero-depth batch closure should succeed as a no-op");
        assert_eq!(total, 0, "zero depth must fetch nothing");
    }

    #[test]
    fn prefetch_batch_closure_returns_zero_without_prefetch_pool() {
        // When prefetch_workers == 0 and network is blocked, the
        // batch closure should short-circuit to a no-op.
        let config = NpmConfig::default();
        let cache = Arc::new(MetadataCache::open_in_memory().unwrap());
        // Offline + no prefetch workers = early return Ok(0).
        let client = RegistryClient::new(config).with_metadata_cache(cache, CacheMode::Offline);
        let mut root_deps = BTreeMap::new();
        root_deps.insert("pkg".to_string(), "*".to_string());
        let total = client
            .prefetch_batch_closure(&root_deps, 3)
            .expect("offline no-prefetch batch closure should return 0");
        assert_eq!(total, 0, "must be a no-op when no workers and offline");
    }

    #[test]
    fn offline_mode_errors_on_a_cache_miss_without_network_contact() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let registry = format!("http://{address}");
        // Drop the listener so nothing is listening; offline mode must never
        // attempt a connection anyway.
        drop(listener);

        let config = NpmConfig::default()
            .with_registry_override(&registry)
            .unwrap();
        let cache = Arc::new(MetadataCache::open_in_memory().unwrap());
        let client = RegistryClient::with_client(config.clone(), HttpClient::new(config.clone()))
            .with_metadata_cache(cache, CacheMode::Offline);

        let error = client.resolve(&parse_spec("absent").unwrap()).unwrap_err();
        assert!(
            matches!(error, RegistryError::OfflineMiss { .. }),
            "expected OfflineMiss, got {error:?}"
        );
    }

    #[test]
    fn concurrent_clients_have_independent_diagnostics() {
        // Two clients, each with its own local server, must have completely
        // independent diagnostics: draining one must never reset the other.
        let listener_a = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr_a = listener_a.local_addr().unwrap();
        let listener_b = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr_b = listener_b.local_addr().unwrap();
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Start two servers that each serve the same packument data.
        let body = r#"{"name":"pkg","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"pkg","version":"1.0.0","dist":{"tarball":"/pkg.tgz","integrity":"sha512-7iaw3Ur350mqGo7jwQrpkj9hiYB3Lkc/iBml1JQODbJ6wYX4oOHV+E+IvIh/1nsUNzLDBMxfqa2Ob1f1ACio/w=="},"dependencies":{}}}}"#;

        let shutdown_a = std::sync::Arc::clone(&shutdown);
        let body_a = body.to_owned();
        let server_a = std::thread::spawn(move || {
            let listener = listener_a;
            listener.set_nonblocking(true).unwrap();
            while !shutdown_a.load(std::sync::atomic::Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(c) => c,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                stream.set_nonblocking(false).ok();
                let mut request = [0u8; 2048];
                let _ = std::io::Read::read(&mut stream, &mut request);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body_a.len(),
                    body_a
                );
                let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
            }
        });

        let shutdown_b = std::sync::Arc::clone(&shutdown);
        let body_b = body.to_owned();
        let server_b = std::thread::spawn(move || {
            let listener = listener_b;
            listener.set_nonblocking(true).unwrap();
            while !shutdown_b.load(std::sync::atomic::Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(c) => c,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                stream.set_nonblocking(false).ok();
                let mut request = [0u8; 2048];
                let _ = std::io::Read::read(&mut stream, &mut request);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body_b.len(),
                    body_b
                );
                let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
            }
        });

        // Client A points to server A, client B points to server B.
        let config_a = NpmConfig::default()
            .with_registry_override(&format!("http://{addr_a}"))
            .unwrap();
        let config_b = NpmConfig::default()
            .with_registry_override(&format!("http://{addr_b}"))
            .unwrap();
        let client_a = RegistryClient::new(config_a);
        let client_b = RegistryClient::new(config_b);

        // Drain both to reset.
        let _ = client_a.take_diagnostics();
        let _ = client_b.take_diagnostics();

        // Resolve on client A only.
        let spec = parse_spec("pkg").unwrap();
        let _ = client_a.resolve(&spec).unwrap();

        // Client A must have some diagnostics; client B must have zero.
        let diag_a = client_a.take_diagnostics();
        let diag_b = client_b.take_diagnostics();
        assert!(
            diag_a.inline_fetches >= 1,
            "client A should have performed fetches"
        );
        assert_eq!(
            diag_b,
            ResolverDiagnosticsSnapshot::default(),
            "client B's diagnostics must remain untouched by client A's work"
        );

        // Now resolve on client B and prove A's second drain returns zero.
        let _ = client_b.resolve(&spec).unwrap();
        let diag_a2 = client_a.take_diagnostics();
        let diag_b2 = client_b.take_diagnostics();
        assert_eq!(
            diag_a2,
            ResolverDiagnosticsSnapshot::default(),
            "second drain on client A must return zero"
        );
        assert!(
            diag_b2.inline_fetches >= 1,
            "client B should have performed fetches"
        );

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = server_a.join();
        let _ = server_b.join();
    }
}
