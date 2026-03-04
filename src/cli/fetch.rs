//! Package fetch command orchestration.
//!
//! Loads effective npm configuration once, constructs one pooled HTTP client,
//! and passes it to both registry metadata resolution and artifact retrieval so
//! auth tokens, retry policy, scoped registries, and connection pooling apply
//! to every request. Never calls default-config compatibility APIs.

use std::{env, fs, io, path::PathBuf};

use bpm::config::NpmConfig;
use bpm::http::HttpClient;
use bpm::integrity::Integrity;
use bpm::metrics::Metrics;
use bpm::registry::RegistryClient;
use bpm::store::ArtifactStore;

pub(super) fn run(
    target: &str,
    integrity: Option<String>,
    registry: Option<String>,
    store: Option<PathBuf>,
    no_extract: bool,
    json_metrics: Option<PathBuf>,
) -> anyhow::Result<()> {
    let store_root = store_root(store)?;
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
    let registry_client = RegistryClient::with_client(config, http.clone());

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
                resolved.name, resolved.version, resolved.tarball_url
            );
            (resolved.tarball_url, Some(parsed))
        } else {
            let parsed = integrity.as_deref().map(Integrity::parse).transpose()?;
            (target.to_string(), parsed)
        };

    let artifact =
        store.ensure_artifact_with_client(&http, &url, integrity.as_ref(), &mut metrics)?;
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
