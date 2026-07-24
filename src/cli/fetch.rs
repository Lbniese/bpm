//! Package fetch command orchestration.
//!
//! Loads effective npm configuration once, constructs one pooled HTTP client,
//! and passes it to both registry metadata resolution and artifact retrieval so
//! auth tokens, retry policy, scoped registries, and connection pooling apply
//! to every request. Never calls default-config compatibility APIs.

use std::{env, fs, io, path::PathBuf};

use bpm::config::NpmConfig;
use bpm::http::{redact_url, HttpClient};
use bpm::integrity::Integrity;
use bpm::metadata_cache::{CacheMode, MetadataCache};
use bpm::metrics::Metrics;
use bpm::registry::RegistryClient;
use bpm::store::ArtifactStore;

#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    target: &str,
    integrity: Option<String>,
    registry: Option<String>,
    store: Option<PathBuf>,
    no_extract: bool,
    json_metrics: Option<PathBuf>,
    cache_mode: CacheMode,
    remote_cache: Option<String>,
) -> anyhow::Result<()> {
    let store_root = store_root(store)?;
    let remote_cache = remote_cache
        .or_else(|| env::var_os("BPM_REMOTE_CACHE").map(|v| v.to_string_lossy().into_owned()));
    let store = ArtifactStore::open(&store_root)?;
    let mut metrics = Metrics::new();

    // Load effective npm configuration: user $HOME/.npmrc, then project .npmrc.
    // If no project root exists (no package.json), fall back to the current
    // directory so fetching a bare tarball URL still works.
    let cwd = env::current_dir()?;
    let home = env::var_os("HOME").map(PathBuf::from);
    let project_dir = bpm::project::find_project_root(&cwd).unwrap_or_else(|_| cwd.clone());
    let config = NpmConfig::load(&project_dir, home.as_deref())
        .map_err(|e| anyhow::anyhow!("failed to load npm config: {e}"))?;

    // Apply --registry or $BPM_REGISTRY as a default-registry override,
    // preserving loaded auth tokens, scoped registries, retry policy, and
    // timeouts. The override only changes the default registry URL used when a
    // package has no matching scoped-registry entry.
    let effective_registry =
        registry.or_else(|| env::var_os("BPM_REGISTRY").map(|s| s.to_string_lossy().into_owned()));
    let config = match effective_registry {
        Some(r) => config
            .with_registry_override(&r)
            .map_err(|e| anyhow::anyhow!("invalid registry override: {e}"))?,
        None => config,
    };

    // One pooled HTTP client shared between metadata resolution and every
    // artifact download so auth tokens, retry/backoff, timeouts, and the
    // underlying connection pool apply uniformly.
    let http = HttpClient::new(config.clone());
    let registry_client = open_registry_client(&store_root, config, http.clone(), cache_mode)?;

    let (url, integrity): (String, Option<Integrity>) =
        if bpm::registry::is_valid_npm_name(name_of_spec(target)) {
            let spec = bpm::registry::parse_spec(target)?;
            let resolved = metrics
                .measure("metadata_fetch", || registry_client.resolve(&spec))
                .map_err(|e| anyhow::anyhow!("failed to resolve '{target}': {e}"))?;
            let parsed = match integrity.as_deref() {
                Some(value) => Integrity::parse(value)?,
                None => Integrity::parse(&resolved.integrity)?,
            };
            eprintln!(
                "resolved {}@{} -> {}",
                resolved.name,
                resolved.version,
                redact_url(&resolved.tarball_url)
            );
            (resolved.tarball_url, Some(parsed))
        } else {
            let parsed = integrity.as_deref().map(Integrity::parse).transpose()?;
            (target.to_string(), parsed)
        };

    let remote = if cache_mode.allows_network() {
        remote_cache
            .map(|base| {
                let token = env::var("BPM_REMOTE_CACHE_TOKEN").ok();
                let config =
                    bpm::remote_cache::RemoteCacheConfig::new(&base, token).map_err(|error| {
                        anyhow::anyhow!("invalid remote cache configuration: {error}")
                    })?;
                bpm::remote_cache::RemoteCacheClient::new(config).map_err(|error| {
                    anyhow::anyhow!("could not create remote cache client: {error}")
                })
            })
            .transpose()?
    } else {
        None
    };
    let artifact = if let Some(remote) = remote.as_ref() {
        store
            .ensure_artifact_with_remote(&http, remote, &url, integrity.as_ref(), &mut metrics)?
            .artifact
    } else {
        store.ensure_artifact_with_client(&http, &url, integrity.as_ref(), &mut metrics)?
    };
    println!(
        "artifact {} ({}) -> {}",
        artifact.id,
        if artifact.cached { "cached" } else { "stored" },
        artifact.path.display()
    );
    if !no_extract {
        let image = store.ensure_image(&artifact.id, &mut metrics)?;
        println!(
            "image {} ({}) -> {}",
            image.id,
            if image.cached { "cached" } else { "extracted" },
            image.path.display()
        );
    }
    write_metrics(&metrics, json_metrics)
}

