//! Non-blocking dependency graph resolution that does not stall the calling
//! thread on inline registry fetches.
//!
//! Instead of calling blocking HTTP I/O, this module uses `reqwest::Client`
//! (async) and `tokio` tasks to issue multiple packument fetches concurrently
//! while the resolution algorithm processes already-available metadata.
//!
//! ## Design (post-unification)
//!
//! The async resolver shares the deterministic placement algorithm with the
//! blocking resolver by using the same helper functions (`resolver::parent_path`,
//! `resolver::merged_dependencies`, `resolver::request_matches`,
//! `resolver::registry_request`, `resolver::looks_like_registry_spec`) and
//! the same `Node` type (`resolver::placement::Node`).  The async
//! `GraphResolver` (`AsyncGraphResolver`) mirrors
//! `resolver::placement::GraphResolver`'s structure but uses `.await` on
//! `AsyncRegistryClient` instead of synchronous I/O.

// ── Imports ──────────────────────────────────────────────────────────────

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use semver::Version;
use thiserror::Error;

use crate::integrity::Integrity;
use crate::lockfile::{
    LockDependency, LockSource, Lockfile, PackageEntry, PackageResolution, RootEntry,
    RootResolution,
};
use crate::manifest::PackageManifest;
use crate::registry::{
    self, parse_spec, resolve_packument, PackageSpec, Packument, RegistryError, VersionMetadata,
    VersionRequest,
};
use crate::resolver;
use crate::resolver::model::*;
use crate::resolver::overrides::OverrideSet;
use crate::resolver::peer::{PeerMode, VisibleProviders};
use crate::resolver::platform::{self, check_package_platform, PackageReachability};
use crate::resolver::workspaces::WorkspaceIndex;
use crate::resolver::DependencySource;
use crate::resolver::{ResolveSink, ResolvedDownloadUnit};

// ── Public types ────────────────────────────────────────────────────────

/// Errors that can occur during async resolution.
#[derive(Debug, Error)]
pub enum AsyncResolveError {
    #[error("registry resolution failed for {package}@{spec}: {source}")]
    Registry {
        package: String,
        spec: String,
        #[source]
        source: Box<RegistryError>,
    },
    #[error("HTTP request failed for {url}: {message}")]
    Http { url: String, message: String },
    #[error("package {package}@{version} is incompatible with the current platform")]
    Platform { package: String, version: String },
    #[error("invalid dependency range {package}@{spec}: {reason}")]
    InvalidRange {
        package: String,
        spec: String,
        reason: String,
    },
    #[error("root override validation failed: {0}")]
    Override(String),
    #[error("peer dependency conflict: {0}")]
    Peer(String),
    #[error("non-registry source resolution failed for {package}@{spec}: {reason}")]
    Source {
        package: String,
        spec: String,
        reason: String,
    },
    #[error("dependency placement conflict at {path}: {package}@{requested} cannot replace selected {selected}")]
    PlacementConflict {
        path: String,
        package: String,
        requested: String,
        selected: String,
    },
}

// ── Async HTTP helpers ──────────────────────────────────────────────────

/// Build a fully-configured async `reqwest::Client` mirroring the sync
/// `HttpClient` configuration (user-agent, timeout, auth token handling).
fn build_async_client(config: &NpmConfig) -> reqwest::Client {
    let timeout = config.network.fetch_timeout;
    reqwest::Client::builder()
        .user_agent(concat!("bpm/", env!("CARGO_PKG_VERSION"), " (async)"))
        .timeout(timeout)
        .build()
        .expect("valid reqwest async client with defaults")
}

use crate::config::NpmConfig;
use crate::http::redact_url;
use crate::metadata_cache::{CacheMode, MetadataCache};
use crate::registry::{version_metadata, WireVersionMetadata, ABBREV_ACCEPT};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;

async fn async_fetch_url_with_validators(
    client: &reqwest::Client,
    url: &str,
    config: &NpmConfig,
    send_abbreviated_accept: bool,
    etag: Option<String>,
    last_modified: Option<String>,
) -> Result<String, AsyncResolveError> {
    let mut request = client.get(url);
    if send_abbreviated_accept {
        request = request.header("Accept", ABBREV_ACCEPT);
    }
    if let Some(token) = config.auth_token_for_url(url) {
        request = request.bearer_auth(token);
    }
    if let Some(ref etag) = etag {
        request = request.header("If-None-Match", etag);
    }
    if let Some(ref lm) = last_modified {
        request = request.header("If-Modified-Since", lm);
    }

    let response = request.send().await.map_err(|e| AsyncResolveError::Http {
        url: redact_url(url),
        message: e.to_string(),
    })?;

    let status = response.status().as_u16();
    if status == 304 {
        return Ok(String::new());
    }
    if status >= 400 {
        return Err(AsyncResolveError::Http {
            url: redact_url(url),
            message: format!("HTTP {status}"),
        });
    }

    response.text().await.map_err(|e| AsyncResolveError::Http {
        url: redact_url(url),
        message: format!("body read failed: {e}"),
    })
}

async fn async_fetch_url(
    client: &reqwest::Client,
    url: &str,
    config: &NpmConfig,
    send_abbreviated_accept: bool,
) -> Result<String, AsyncResolveError> {
    let mut request = client.get(url);
    if send_abbreviated_accept {
        request = request.header("Accept", ABBREV_ACCEPT);
    }
    if let Some(token) = config.auth_token_for_url(url) {
        request = request.bearer_auth(token);
    }

    let response = request.send().await.map_err(|e| AsyncResolveError::Http {
        url: redact_url(url),
        message: e.to_string(),
    })?;

    let status = response.status().as_u16();
    if status >= 400 {
        return Err(AsyncResolveError::Http {
            url: redact_url(url),
            message: format!("HTTP {status}"),
        });
    }

    response.text().await.map_err(|e| AsyncResolveError::Http {
        url: redact_url(url),
        message: format!("body read failed: {e}"),
    })
}

async fn async_cache_get(
    cache: Option<&Arc<MetadataCache>>,
    url: &str,
    mode: CacheMode,
) -> Result<Option<(String, Option<String>, Option<String>)>, AsyncResolveError> {
    let Some(cache) = cache else { return Ok(None) };
    let url_owned = url.to_owned();
    let url_for_display = url.to_owned();
    let cache = Arc::clone(cache);
    let result = tokio::task::spawn_blocking(move || cache.get(&url_owned)).await;
    match result {
        Ok(Ok(Some(entry))) => {
            let body = String::from_utf8_lossy(&entry.body).into_owned();
            if !mode.allows_network() || mode.serves_stale() {
                return Ok(Some((body, entry.etag, entry.last_modified)));
            }
            Ok(Some((body, entry.etag, entry.last_modified)))
        }
        Ok(Ok(None)) => {
            if !mode.allows_network() {
                return Err(AsyncResolveError::Http {
                    url: redact_url(&url_for_display),
                    message: format!("offline miss: no cached metadata for {url_for_display}"),
                });
            }
            Ok(None)
        }
        Ok(Err(_)) => Ok(None),
        Err(_) => Ok(None),
    }
}

#[allow(dead_code)]
async fn async_cache_put(
    cache: Option<&Arc<MetadataCache>>,
    url: &str,
    body: &str,
    etag: Option<String>,
    last_modified: Option<String>,
) {
    let Some(cache) = cache else { return };
    let url = url.to_owned();
    let body = body.to_owned();
    let cache = Arc::clone(cache);
    let _ = tokio::task::spawn_blocking(move || {
        cache.put(
            &url,
            body.as_bytes(),
            etag.as_deref(),
            last_modified.as_deref(),
        )
    })
    .await;
}

fn encode_package_name(name: &str) -> String {
    name.replace('/', "%2F")
}

async fn async_fetch_packument(
    client: &reqwest::Client,
    name: &str,
    config: &NpmConfig,
    registry: &str,
    cache: Option<&Arc<MetadataCache>>,
    mode: CacheMode,
    fetch_bytes: &Arc<AtomicU64>,
) -> Result<Packument, AsyncResolveError> {
    let base = registry.trim_end_matches('/');
    let encoded = encode_package_name(name);
    let url = format!("{base}/{encoded}");

    if let Some((body, etag, last_modified)) = async_cache_get(cache, &url, mode).await? {
        if !mode.allows_network() || mode.serves_stale() {
            return serde_json::from_str(&body).map_err(|source| AsyncResolveError::Registry {
                package: name.to_string(),
                spec: "latest".to_string(),
                source: Box::new(RegistryError::BadJson {
                    package: name.to_string(),
                    source,
                }),
            });
        }
        let body = async_fetch_url_with_validators(client, &url, config, true, etag, last_modified)
            .await?;
        fetch_bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
        return serde_json::from_str(&body).map_err(|source| AsyncResolveError::Registry {
            package: name.to_string(),
            spec: "latest".to_string(),
            source: Box::new(RegistryError::BadJson {
                package: name.to_string(),
                source,
            }),
        });
    }

    let body = async_fetch_url(client, &url, config, true).await?;
    fetch_bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
    serde_json::from_str(&body).map_err(|source| AsyncResolveError::Registry {
        package: name.to_string(),
        spec: "latest".to_string(),
        source: Box::new(RegistryError::BadJson {
            package: name.to_string(),
            source,
        }),
    })
}

#[allow(clippy::too_many_arguments)]
async fn async_fetch_version_packument(
    client: &reqwest::Client,
    name: &str,
    version: &Version,
    config: &NpmConfig,
    registry: &str,
    cache: Option<&Arc<MetadataCache>>,
    mode: CacheMode,
    fetch_bytes: &Arc<AtomicU64>,
) -> Result<Packument, AsyncResolveError> {
    let base = registry.trim_end_matches('/');
    let encoded = encode_package_name(name);
    let url = format!("{base}/{encoded}/{version}");

    if let Some((body, _, _)) = async_cache_get(cache, &url, mode).await? {
        if !mode.allows_network() || mode.serves_stale() {
            let wire: WireVersionMetadata =
                serde_json::from_str(&body).map_err(|source| AsyncResolveError::Registry {
                    package: name.to_string(),
                    spec: version.to_string(),
                    source: Box::new(RegistryError::BadJson {
                        package: name.to_string(),
                        source,
                    }),
                })?;
            let metadata = version_metadata(name, &version.to_string(), wire).ok_or_else(|| {
                AsyncResolveError::Registry {
                    package: name.to_string(),
                    spec: version.to_string(),
                    source: Box::new(RegistryError::NoVersions {
                        package: name.to_string(),
                    }),
                }
            })?;
            return Ok(Packument {
                name: name.to_string(),
                dist_tags: BTreeMap::new(),
                versions: BTreeMap::from([(version.to_string(), metadata)]),
            });
        }
    }

    let body = async_fetch_url(client, &url, config, false).await?;
    fetch_bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
    let wire: WireVersionMetadata =
        serde_json::from_str(&body).map_err(|source| AsyncResolveError::Registry {
            package: name.to_string(),
            spec: version.to_string(),
            source: Box::new(RegistryError::BadJson {
                package: name.to_string(),
                source,
            }),
        })?;
    let metadata = version_metadata(name, &version.to_string(), wire).ok_or_else(|| {
        AsyncResolveError::Registry {
            package: name.to_string(),
            spec: version.to_string(),
            source: Box::new(RegistryError::NoVersions {
                package: name.to_string(),
            }),
        }
    })?;
    Ok(Packument {
        name: name.to_string(),
        dist_tags: BTreeMap::new(),
        versions: BTreeMap::from([(version.to_string(), metadata)]),
    })
}