pub(super) fn name_of_spec(target: &str) -> &str {
    match target.rfind('@') {
        Some(0) | None => target,
        Some(index) => &target[..index],
    }
}

/// Resolve the metadata cache mode from CLI flags, falling back to the
/// `BPM_OFFLINE` / `BPM_PREFER_OFFLINE` / `BPM_PREFER_ONLINE` environment
/// variables (npm-compatible behavior) and finally [`CacheMode::Default`].
///
/// `offline` wins over `prefer_offline`, which wins over `prefer_online`,
/// matching npm's mutual-exclusion of these options.
pub(super) fn resolve_cache_mode(
    offline: bool,
    prefer_offline: bool,
    prefer_online: bool,
) -> CacheMode {
    if offline || truthy_env("BPM_OFFLINE") {
        CacheMode::Offline
    } else if prefer_offline || truthy_env("BPM_PREFER_OFFLINE") {
        CacheMode::PreferOffline
    } else if prefer_online || truthy_env("BPM_PREFER_ONLINE") {
        CacheMode::PreferOnline
    } else {
        CacheMode::Default
    }
}

fn truthy_env(name: &str) -> bool {
    matches!(env::var(name).ok().as_deref(), Some("1") | Some("true"))
}

/// Build a registry client with the persistent packument cache attached when
/// available. For `Offline` and `PreferOffline`, a cache-open failure is
/// fatal; other modes fall back to the uncached client path.
pub(super) fn open_registry_client(
    store_root: &std::path::Path,
    config: NpmConfig,
    http: HttpClient,
    cache_mode: CacheMode,
) -> anyhow::Result<RegistryClient> {
    // Prefetch overlaps sibling packument fetches during graph expansion over
    // the shared HTTP/2 pool. It is pointless (and would spin failing
    // offline-miss attempts) when network access is forbidden. The
    // `BPM_PREFETCH_WORKERS` env var overrides the default (0 disables).
    let workers = prefetch_worker_count(cache_mode);
    let client = RegistryClient::with_client(config, http).with_prefetch(workers);
    match open_metadata_cache(store_root, cache_mode)? {
        Some(cache) => Ok(client.with_metadata_cache(cache, cache_mode)),
        None => Ok(client),
    }
}

/// Open metadata cache in strict mode for offline/pref-offline, and fallback to
/// uncached resolution only for online modes.
pub(super) fn open_metadata_cache(
    store_root: &std::path::Path,
    cache_mode: CacheMode,
) -> anyhow::Result<Option<std::sync::Arc<MetadataCache>>> {
    match MetadataCache::open(store_root) {
        Ok(cache) => Ok(Some(std::sync::Arc::new(cache))),
        Err(error) => match cache_mode {
            CacheMode::Offline => Err(anyhow::anyhow!(
                "metadata cache unavailable in offline mode: {error}"
            )),
            CacheMode::PreferOffline => Err(anyhow::anyhow!(
                "metadata cache unavailable in prefer-offline mode: {error}"
            )),
            CacheMode::Default | CacheMode::PreferOnline => {
                eprintln!("warn: metadata cache unavailable, continuing uncached: {error}");
                Ok(None)
            }
        },
    }
}

/// Default number of background packument prefetch threads: one per available
/// core, capped low so concurrent metadata fetches do not burst hard enough
/// to trip registry rate-limiting or stall the HTTP/2 stream. The pooled
/// client multiplexes whatever count is chosen.
fn default_prefetch_workers() -> usize {
    // Scale workers with available cores. The HTTP/1.1 transport gives each
    // worker its own connection (pool_max_idle_per_host=64), so higher worker
    // counts directly increase packument fetch concurrency.
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8)
        .min(16)
}

/// Resolve the prefetch worker count from the `BPM_PREFETCH_WORKERS` override
/// (0 disables prefetch entirely) or the computed default. Forced to 0 when
/// the cache mode forbids network access.
fn prefetch_worker_count(cache_mode: CacheMode) -> usize {
    if let Ok(raw) = std::env::var("BPM_PREFETCH_WORKERS") {
        if let Ok(value) = raw.trim().parse::<usize>() {
            return value;
        }
    }
    if cache_mode.allows_network() {
        default_prefetch_workers()
    } else {
        0
    }
}

pub(super) fn store_root(store: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    store
        .or_else(|| env::var_os("BPM_STORE").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".bpm")))
        .ok_or_else(|| anyhow::anyhow!("no --store given and $BPM_STORE/$HOME is unset"))
}

pub(super) fn write_metrics(
    metrics: &Metrics,
    json_metrics: Option<PathBuf>,
) -> anyhow::Result<()> {
    if matches!(
        env::var("BPM_TRACE").ok().as_deref(),
        Some("1") | Some("true")
    ) {
        metrics
            .print_trace(&mut io::stderr())
            .map_err(|e| anyhow::anyhow!("failed to write trace: {e}"))?;
    }
    if let Some(path) = json_metrics {
        fs::write(&path, metrics.to_json())
            .map_err(|e| anyhow::anyhow!("failed to write metrics to {}: {e}", path.display()))?;
    }
    Ok(())
}