// ── AsyncRegistryClient ─────────────────────────────────────────────────

#[derive(Clone)]
pub struct AsyncRegistryClient {
    config: NpmConfig,
    http: reqwest::Client,
    packument_cache: Arc<AsyncMutex<BTreeMap<String, Packument>>>,
    fetch_bytes: Arc<AtomicU64>,
    inline_fetches: Arc<AtomicU64>,
    cache_hits: Arc<AtomicU64>,
    metadata_cache: Option<Arc<MetadataCache>>,
    cache_mode: CacheMode,
    max_in_flight: u32,
    peak_in_flight: Arc<AtomicU64>,
    #[allow(dead_code)]
    in_flight: Arc<AtomicU64>,
}

impl AsyncRegistryClient {
    pub fn new(config: NpmConfig) -> Self {
        let http = build_async_client(&config);
        Self {
            config,
            http,
            packument_cache: Arc::new(AsyncMutex::new(BTreeMap::new())),
            fetch_bytes: Arc::new(AtomicU64::new(0)),
            inline_fetches: Arc::new(AtomicU64::new(0)),
            cache_hits: Arc::new(AtomicU64::new(0)),
            metadata_cache: None,
            cache_mode: CacheMode::Default,
            max_in_flight: 4,
            peak_in_flight: Arc::new(AtomicU64::new(0)),
            in_flight: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn with_max_in_flight(mut self, max: u32) -> Self {
        self.max_in_flight = max.max(1);
        self
    }

    pub fn peak_in_flight(&self) -> u64 {
        self.peak_in_flight.load(Ordering::Relaxed)
    }

    pub fn with_metadata_cache(mut self, cache: Arc<MetadataCache>, cache_mode: CacheMode) -> Self {
        self.metadata_cache = Some(cache);
        self.cache_mode = cache_mode;
        self
    }

    pub fn registry_for_package(&self, package: &str) -> &str {
        self.config.registry_for_package(package)
    }

    pub async fn resolve(
        &self,
        spec: &PackageSpec,
    ) -> Result<registry::ResolvedArtifact, AsyncResolveError> {
        let registry_url = self.config.registry_for_package(&spec.name);
        let packument = self.packument_for(spec).await?;
        resolve_packument(spec, &packument, registry_url).map_err(|source| {
            AsyncResolveError::Registry {
                package: spec.name.clone(),
                spec: spec.name.clone(),
                source: Box::new(source),
            }
        })
    }

    pub async fn packument_for(&self, spec: &PackageSpec) -> Result<Packument, AsyncResolveError> {
        match &spec.req {
            VersionRequest::Exact(version) => {
                let registry_url = self.config.registry_for_package(&spec.name);
                async_fetch_version_packument(
                    &self.http,
                    &spec.name,
                    version,
                    &self.config,
                    registry_url,
                    self.metadata_cache.as_ref(),
                    self.cache_mode,
                    &self.fetch_bytes,
                )
                .await
            }
            VersionRequest::Latest | VersionRequest::Range(_) => self.packument(&spec.name).await,
        }
    }

    pub async fn packument(&self, name: &str) -> Result<Packument, AsyncResolveError> {
        let registry_url = self.config.registry_for_package(name);
        let key = format!("{}\0{name}", registry_url.trim_end_matches('/'));

        {
            let cache = self.packument_cache.lock().await;
            if let Some(packument) = cache.get(&key) {
                self.cache_hits.fetch_add(1, Ordering::Relaxed);
                return Ok(packument.clone());
            }
        }

        self.inline_fetches.fetch_add(1, Ordering::Relaxed);
        let packument = async_fetch_packument(
            &self.http,
            name,
            &self.config,
            registry_url,
            self.metadata_cache.as_ref(),
            self.cache_mode,
            &self.fetch_bytes,
        )
        .await?;

        let mut cache = self.packument_cache.lock().await;
        cache.entry(key).or_insert_with(|| packument.clone());
        Ok(packument)
    }

    pub fn take_diagnostics(&self) -> (u64, u64, u64) {
        (
            self.cache_hits.swap(0, Ordering::Relaxed),
            self.inline_fetches.swap(0, Ordering::Relaxed),
            self.fetch_bytes.swap(0, Ordering::Relaxed),
        )
    }
}

// ── AsyncGraphResolver ──────────────────────────────────────────────────

struct AsyncNode {
    path: String,
    placement_name: String,
    metadata: VersionMetadata,
    resolved: String,
    integrity: String,
    dependencies: BTreeMap<String, String>,
    targets: BTreeMap<String, String>,
    optional: bool,
    dev: bool,
    peer_context: BTreeMap<String, crate::lockfile::PeerProvider>,
    source: LockSource,
    link: bool,
    workspace_target: Option<String>,
    source_dir: Option<PathBuf>,
}

struct AsyncGraphResolver<'a> {
    registry: &'a AsyncRegistryClient,
    overrides: OverrideSet,
    nodes: BTreeMap<String, AsyncNode>,
    diagnostics: Vec<String>,
    workspace: Option<&'a WorkspaceIndex>,
    root_dir: Option<PathBuf>,
    target: TargetPlatform,
    sink: Option<&'a dyn ResolveSink>,
}

impl<'a> AsyncGraphResolver<'a> {
    fn new(
        registry: &'a AsyncRegistryClient,
        overrides: OverrideSet,
        workspace: Option<&'a WorkspaceIndex>,
        root_dir: Option<PathBuf>,
        target: TargetPlatform,
        sink: Option<&'a dyn ResolveSink>,
    ) -> Self {
        Self {
            registry,
            overrides,
            nodes: BTreeMap::new(),
            diagnostics: Vec::new(),
            workspace,
            root_dir,
            target,
            sink,
        }
    }

    fn resolve_dependency<'s>(
        &'s mut self,
        parent: &'s str,
        name: &'s str,
        requested: &'s str,
        optional: bool,
        dev: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<String>, AsyncResolveError>> + 's>,
    > {
        Box::pin(async move {
            let ancestors = self.ancestor_chain(parent);
            let spec = self
                .overrides
                .effective_spec_for(name, requested, &ancestors)
                .to_owned();

            // ── Workspace dependency ────────────────────────────────────────
            if let Some(workspace) = self.workspace {
                if let crate::resolver::workspaces::WorkspaceResolution::Link(edge) = workspace
                    .resolve(name, &spec)
                    .map_err(|e| AsyncResolveError::Peer(e.to_string()))?
                {
                    let relative_path = match &edge.target.source {
                        PackageSource::Workspace { relative_path } => relative_path.clone(),
                        _ => unreachable!(),
                    };
                    let path = format!("node_modules/{name}");
                    if self.nodes.contains_key(&path) {
                        self.upgrade_reachability(&path, optional, dev);
                        return Ok(Some(path));
                    }
                    let metadata = resolver::workspace_metadata(
                        name,
                        &edge.target.version,
                        workspace.get(name).and_then(|w| w.manifest.as_ref()),
                    );
                    if !self.platform_allows(name, &metadata, optional)? {
                        return Ok(None);
                    }
                    let dependencies = resolver::merged_dependencies(&metadata);
                    self.nodes.insert(
                        path.clone(),
                        AsyncNode {
                            path: path.clone(),
                            placement_name: name.to_owned(),
                            metadata,
                            resolved: String::new(),
                            integrity: String::new(),
                            dependencies: dependencies.clone(),
                            targets: BTreeMap::new(),
                            optional,
                            dev,
                            peer_context: BTreeMap::new(),
                            source: LockSource::Workspace {
                                relative_path: relative_path.clone(),
                            },
                            link: true,
                            workspace_target: Some(relative_path),
                            source_dir: workspace
                                .get(name)
                                .and_then(|w| w.manifest.as_ref())
                                .and_then(|m| m.source_dir.clone()),
                        },
                    );
                    self.resolve_children(&path, &dependencies, optional, dev)
                        .await?;
                    return Ok(Some(path));
                }
            }

            // ── Source dependency (file:, git:, tarball) ────────────────────
            if let Some(source) = DependencySource::parse(&spec) {
                return self
                    .resolve_source_dependency(parent, name, &spec, source, optional, dev)
                    .await;
            }

            // ── Check if already visible in parent ──────────────────────────
            let (_, visible_spec) = resolver::registry_request(name, &spec);
            if let Some(path) = self.find_visible(parent, name, &visible_spec) {
                self.upgrade_reachability(&path, optional, dev);
                return Ok(Some(path));
            }

            // ── Build the placement path ────────────────────────────────────
            let path = if parent.is_empty() {
                format!("node_modules/{name}")
            } else {
                format!("{parent}/node_modules/{name}")
            };
            if self.nodes.contains_key(&path) {
                let selected = self.nodes.get(&path).expect("checked above");
                if resolver::request_matches(&visible_spec, &selected.metadata.version) {
                    return Ok(Some(path));
                }
                return Err(AsyncResolveError::PlacementConflict {
                    path,
                    package: name.to_owned(),
                    requested: spec,
                    selected: selected.metadata.version.to_string(),
                });
            }

            // ── Registry resolution (async) ─────────────────────────────────
            let (registry_name, registry_spec) = resolver::registry_request(name, &spec);
            let parsed =
                parse_spec(&format!("{registry_name}@{registry_spec}")).map_err(|source| {
                    AsyncResolveError::Registry {
                        package: name.to_owned(),
                        spec: spec.clone(),
                        source: Box::new(source),
                    }
                })?;
            let registry_base = self
                .registry
                .registry_for_package(&registry_name)
                .to_owned();
            let packument = self
                .registry
                .packument_for(&parsed)
                .await
                .map_err(|source| AsyncResolveError::Registry {
                    package: name.to_owned(),
                    spec: spec.clone(),
                    source: match source {
                        AsyncResolveError::Registry {
                            package: _,
                            spec: _,
                            source,
                        } => source,
                        other => Box::new(RegistryError::Network {
                            package: name.to_owned(),
                            source: other.to_string().into(),
                        }),
                    },
                })?;
            let mut resolved =
                resolve_packument(&parsed, &packument, &registry_base).map_err(|source| {
                    AsyncResolveError::Registry {
                        package: name.to_owned(),
                        spec: spec.clone(),
                        source: Box::new(source),
                    }
                })?;

            // ── Peer backtracking ───────────────────────────────────────────
            if !self.peer_candidate_matches(&resolved.metadata, parent) {
                let mut versions: Vec<Version> = packument
                    .versions
                    .keys()
                    .filter_map(|k| Version::parse(k).ok())
                    .collect();
                versions.sort();
                versions.reverse();
                for version in versions {
                    let exact = PackageSpec {
                        name: registry_name.to_string(),
                        req: VersionRequest::Exact(version),
                    };
                    let candidate = resolve_packument(&exact, &packument, &registry_base).map_err(
                        |source| AsyncResolveError::Registry {
                            package: name.to_owned(),
                            spec: spec.clone(),
                            source: Box::new(source),
                        },
                    )?;
                    if self.peer_candidate_matches(&candidate.metadata, parent) {
                        resolved = candidate;
                        break;
                    }
                }
            }

            // ── Platform check ──────────────────────────────────────────────
            if !self.platform_allows(name, &resolved.metadata, optional)? {
                return Ok(None);
            }

            // ── Place the node ──────────────────────────────────────────────
            let dependencies = resolver::merged_dependencies(&resolved.metadata);
            self.nodes.insert(
                path.clone(),
                AsyncNode {
                    path: path.clone(),
                    placement_name: name.to_owned(),
                    metadata: resolved.metadata,
                    resolved: resolved.tarball_url,
                    integrity: resolved.integrity,
                    dependencies: dependencies.clone(),
                    targets: BTreeMap::new(),
                    optional,
                    dev,
                    peer_context: BTreeMap::new(),
                    source: LockSource::Registry {
                        registry: registry_base,
                    },
                    link: false,
                    workspace_target: None,
                    source_dir: None,
                },
            );

            // ── Announce to sink ────────────────────────────────────────────
            self.announce(&path);

            // ── Recurse into children ───────────────────────────────────────
            self.resolve_children(&path, &dependencies, optional, dev)
                .await?;
            Ok(Some(path))
        })
    }

    async fn resolve_children(
        &mut self,
        parent_path: &str,
        dependencies: &BTreeMap<String, String>,
        optional: bool,
        dev: bool,
    ) -> Result<(), AsyncResolveError> {
        for (child, child_spec) in dependencies {
            let child_optional = self.nodes.get(parent_path).is_some_and(|node| {
                optional || node.metadata.optional_dependencies.contains_key(child)
            });
            if let Some(target) = self
                .resolve_dependency(parent_path, child, child_spec, child_optional, dev)
                .await?
            {
                if let Some(node) = self.nodes.get_mut(parent_path) {
                    node.targets.insert(child.clone(), target);
                }
            }
        }
        Ok(())
    }

    async fn resolve_source_dependency(
        &mut self,
        parent: &str,
        name: &str,
        spec: &str,
        source: DependencySource,
        optional: bool,
        dev: bool,
    ) -> Result<Option<String>, AsyncResolveError> {
        let path = if parent.is_empty() {
            format!("node_modules/{name}")
        } else {
            format!("{parent}/node_modules/{name}")
        };
        if self.nodes.contains_key(&path) {
            self.upgrade_reachability(&path, optional, dev);
            return Ok(Some(path));
        }
        let base_dir = self.base_dir_for(parent);
        let source_res = source
            .resolve(&base_dir)
            .map_err(|reason| AsyncResolveError::Source {
                package: name.to_owned(),
                spec: spec.to_owned(),
                reason,
            })?;
        let metadata = source_res.metadata;
        if !self.platform_allows(name, &metadata, optional)? {
            return Ok(None);
        }
        let dependencies = resolver::merged_dependencies(&metadata);
        self.nodes.insert(
            path.clone(),
            AsyncNode {
                path: path.clone(),
                placement_name: name.to_owned(),
                metadata,
                resolved: source_res.resolved.clone(),
                integrity: source_res.integrity.clone().unwrap_or_default(),
                dependencies: dependencies.clone(),
                targets: BTreeMap::new(),
                optional,
                dev,
                peer_context: BTreeMap::new(),
                source: source_res.source,
                link: source_res.link,
                workspace_target: source_res.workspace_target,
                source_dir: source_res.source_dir,
            },
        );
        self.announce(&path);
        self.resolve_children(&path, &dependencies, optional, dev)
            .await?;
        Ok(Some(path))
    }

    fn announce(&self, path: &str) {
        let Some(sink) = self.sink else { return };
        let unit = {
            let Some(node) = self.nodes.get(path) else {
                return;
            };
            if node.link || node.resolved.is_empty() {
                return;
            }
            ResolvedDownloadUnit {
                path: node.path.clone(),
                name: node.placement_name.clone(),
                url: node.resolved.clone(),
                integrity: if node.integrity.is_empty() {
                    None
                } else {
                    Some(Integrity::parse(&node.integrity).unwrap_or_else(|e| {
                        panic!(
                            "invalid integrity in resolved node for {}: {}: {}",
                            node.placement_name, node.integrity, e
                        )
                    }))
                },
            }
        };
        sink.emit(unit);
    }

    fn platform_allows(
        &mut self,
        name: &str,
        metadata: &VersionMetadata,
        optional: bool,
    ) -> Result<bool, AsyncResolveError> {
        let constraints = PlatformConstraints {
            os: metadata.os.iter().cloned().collect::<BTreeSet<_>>(),
            cpu: metadata.cpu.iter().cloned().collect::<BTreeSet<_>>(),
            libc: metadata.libc.iter().cloned().collect::<BTreeSet<_>>(),
        };
        let reachability = if optional {
            PackageReachability::OptionalOnly
        } else {
            PackageReachability::Required
        };
        match check_package_platform(
            &format!("{}@{}", name, metadata.version),
            &constraints,
            &self.target,
            reachability,
        ) {
            Ok(platform::PlatformDisposition::Compatible) => Ok(true),
            Ok(platform::PlatformDisposition::SkipOptional(diag)) => {
                self.diagnostics.push(diag.message);
                Ok(false)
            }
            Err(_) => Err(AsyncResolveError::Platform {
                package: name.to_owned(),
                version: metadata.version.to_string(),
            }),
        }
    }

    fn peer_candidate_matches(&self, metadata: &VersionMetadata, parent: &str) -> bool {
        metadata.peer_dependencies.iter().all(|(name, range)| {
            let Some(path) = self.find_visible_any(parent, name) else {
                return metadata
                    .peer_dependencies_meta
                    .get(name)
                    .is_some_and(|meta| meta.optional);
            };
            self.nodes.get(&path).is_some_and(|provider| {
                resolver::request_matches(range, &provider.metadata.version)
            })
        })
    }

    fn visible_providers(
        &self,
        parent: &str,
    ) -> BTreeMap<String, crate::resolver::peer::VisibleProvider> {
        let mut providers = BTreeMap::new();
        let mut candidate = parent.to_owned();
        loop {
            for node in self.nodes.values() {
                let expected = if candidate.is_empty() {
                    format!("node_modules/{}", node.placement_name)
                } else {
                    format!("{candidate}/node_modules/{}", node.placement_name)
                };
                if node.path == expected {
                    providers
                        .entry(node.metadata.name.clone())
                        .or_insert_with(|| crate::resolver::peer::VisibleProvider {
                            identity: ProviderIdentity {
                                name: node.metadata.name.clone(),
                                version: node.metadata.version.to_string(),
                                source: PackageSource::Registry {
                                    registry: self
                                        .registry
                                        .registry_for_package(&node.metadata.name)
                                        .to_owned(),
                                },
                            },
                            path: node.path.clone(),
                            competing_requester: None,
                        });
                }
            }
            if candidate.is_empty() {
                break;
            }
            candidate = candidate
                .rsplit_once("/node_modules/")
                .map(|(prefix, _)| prefix.to_owned())
                .unwrap_or_default();
        }
        providers
    }

    fn find_visible(&self, parent: &str, name: &str, spec: &str) -> Option<String> {
        let mut candidate = if parent.is_empty() {
            String::new()
        } else {
            parent.to_owned()
        };
        loop {
            let path = if candidate.is_empty() {
                format!("node_modules/{name}")
            } else {
                format!("{candidate}/node_modules/{name}")
            };
            if let Some(node) = self.nodes.get(&path) {
                if resolver::request_matches(spec, &node.metadata.version) {
                    return Some(path);
                }
            }
            if candidate.is_empty() {
                break;
            }
            candidate = candidate
                .rsplit_once("/node_modules/")
                .map(|(prefix, _)| prefix.to_owned())
                .unwrap_or_default();
        }
        None
    }

    fn find_visible_any(&self, parent: &str, name: &str) -> Option<String> {
        let mut candidate = parent.to_owned();
        loop {
            let path = if candidate.is_empty() {
                format!("node_modules/{name}")
            } else {
                format!("{candidate}/node_modules/{name}")
            };
            if self.nodes.contains_key(&path) {
                return Some(path);
            }
            if candidate.is_empty() {
                return None;
            }
            candidate = candidate
                .rsplit_once("/node_modules/")
                .map(|(prefix, _)| prefix.to_owned())
                .unwrap_or_default();
        }
    }

    fn ancestor_chain(&self, parent: &str) -> Vec<(String, Version)> {
        if parent.is_empty() {
            return Vec::new();
        }
        let mut paths = Vec::new();
        let mut current = parent.to_owned();
        loop {
            paths.push(current.clone());
            let next = resolver::parent_path(&current);
            if next.is_empty() {
                break;
            }
            current = next;
        }
        paths.reverse();
        paths
            .into_iter()
            .filter_map(|p| {
                let node = self.nodes.get(&p)?;
                Some((node.placement_name.clone(), node.metadata.version.clone()))
            })
            .collect()
    }

    fn base_dir_for(&self, parent: &str) -> PathBuf {
        if parent.is_empty() {
            return self.root_dir.clone().unwrap_or_else(|| PathBuf::from("."));
        }
        self.nodes
            .get(parent)
            .and_then(|node| node.source_dir.clone())
            .unwrap_or_else(|| self.root_dir.clone().unwrap_or_else(|| PathBuf::from(".")))
    }

    fn upgrade_reachability(&mut self, path: &str, optional: bool, dev: bool) {
        if let Some(node) = self.nodes.get_mut(path) {
            node.optional &= optional;
            node.dev &= dev;
        }
    }
}

// ── Public entry points ─────────────────────────────────────────────────

pub async fn resolve_manifest_async(
    manifest: &PackageManifest,
    registry: &AsyncRegistryClient,
    generator: &str,
) -> Result<Lockfile, AsyncResolveError> {
    resolve_manifest_with_options_and_target_async(
        manifest,
        registry,
        generator,
        None,
        PeerMode::Strict,
        crate::resolver::current_target_platform(),
    )
    .await
}

pub async fn resolve_manifest_with_workspaces_async(
    manifest: &PackageManifest,
    registry: &AsyncRegistryClient,
    generator: &str,
    workspace: Option<&WorkspaceIndex>,
) -> Result<Lockfile, AsyncResolveError> {
    resolve_manifest_with_options_and_target_async(
        manifest,
        registry,
        generator,
        workspace,
        PeerMode::Strict,
        crate::resolver::current_target_platform(),
    )
    .await
}

pub async fn resolve_manifest_with_workspaces_async_sink(
    manifest: &PackageManifest,
    registry: &AsyncRegistryClient,
    generator: &str,
    workspace: Option<&WorkspaceIndex>,
    sink: Option<&dyn ResolveSink>,
) -> Result<Lockfile, AsyncResolveError> {
    resolve_manifest_with_options_and_target_async_sink(
        manifest,
        registry,
        generator,
        workspace,
        PeerMode::Strict,
        crate::resolver::current_target_platform(),
        sink,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn resolve_manifest_with_options_and_target_async_sink(
    manifest: &PackageManifest,
    registry: &AsyncRegistryClient,
    generator: &str,
    workspace: Option<&WorkspaceIndex>,
    peer_mode: PeerMode,
    target: TargetPlatform,
    sink: Option<&dyn ResolveSink>,
) -> Result<Lockfile, AsyncResolveError> {
    let root_deps = manifest.root_dependency_declarations();
    let overrides = OverrideSet::from_manifest(
        &manifest.overrides,
        &root_deps,
        crate::resolver::overrides::OverrideOrigin::Root,
    )
    .map_err(|e| AsyncResolveError::Override(e.to_string()))?;
    let normalized_overrides = overrides.as_map().clone();

    let mut res = AsyncGraphResolver::new(
        registry,
        overrides,
        workspace,
        manifest.source_dir.clone(),
        target.clone(),
        sink,
    );

    // ── Prefetch root-level packuments (concurrent warmup) ──────────
    for (name, spec) in &root_deps {
        if let Ok(parsed) = parse_spec(&format!("{name}@{spec}")) {
            let _ = registry.packument_for(&parsed).await;
        }
    }

    // ── Resolve root dependencies ───────────────────────────────────────
    let mut root_targets: BTreeMap<String, (String, String)> = BTreeMap::new();
    for (name, spec) in &root_deps {
        let optional = manifest.optional_dependencies.contains_key(name);
        let dev = manifest.dev_dependencies.contains_key(name)
            && !manifest.dependencies.contains_key(name)
            && !manifest.optional_dependencies.contains_key(name);
        if let Some(path) = res
            .resolve_dependency("", name, spec, optional, dev)
            .await?
        {
            root_targets.insert(name.clone(), (spec.clone(), path));
        }
    }

    // ── Build the lockfile ──────────────────────────────────────────────
    let mut lock = Lockfile::new(generator);
    lock.root = RootEntry {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        dependencies: root_deps,
    };
    lock.resolution.root = RootResolution {
        dev_dependencies: manifest.dev_dependencies.clone(),
        optional_dependencies: manifest.optional_dependencies.clone(),
        overrides: normalized_overrides,
        target: Some(crate::lockfile::LockTarget {
            os: target.os.clone(),
            cpu: target.cpu.clone(),
            libc: target.libc.clone(),
        }),
        ..RootResolution::default()
    };
    lock.resolution.root.peer_mode = match peer_mode {
        PeerMode::Strict => crate::lockfile::PeerMode::Strict,
        PeerMode::LegacyIgnore => crate::lockfile::PeerMode::LegacyIgnore,
    };

    // ── Bind peer contexts ──────────────────────────────────────────────
    let node_paths: Vec<String> = res.nodes.keys().cloned().collect();
    for path in &node_paths {
        let (metadata, parent) = {
            let node = res.nodes.get(path).expect("node path exists");
            (node.metadata.clone(), resolver::parent_path(path))
        };
        let providers = res.visible_providers(&parent);
        let visible = VisibleProviders::new(std::iter::once(path.clone()), providers);
        let context = crate::resolver::peer::bind_peer_context(&metadata, &visible, peer_mode)
            .map_err(|e| AsyncResolveError::Peer(e.to_string()))?;
        let peer_context: BTreeMap<String, crate::lockfile::PeerProvider> = context
            .0
            .into_iter()
            .map(|(peer_name, provider)| {
                let provider_name = provider.name.clone();
                let provider_path = res
                    .find_visible_any(&parent, &provider_name)
                    .unwrap_or_default();
                let source = res
                    .nodes
                    .get(&provider_path)
                    .map(|n| n.source.clone())
                    .unwrap_or_else(|| LockSource::Registry {
                        registry: registry.registry_for_package(&provider_name).to_owned(),
                    });
                (
                    peer_name,
                    crate::lockfile::PeerProvider {
                        name: provider.name,
                        version: provider.version,
                        source,
                        path: provider_path,
                    },
                )
            })
            .collect();
        if let Some(node) = res.nodes.get_mut(path) {
            node.peer_context = peer_context;
        }
    }

    // ── Emit packages ───────────────────────────────────────────────────
    for node in res.nodes.values() {
        lock.packages.push(PackageEntry {
            path: node.path.clone(),
            name: node.placement_name.clone(),
            version: node.metadata.version.to_string(),
            resolved: node.resolved.clone(),
            workspace_target: node.workspace_target.clone(),
            integrity: Some(node.integrity.clone()),
            link: node.link,
            dev: node.dev,
            optional: node.optional,
            os: node.metadata.os.clone(),
            cpu: node.metadata.cpu.clone(),
            bin: node.metadata.bin.clone(),
            dependencies: node.dependencies.clone(),
        });
    }
    for node in res.nodes.values() {
        let mut dependencies = BTreeMap::new();
        for (dep_name, spec) in &node.dependencies {
            if let Some(target) = node.targets.get(dep_name) {
                dependencies.insert(
                    dep_name.clone(),
                    LockDependency {
                        spec: spec.clone(),
                        target: target.clone(),
                    },
                );
            }
        }
        lock.resolution.packages.insert(
            node.path.clone(),
            PackageResolution {
                source: node.source.clone(),
                dev_optional: node.dev || node.optional,
                dependencies,
                has_install_script: node.metadata.has_install_script,
                peer: !node.metadata.peer_dependencies.is_empty(),
                libc: Vec::new(),
                optional_dependencies: node
                    .metadata
                    .optional_dependencies
                    .iter()
                    .filter_map(|(name, spec)| {
                        node.targets.get(name).map(|target| {
                            (
                                name.clone(),
                                LockDependency {
                                    spec: spec.clone(),
                                    target: target.clone(),
                                },
                            )
                        })
                    })
                    .collect(),
                peer_dependencies: node
                    .metadata
                    .peer_dependencies
                    .iter()
                    .filter_map(|(name, spec)| {
                        node.peer_context.get(name).map(|provider| {
                            (
                                name.clone(),
                                LockDependency {
                                    spec: spec.clone(),
                                    target: provider.path.clone(),
                                },
                            )
                        })
                    })
                    .collect(),
                optional_peers: node
                    .metadata
                    .peer_dependencies_meta
                    .iter()
                    .filter(|(_, meta)| meta.optional)
                    .map(|(name, _)| name.clone())
                    .collect(),
                peer_context: node.peer_context.clone(),
                workspace_target: node.workspace_target.clone(),
            },
        );
    }
    lock.sort_packages();
    let _ = root_targets;
    Ok(lock)
}

#[allow(clippy::too_many_arguments)]
pub async fn resolve_manifest_with_options_and_target_async(
    manifest: &PackageManifest,
    registry: &AsyncRegistryClient,
    generator: &str,
    workspace: Option<&WorkspaceIndex>,
    peer_mode: PeerMode,
    target: TargetPlatform,
) -> Result<Lockfile, AsyncResolveError> {
    resolve_manifest_with_options_and_target_async_sink(
        manifest, registry, generator, workspace, peer_mode, target, None,
    )
    .await
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver;

    #[tokio::test]
    async fn test_looks_like_registry_spec() {
        assert!(resolver::looks_like_registry_spec("lodash"));
        assert!(resolver::looks_like_registry_spec("^4.0.0"));
        assert!(resolver::looks_like_registry_spec("@scope/pkg"));
        assert!(!resolver::looks_like_registry_spec("file:./local.tgz"));
        assert!(!resolver::looks_like_registry_spec(
            "https://registry.npmjs.org/pkg.tgz"
        ));
        assert!(!resolver::looks_like_registry_spec("workspace:*"));
    }

    #[tokio::test]
    async fn test_parent_path() {
        assert_eq!(resolver::parent_path("node_modules/foo"), "");
        assert_eq!(
            resolver::parent_path("node_modules/foo/node_modules/bar"),
            "node_modules/foo"
        );
        assert_eq!(resolver::parent_path(""), "");
    }

    #[tokio::test]
    async fn test_merged_deps() {
        let mut deps = BTreeMap::new();
        deps.insert("a".into(), "^1.0.0".into());
        deps.insert("b".into(), "^2.0.0".into());
        use crate::registry::Dist;
        let metadata = VersionMetadata {
            name: String::new(),
            version: Version::new(0, 0, 0),
            deprecated: None,
            dependencies: deps,
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
        };
        let merged = resolver::merged_dependencies(&metadata);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged.get("a").unwrap(), "^1.0.0");
    }

    #[tokio::test]
    async fn test_request_matches_spec() {
        let v173 = Version::parse("1.7.3").unwrap();
        assert!(resolver::request_matches("*", &v173));
        assert!(resolver::request_matches("latest", &v173));
        assert!(resolver::request_matches("1.7.3", &v173));
        assert!(!resolver::request_matches("2.0.0", &v173));
        assert!(resolver::request_matches("^1.0.0", &v173));
        assert!(!resolver::request_matches("^2.0.0", &v173));
    }

    fn test_manifest(name: &str, version: &str) -> PackageManifest {
        let json = serde_json::json!({
            "name": name, "version": version,
            "dependencies": {}, "devDependencies": {}, "optionalDependencies": {}, "peerDependencies": {},
        });
        let path = std::path::Path::new("/tmp/test.json");
        PackageManifest::from_json(&json.to_string(), path).expect("valid test manifest")
    }

    #[tokio::test]
    async fn async_resolve_no_registry_hang() {
        let manifest = test_manifest("net-test", "1.0.0");
        let config = NpmConfig::default();
        let registry = AsyncRegistryClient::new(config);
        let result = resolve_manifest_async(&manifest, &registry, "bpm-test").await;
        if let Err(error) = &result {
            match error {
                AsyncResolveError::Http { .. }
                | AsyncResolveError::Registry { .. }
                | AsyncResolveError::InvalidRange { .. } => {}
                other => {
                    eprintln!("Got expected error: {other}");
                }
            }
        }
    }

    #[tokio::test]
    async fn test_registry_req() {
        let (name, spec) = resolver::registry_request("react", "npm:react@18.2.0");
        assert_eq!(name, "react");
        assert_eq!(spec, "18.2.0");
        let (name, spec) = resolver::registry_request("react", "^18.0.0");
        assert_eq!(name, "react");
        assert_eq!(spec, "^18.0.0");
    }

    #[tokio::test]
    async fn async_sink_none_matches_vanilla() {
        let manifest = test_manifest("sink-test", "1.0.0");
        let config = NpmConfig::default();
        let registry = AsyncRegistryClient::new(config.clone());
        let registry2 = AsyncRegistryClient::new(config);
        let lock_no_sink = resolve_manifest_async(&manifest, &registry, "bpm-test")
            .await
            .unwrap();
        let lock_with_sink = resolve_manifest_with_workspaces_async_sink(
            &manifest, &registry2, "bpm-test", None, None,
        )
        .await
        .unwrap();
        assert_eq!(
            lock_no_sink.to_json().unwrap(),
            lock_with_sink.to_json().unwrap(),
            "sink=None must produce identical output"
        );
    }

    #[tokio::test]
    async fn async_sink_records_announced_packages() {
        let manifest = test_manifest("sink-recording", "1.0.0");
        let config = NpmConfig::default();
        let registry = AsyncRegistryClient::new(config);
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        struct RecordSink(std::sync::Arc<std::sync::Mutex<Vec<ResolvedDownloadUnit>>>);
        impl ResolveSink for RecordSink {
            fn emit(&self, unit: ResolvedDownloadUnit) {
                self.0.lock().unwrap().push(unit);
            }
        }
        let sink = RecordSink(recorded.clone());
        let _lock = resolve_manifest_with_workspaces_async_sink(
            &manifest,
            &registry,
            "bpm-test",
            None,
            Some(&sink as &dyn ResolveSink),
        )
        .await
        .unwrap();
        let units = recorded.lock().unwrap();
        assert!(
            units.is_empty(),
            "empty manifest must not announce any packages; got {}: {:?}",
            units.len(),
            units
        );
    }
}
