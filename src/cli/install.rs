//! Lockfile and global-bin install orchestration.

use std::collections::{BTreeSet, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use bpm::config::NpmConfig;
use bpm::graph;
use bpm::http::HttpClient;
use bpm::integrity::{ArtifactId, Integrity};
use bpm::lockfile::{find_lockfile, Lockfile};
use bpm::manifest::PackageManifest;
use bpm::metrics::Metrics;
use bpm::resolver;
use bpm::resolver::model::PlatformConstraints;
use bpm::resolver::platform::{check_package_platform, PackageReachability, PlatformDisposition};
use bpm::store::{ArtifactStore, StoreError};

use super::fetch::{name_of_spec, open_registry_client, store_root, write_metrics};

pub(super) struct Options {
    pub target: Option<String>,
    pub frozen: bool,
    pub registry: Option<String>,
    pub store: Option<PathBuf>,
    pub concurrency: usize,
    pub json_metrics: Option<PathBuf>,
    pub global: bool,
    pub ignore_scripts: bool,
    pub legacy_peer_deps: bool,
    pub cache_mode: bpm::metadata_cache::CacheMode,
}

pub(super) fn run(options: Options) -> anyhow::Result<()> {
    if let Some(target) = options.target {
        return run_install_bin(
            &target,
            options.registry,
            options.store,
            options.global,
            options.cache_mode,
        );
    }

    let store_root_path = store_root(options.store.clone())?;
    let store = ArtifactStore::open(&store_root_path)?;
    let mut metrics = Metrics::new();
    let cwd = env::current_dir()?;
    let (lockfile_path, lockfile, project_root) = match find_lockfile(&cwd)? {
        Some((path, lockfile)) => {
            let root = path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            (path, lockfile, root)
        }
        None if options.frozen => anyhow::bail!(
            "frozen install requires bpm.lock in {} or an ancestor",
            cwd.display()
        ),
        None => {
            let root = bpm::project::find_project_root(&cwd).unwrap_or(cwd.clone());
            let manifest = PackageManifest::from_path(&root.join("package.json"))
                .map_err(|error| anyhow::anyhow!("cannot resolve dependencies: {error}"))?;
            let config = effective_npm_config(&root, options.registry.as_deref())?;
            let http = HttpClient::new(config.clone());
            let client =
                open_registry_client(&store_root_path, config, http.clone(), options.cache_mode)?;
            let workspace_layout = bpm::workspace::discover(&root);
            let workspace_index = bpm::resolver::workspaces::WorkspaceIndex::from_project_root(
                &root,
                &workspace_layout,
            )
            .map_err(|error| anyhow::anyhow!("workspace resolution failed: {error}"))?;
            let peer_mode = if options.legacy_peer_deps {
                bpm::resolver::peer::PeerMode::LegacyIgnore
            } else {
                bpm::resolver::peer::PeerMode::Strict
            };
            if streaming_install_enabled() {
                return run_streaming_install(
                    &root,
                    &manifest,
                    &client,
                    &workspace_index,
                    peer_mode,
                    options.concurrency,
                    &store,
                    &http,
                    &mut metrics,
                    &options,
                );
            }
            // Streaming disabled (BPM_STREAM_INSTALL=0): resolve the whole
            // graph first, then let the shared download pipeline below run.
            let lockfile = metrics
                .measure("dependency_resolution", || {
                    resolver::resolve_manifest_with_options(
                        &manifest,
                        &client,
                        "bpm",
                        Some(&workspace_index),
                        peer_mode,
                    )
                })
                .map_err(|error| anyhow::anyhow!("dependency resolution failed: {error}"))?;
            let path = root.join(bpm::lockfile::BPM_LOCK_FILE);
            lockfile.write_to(&path)?;
            eprintln!(
                "resolved {} package(s) and wrote {}",
                lockfile.packages.len(),
                path.display()
            );
            (path, lockfile, root)
        }
    };
    if options.frozen {
        enforce_frozen(&project_root, &lockfile)?;
    }

    let config = effective_npm_config(&project_root, options.registry.as_deref())?;
    let http = HttpClient::new(config);

    let plan_path = graph::plan_path_for(&lockfile_path);
    let cached_plan = graph::read_plan(&plan_path)?;
    let plan_valid = cached_plan
        .as_ref()
        .is_some_and(|plan| graph::validate_plan(plan, &lockfile, &project_root, &store).is_ok());
    if plan_valid {
        metrics.record("plan_cache_hit", std::time::Duration::ZERO);
        let plan = cached_plan.as_ref().expect("validated cached plan exists");
        let materialized = plan
            .entries
            .iter()
            .filter(|entry| {
                !entry.link && !entry.resolved.is_empty() && !entry.artifact_hex.is_empty()
            })
            .count();
        let bins = plan
            .entries
            .iter()
            .map(|entry| entry.bin.len())
            .sum::<usize>();
        println!(
            "nothing to install — graph {} unchanged ({} package(s), {} bin(s) already materialized)",
            graph::graph_id_for_project(&lockfile, &project_root).to_hex_short(),
            materialized,
            bins
        );
        return write_metrics(&metrics, options.json_metrics);
    }
    metrics.record("plan_cache_miss", std::time::Duration::ZERO);

    let work = build_install_work(&lockfile, options.frozen)?;
    let workers = adaptive_workers(options.concurrency, work.len(), &project_root);
    let outcomes = std::thread::scope(|scope| -> anyhow::Result<Vec<FetchOutcome>> {
        let (unit_tx, unit_rx) = std::sync::mpsc::sync_channel::<InstallWork>(workers.max(1) * 2);
        let unit_rx = std::sync::Arc::new(std::sync::Mutex::new(unit_rx));
        let (downloaders, extractors) =
            spawn_fetch_pipeline(scope, &store, &http, unit_rx, workers);
        for item in work {
            if unit_tx.send(item).is_err() {
                break;
            }
        }
        drop(unit_tx);
        join_pipeline(downloaders, extractors, &mut metrics)
    })?;

    let cached = outcomes
        .iter()
        .filter(|outcome| outcome.artifact_cached && outcome.image_cached)
        .count();
    let fetched = outcomes.len() - cached;
    let artifact_ids = outcomes_to_artifact_ids(&outcomes, &lockfile);
    finalize_install(
        &project_root,
        &store,
        &lockfile,
        &artifact_ids,
        cached,
        fetched,
        &mut metrics,
        &options,
        &lockfile_path,
    )
}

fn use_local_project_view(lockfile: &Lockfile) -> bool {
    match env::var("BPM_PROJECT_VIEW").as_deref() {
        Ok("local") => true,
        Ok("relay") => false,
        Ok(value) if !value.is_empty() => {
            eprintln!(
                "warning: unsupported BPM_PROJECT_VIEW={value:?}; expected relay or local; using auto"
            );
            lockfile.root.dependencies.contains_key("next")
        }
        _ => auto_local_project_view(lockfile),
    }
}

fn auto_local_project_view(lockfile: &Lockfile) -> bool {
    // Check the resolved graph rather than only root declarations. Imported
    // lockfiles and workspace layouts can represent the app's Next package as
    // a transitive placement, but Next still resolves its toolchain from the
    // project and requires those realpaths to remain project-local.
    lockfile
        .packages
        .iter()
        .any(|package| package.name == "next")
}

fn adaptive_workers(requested: usize, work_items: usize, project_root: &Path) -> usize {
    if requested > 0 {
        return requested.min(work_items.max(1));
    }
    let cpu = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1);
    let fs_limit = bpm::workspace::probe_fs_capabilities(project_root)
        .ok()
        .map(|caps| {
            if caps.atomic_directory_rename.is_supported() {
                8
            } else {
                2
            }
        })
        .unwrap_or(4);
    cpu.saturating_mul(2)
        .clamp(1, fs_limit)
        .min(work_items.max(1))
}

#[allow(clippy::too_many_arguments)]
fn run_lifecycle_if_enabled(
    project_root: &Path,
    store: &ArtifactStore,
    lockfile: &Lockfile,
    artifact_ids: &[Option<ArtifactId>],
    volume_path: Option<&Path>,
    ignore_scripts: bool,
    skip_execution: bool,
    metrics: &mut Metrics,
) -> bpm::lifecycle::LifecycleStats {
    if ignore_scripts {
        metrics.record("lifecycle", std::time::Duration::ZERO);
        return bpm::lifecycle::LifecycleStats::default();
    }
    let policy = bpm::lifecycle::LifecyclePolicy {
        ignore_scripts: false,
        skip_execution,
    };
    match bpm::lifecycle::run_lifecycle(
        project_root,
        store,
        lockfile,
        artifact_ids,
        volume_path,
        policy,
        metrics,
    ) {
        Ok(result) if result.skipped => {
            // Cached volume: nothing ran, so there is no per-phase summary to
            // print. `run_lifecycle` already recorded the skip metric.
            result
        }
        Ok(result) if result.packages_with_scripts > 0 => {
            eprintln!(
                "lifecycle: {} package(s) with scripts ({} phase(s) executed, {} succeeded, {} failed)",
                result.packages_with_scripts,
                result.phases_executed,
                result.phases_succeeded,
                result.phases_failed
            );
            for outcome in &result.outcomes {
                let marker = if outcome.exit_code == Some(0) {
                    "ok"
                } else {
                    "FAIL"
                };
                eprintln!(
                    "  [{marker}] {}.{}) {}",
                    outcome.package, outcome.phase, outcome.command
                );
            }
            result
        }
        Ok(result) => result,
        Err(error) => {
            eprintln!("warning: lifecycle phase failed: {error}");
            bpm::lifecycle::LifecycleStats::default()
        }
    }
}

fn run_install_bin(
    target: &str,
    registry: Option<String>,
    store: Option<PathBuf>,
    _global: bool,
    cache_mode: bpm::metadata_cache::CacheMode,
) -> anyhow::Result<()> {
    let store_root_path = store_root(store)?;
    let store = ArtifactStore::open(&store_root_path)?;
    let mut metrics = Metrics::new();
    let cwd = env::current_dir()?;
    let project_root = bpm::project::find_project_root(&cwd).unwrap_or(cwd);
    let config = effective_npm_config(&project_root, registry.as_deref())?;
    let http = HttpClient::new(config.clone());
    let registry_client = open_registry_client(&store_root_path, config, http.clone(), cache_mode)?;

    let (url, integrity) = if bpm::registry::is_valid_npm_name(name_of_spec(target)) {
        let spec = bpm::registry::parse_spec(target)?;
        let resolved = metrics
            .measure("metadata_fetch", || registry_client.resolve(&spec))
            .map_err(|error| anyhow::anyhow!("failed to resolve '{target}': {error}"))?;
        eprintln!(
            "resolved {}@{} -> {}",
            resolved.name, resolved.version, resolved.tarball_url
        );
        let integrity = Integrity::parse(&resolved.integrity)?;
        (resolved.tarball_url, Some(integrity))
    } else {
        (target.to_string(), None)
    };
    let artifact =
        store.ensure_artifact_with_client(&http, &url, integrity.as_ref(), &mut metrics)?;
    let image = store.ensure_image(&artifact.id, &mut metrics)?;
    println!(
        "fetched {} ({}) -> {}",
        artifact.id,
        if artifact.cached { "cached" } else { "stored" },
        image.path.display()
    );

    let manifest_path = image.path.join("package.json");
    let manifest = PackageManifest::from_path(&manifest_path)
        .map_err(|error| anyhow::anyhow!("could not read {}: {error}", manifest_path.display()))?;
    let bins: Vec<(String, String)> = match &manifest.bin {
        Some(bpm::manifest::BinField::Map(entries)) => entries
            .iter()
            .map(|(name, path)| (name.clone(), path.clone()))
            .collect(),
        Some(bpm::manifest::BinField::One(path)) => vec![(
            manifest.name.clone().unwrap_or_else(|| target.to_string()),
            path.clone(),
        )],
        None => Vec::new(),
    };
    if bins.is_empty() {
        anyhow::bail!(
            "package {} declares no `bin` executables; nothing to link",
            manifest.name.as_deref().unwrap_or(target)
        );
    }
    let bin_dir = bin_dir()?;
    fs::create_dir_all(&bin_dir)
        .map_err(|error| anyhow::anyhow!("could not create {}: {error}", bin_dir.display()))?;
    let mut linked = Vec::new();
    for (name, relative_path) in bins {
        let relative_path = relative_path.strip_prefix("./").unwrap_or(&relative_path);
        let target_file = image.path.join(relative_path);
        if !target_file.exists() {
            eprintln!(
                "warning: bin '{}' points at missing file {}; skipping",
                name,
                target_file.display()
            );
            continue;
        }
        set_executable(&target_file);
        link_bin(&bin_dir.join(&name), &target_file)?;
        linked.push(name);
    }
    if linked.is_empty() {
        anyhow::bail!("no bins were linked for {target}");
    }
    println!(
        "linked {} bin(s) into {}: {}",
        linked.len(),
        bin_dir.display(),
        linked.join(", ")
    );
    if !bin_dir_on_path() {
        eprintln!(
            "note: {} is not on your PATH; add it (e.g. `export PATH=\"{}:$PATH\"`)",
            bin_dir.display(),
            bin_dir.display()
        );
    }
    Ok(())
}

fn effective_npm_config(project_root: &Path, registry: Option<&str>) -> anyhow::Result<NpmConfig> {
    let home = env::var_os("HOME").map(PathBuf::from);
    let mut config = NpmConfig::load(project_root, home.as_deref())
        .map_err(|e| anyhow::anyhow!("failed to load npm config: {e}"))?;
    if let Some(registry) = registry {
        config = config
            .with_registry_override(registry)
            .map_err(|e| anyhow::anyhow!("invalid registry override: {e}"))?;
    }
    Ok(config)
}

pub(super) fn bin_dir() -> anyhow::Result<PathBuf> {
    if let Some(path) = env::var_os("BPM_BIN") {
        return Ok(PathBuf::from(path));
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("$HOME is unset; cannot choose a bin dir"))?;
    let local = home.join(".local").join("bin");
    if local.is_dir() {
        return Ok(local);
    }
    Ok(home.join("bin"))
}

fn bin_dir_on_path() -> bool {
    let Ok(bin_dir) = bin_dir() else {
        return false;
    };
    env::var_os("PATH")
        .map(|path| env::split_paths(&path).any(|entry| entry == bin_dir))
        .unwrap_or(false)
}

fn link_bin(link: &Path, target: &Path) -> anyhow::Result<()> {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| anyhow::anyhow!("could not create {}: {error}", parent.display()))?;
    }
    if fs::read_link(link).is_ok_and(|existing| existing.components().eq(target.components())) {
        return Ok(());
    }
    let _ = fs::remove_file(link);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).map_err(|error| {
            anyhow::anyhow!(
                "could not symlink {} -> {}: {error}",
                link.display(),
                target.display()
            )
        })?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!("bin linking is only supported on Unix-like systems");
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        let _ = fs::set_permissions(path, permissions);
    }
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) {}

fn enforce_frozen(project_root: &Path, lockfile: &Lockfile) -> anyhow::Result<()> {
    let manifest_path = project_root.join("package.json");
    let manifest = match PackageManifest::from_path(&manifest_path) {
        Ok(manifest) => manifest,
        Err(error) => {
            eprintln!(
                "warning: --frozen given but no readable package.json at {} ({error}); skipping drift check",
                project_root.display()
            );
            return Ok(());
        }
    };
    let root_declarations = manifest.root_dependency_declarations();
    let declared: BTreeSet<String> = root_declarations.keys().cloned().collect();
    let locked: BTreeSet<String> = lockfile.root.dependencies.keys().cloned().collect();
    let expected_overrides = resolver::overrides::OverrideSet::from_manifest(
        &manifest.overrides,
        &root_declarations,
        resolver::overrides::OverrideOrigin::Root,
    )
    .map_err(|error| anyhow::anyhow!("frozen install refused: invalid overrides: {error}"))?
    .as_map()
    .clone();
    let root_resolution = &lockfile.resolution.root;
    if declared == locked
        && root_resolution.dev_dependencies == manifest.dev_dependencies
        && root_resolution.optional_dependencies == manifest.optional_dependencies
        && root_resolution.overrides == expected_overrides
    {
        return Ok(());
    }
    let only_manifest = declared
        .difference(&locked)
        .map(String::as_str)
        .collect::<Vec<_>>();
    let only_lock = locked
        .difference(&declared)
        .map(String::as_str)
        .collect::<Vec<_>>();
    anyhow::bail!(
        "frozen install refused: package.json and bpm.lock disagree on root dependencies\n  \
         in package.json but not bpm.lock: {}\n  \
         in bpm.lock but not package.json: {}\n  \
         re-run `bpm import` after editing package.json",
        if only_manifest.is_empty() {
            "(none)".to_string()
        } else {
            only_manifest.join(", ")
        },
        if only_lock.is_empty() {
            "(none)".to_string()
        } else {
            only_lock.join(", ")
        }
    );
}

struct InstallWork {
    path: String,
    name: String,
    url: String,
    integrity: Option<Integrity>,
}

struct FetchOutcome {
    path: String,
    id: ArtifactId,
    artifact_cached: bool,
    image_cached: bool,
}

struct PendingArtifact {
    path: String,
    name: String,
    url: String,
    artifact: bpm::store::ArtifactRef,
}

#[derive(Debug)]
struct FetchFail {
    name: String,
    url: String,
    source: Box<StoreError>,
}

/// Adapts the resolver's [`ResolveSink`] to the install download pipeline's
/// bounded unit channel. `emit` blocks when the pipeline is full (natural
/// backpressure on resolution) and ignores a disconnected receiver so a failed
/// install still yields a complete lockfile.
struct ChannelSink(std::sync::mpsc::SyncSender<InstallWork>);

impl resolver::ResolveSink for ChannelSink {
    fn emit(&self, unit: resolver::ResolvedDownloadUnit) {
        let integrity = unit
            .integrity
            .as_deref()
            .and_then(|value| Integrity::parse(value).ok());
        let _ = self.0.send(InstallWork {
            path: unit.path,
            name: unit.name,
            url: unit.url,
            integrity,
        });
    }
}

/// Join handle for a download worker thread.
type DownloaderHandle<'scope> = std::thread::ScopedJoinHandle<'scope, Result<Metrics, FetchFail>>;
/// Join handle for an extract worker thread.
type ExtractorHandle<'scope> =
    std::thread::ScopedJoinHandle<'scope, Result<(Vec<FetchOutcome>, Metrics), FetchFail>>;

/// Spawn the download→extract worker pipeline consuming resolved install units
/// from `unit_rx`. Returns the downloader and extractor join handles; the
/// caller joins them via [`join_pipeline`] after the unit producer finishes.
///
/// Downloaders always drain `unit_rx` to completion (stopping only fetch work,
/// never receiving, once the extract stage has gone away) so a streaming
/// producer can never block on a full bounded channel.
#[allow(clippy::too_many_arguments)]
fn spawn_fetch_pipeline<'scope, 'env>(
    scope: &'scope std::thread::Scope<'scope, 'env>,
    store: &'env ArtifactStore,
    http: &'env HttpClient,
    unit_rx: std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<InstallWork>>>,
    workers: usize,
) -> (Vec<DownloaderHandle<'scope>>, Vec<ExtractorHandle<'scope>>) {
    use std::sync::mpsc::sync_channel;
    let workers = workers.max(1);
    let (send, receive) = sync_channel::<Result<PendingArtifact, FetchFail>>(workers * 2);
    let receive = std::sync::Arc::new(std::sync::Mutex::new(receive));
    let mut downloaders = Vec::with_capacity(workers);
    for _ in 0..workers {
        let unit_rx = unit_rx.clone();
        let send = send.clone();
        let http = http.clone();
        downloaders.push(scope.spawn(move || -> Result<Metrics, FetchFail> {
            let mut local = Metrics::new();
            let mut extraction_gone = false;
            while let Ok(item) = unit_rx.lock().expect("unit receiver lock").recv() {
                if extraction_gone {
                    continue;
                }
                let result = store
                    .ensure_artifact_with_client(
                        &http,
                        &item.url,
                        item.integrity.as_ref(),
                        &mut local,
                    )
                    .map(|artifact| PendingArtifact {
                        path: item.path.clone(),
                        name: item.name.clone(),
                        url: item.url.clone(),
                        artifact,
                    })
                    .map_err(|source| FetchFail {
                        name: item.name.clone(),
                        url: item.url.clone(),
                        source: Box::new(source),
                    });
                if send.send(result).is_err() {
                    // Extractors all exited (fatal error). Keep draining unit_rx
                    // so a streaming producer never blocks on a full channel;
                    // just stop doing fetch work.
                    extraction_gone = true;
                }
            }
            Ok(local)
        }));
    }
    drop(send);
    let mut extractors = Vec::with_capacity(workers);
    for _ in 0..workers {
        let receive = receive.clone();
        extractors.push(
            scope.spawn(move || -> Result<(Vec<FetchOutcome>, Metrics), FetchFail> {
                let mut local = Metrics::new();
                let mut outcomes = Vec::new();
                let mut first_error: Option<FetchFail> = None;
                loop {
                    let message = receive.lock().expect("pipeline receiver lock").recv();
                    let Ok(message) = message else {
                        break;
                    };
                    let Ok(pending) = message else {
                        // Keep draining the bounded channel after one worker
                        // fails. Returning immediately would strand downloaders
                        // blocked on send and turn a fetch error into a hang.
                        if first_error.is_none() {
                            first_error = message.err();
                        }
                        continue;
                    };
                    if first_error.is_some() {
                        continue;
                    }
                    match store.ensure_image(&pending.artifact.id, &mut local) {
                        Ok(image) => outcomes.push(FetchOutcome {
                            path: pending.path.clone(),
                            id: pending.artifact.id,
                            artifact_cached: pending.artifact.cached,
                            image_cached: image.cached,
                        }),
                        Err(source) => {
                            first_error = Some(FetchFail {
                                name: pending.name,
                                url: pending.url,
                                source: Box::new(source),
                            });
                        }
                    }
                }
                if let Some(error) = first_error {
                    Err(error)
                } else {
                    Ok((outcomes, local))
                }
            }),
        );
    }
    (downloaders, extractors)
}

/// Join the pipeline handles, merging per-worker metrics and surfacing the
/// first fetch/extract error.
fn join_pipeline(
    downloaders: Vec<DownloaderHandle<'_>>,
    extractors: Vec<ExtractorHandle<'_>>,
    metrics: &mut Metrics,
) -> anyhow::Result<Vec<FetchOutcome>> {
    for handle in downloaders {
        metrics.extend(
            &handle
                .join()
                .map_err(|_| anyhow::anyhow!("download worker panicked"))??,
        );
    }
    let mut outcomes = Vec::new();
    for handle in extractors {
        let (mut values, local) = handle
            .join()
            .map_err(|_| anyhow::anyhow!("extract worker panicked"))??;
        metrics.extend(&local);
        outcomes.append(&mut values);
    }
    Ok(outcomes)
}

/// Map path-keyed fetch outcomes back onto the lockfile's positional index
/// for the materializer and lifecycle phases.
fn outcomes_to_artifact_ids(
    outcomes: &[FetchOutcome],
    lockfile: &Lockfile,
) -> Vec<Option<ArtifactId>> {
    let path_to_index: HashMap<&str, usize> = lockfile
        .packages
        .iter()
        .enumerate()
        .map(|(index, package)| (package.path.as_str(), index))
        .collect();
    let mut artifact_ids = vec![None; lockfile.packages.len()];
    for outcome in outcomes {
        if let Some(&index) = path_to_index.get(outcome.path.as_str()) {
            artifact_ids[index] = Some(outcome.id);
        }
    }
    artifact_ids
}

/// Whether a fresh install overlaps downloads with resolution (Phase 3
/// streaming). Enabled by default; set `BPM_STREAM_INSTALL=0` to resolve the
/// whole graph before downloading (the pre-Phase-3 behavior) for benchmarking
/// or to isolate a streaming-related regression.
fn streaming_install_enabled() -> bool {
    !matches!(
        std::env::var("BPM_STREAM_INSTALL").as_deref(),
        Ok("0") | Ok("false")
    )
}

/// Resolve a fresh manifest while the download/extract pipeline fetches each
/// package the instant the resolver places it, so downloads overlap with the
/// rest of resolution. The returned lockfile is byte-identical to a sequential
/// resolve (the sink only observes placement); downloads are integrity-keyed
/// and idempotent, so streaming never changes the installed graph.
#[allow(clippy::too_many_arguments)]
fn run_streaming_install(
    root: &Path,
    manifest: &PackageManifest,
    client: &bpm::registry::RegistryClient,
    workspace_index: &bpm::resolver::workspaces::WorkspaceIndex,
    peer_mode: bpm::resolver::peer::PeerMode,
    concurrency: usize,
    store: &ArtifactStore,
    http: &HttpClient,
    metrics: &mut Metrics,
    options: &Options,
) -> anyhow::Result<()> {
    // Work count is unknown until resolution completes, so do not clamp the
    // worker count to it (usize::MAX makes adaptive_workers' clamp a no-op).
    let workers = adaptive_workers(concurrency, usize::MAX, root);
    let (lockfile, outcomes) =
        std::thread::scope(|scope| -> anyhow::Result<(Lockfile, Vec<FetchOutcome>)> {
            let (unit_tx, unit_rx) =
                std::sync::mpsc::sync_channel::<InstallWork>(workers.max(1) * 2);
            let unit_rx = std::sync::Arc::new(std::sync::Mutex::new(unit_rx));
            let (downloaders, extractors) =
                spawn_fetch_pipeline(scope, store, http, unit_rx, workers);
            // Run resolution on this thread, emitting each placed node to the
            // sink; dropping `sink` closes the unit channel so downloaders (and
            // then extractors) drain and finish before we join them below.
            let lockfile = {
                let sink = ChannelSink(unit_tx);
                metrics
                    .measure("dependency_resolution", || {
                        resolver::resolve_manifest_with_options_sink(
                            manifest,
                            client,
                            "bpm",
                            Some(workspace_index),
                            peer_mode,
                            Some(&sink),
                        )
                    })
                    .map_err(|error| anyhow::anyhow!("dependency resolution failed: {error}"))?
            };
            let outcomes = join_pipeline(downloaders, extractors, metrics)?;
            Ok((lockfile, outcomes))
        })?;
    let path = root.join(bpm::lockfile::BPM_LOCK_FILE);
    lockfile.write_to(&path)?;
    eprintln!(
        "resolved {} package(s) and wrote {}",
        lockfile.packages.len(),
        path.display()
    );
    // Fresh resolve: no prior lockfile, so no prior plan — always install.
    metrics.record("plan_cache_miss", std::time::Duration::ZERO);
    let cached = outcomes
        .iter()
        .filter(|outcome| outcome.artifact_cached && outcome.image_cached)
        .count();
    let fetched = outcomes.len() - cached;
    let artifact_ids = outcomes_to_artifact_ids(&outcomes, &lockfile);
    finalize_install(
        root,
        store,
        &lockfile,
        &artifact_ids,
        cached,
        fetched,
        metrics,
        options,
        &path,
    )
}

/// Materialize the resolved graph, run lifecycle, write the install plan, and
/// print the summary. Shared by the lockfile-present and fresh-resolve install
/// paths; both produce a `lockfile` and its `artifact_ids` before calling this.
#[allow(clippy::too_many_arguments)]
fn finalize_install(
    project_root: &Path,
    store: &ArtifactStore,
    lockfile: &Lockfile,
    artifact_ids: &[Option<ArtifactId>],
    cached: usize,
    fetched: usize,
    metrics: &mut Metrics,
    options: &Options,
    lockfile_path: &Path,
) -> anyhow::Result<()> {
    let has_workspace_links = lockfile.packages.iter().any(|package| package.link);
    // Turbopack and similar bundlers enforce that dependency realpaths remain
    // inside the project. Keep the O(top-level) relay fast path for ordinary
    // projects, but use a local hardlink view automatically for Next projects;
    // callers can override this with BPM_PROJECT_VIEW=relay|local.
    let local_project_view = use_local_project_view(lockfile);
    let (volume, mut view_entry_count) = if has_workspace_links {
        bpm::materializer::materialize_lockfile_with_backend(
            project_root,
            store,
            lockfile,
            artifact_ids,
            bpm::materializer::MaterializeMode::Compatible,
            if local_project_view {
                bpm::materializer::MaterializeBackend::Hardlink
            } else {
                bpm::materializer::MaterializeBackend::Auto
            },
        )?;
        (None, 0usize)
    } else {
        let volume = bpm::volume::ensure_graph_volume(store, lockfile, artifact_ids, metrics)?;
        let attach = bpm::volume::attach_project(project_root, &volume)?;
        (
            Some(volume),
            attach.relays_created + attach.relays_unchanged,
        )
    };
    let lifecycle = run_lifecycle_if_enabled(
        project_root,
        store,
        lockfile,
        artifact_ids,
        volume.as_ref().map(|v| v.path.as_path()),
        options.ignore_scripts,
        // A reused graph volume already holds the derived lifecycle output
        // from the install that built it, so the scripts must not run again.
        // This only applies to the volume path; the workspace/compatible
        // path (volume == None) uses disposable sandboxes and still runs.
        volume.as_ref().is_some_and(|v| v.cached),
        metrics,
    );
    if local_project_view {
        if let Some(volume) = volume.as_ref() {
            let attached = bpm::volume::attach_project_local(project_root, volume)?;
            view_entry_count = attached.relays_created + attached.relays_unchanged;
        }
    }

    let plan_path = graph::plan_path_for(lockfile_path);
    let mut plan = graph::build_plan(lockfile, artifact_ids, &lifecycle.derived_paths);
    plan.graph_id_hex = graph::graph_id_for_project(lockfile, project_root).to_hex();
    if let Err(error) = graph::write_plan(&plan, &plan_path) {
        eprintln!(
            "warning: failed to write plan {}: {error}",
            plan_path.display()
        );
    }
    let package_count = lockfile
        .packages
        .iter()
        .filter(|package| !package.link && !package.resolved.is_empty())
        .count();
    println!(
        "installed {} package(s) into {} ({} cached, {} fetched; graph volume {}, {} project-view entry(s))",
        package_count,
        project_root.join("node_modules").display(),
        cached,
        fetched,
        if volume.as_ref().is_some_and(|volume| volume.cached) {
            "reused"
        } else if volume.is_some() {
            "built"
        } else {
            "direct"
        },
        view_entry_count
    );
    write_metrics(metrics, options.json_metrics.clone())
}

fn build_install_work(lockfile: &Lockfile, frozen: bool) -> anyhow::Result<Vec<InstallWork>> {
    let mut work = Vec::new();
    let target = resolver::current_target_platform();
    for package in lockfile.packages.iter() {
        if package.link || package.resolved.is_empty() {
            continue;
        }
        let constraints = PlatformConstraints {
            os: package.os.iter().cloned().collect(),
            cpu: package.cpu.iter().cloned().collect(),
            libc: lockfile
                .resolution
                .packages
                .get(&package.path)
                .map(|resolution| resolution.libc.iter().cloned().collect())
                .unwrap_or_default(),
        };
        match check_package_platform(
            &format!("{}@{}", package.name, package.version),
            &constraints,
            &target,
            if package.optional {
                PackageReachability::OptionalOnly
            } else {
                PackageReachability::Required
            },
        )
        .map_err(|error| anyhow::anyhow!("platform filtering failed: {error}"))?
        {
            PlatformDisposition::Compatible => {}
            PlatformDisposition::SkipOptional(diagnostic) => {
                eprintln!("platform: {}", diagnostic.message);
                continue;
            }
        }
        let integrity = match package.integrity.as_deref() {
            Some(value) => Some(Integrity::parse(value).map_err(|error| {
                anyhow::anyhow!(
                    "package '{}' at {} has invalid integrity \"{value}\": {error}",
                    package.name,
                    package.path
                )
            })?),
            None if frozen => anyhow::bail!(
                "package '{}' at {} has no integrity; cannot verify a frozen install (re-run `bpm import`)",
                package.name,
                package.path
            ),
            None => None,
        };
        work.push(InstallWork {
            path: package.path.clone(),
            name: package.name.clone(),
            url: package.resolved.clone(),
            integrity,
        });
    }
    Ok(work)
}

impl std::fmt::Display for FetchFail {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "install failed for package '{}' from {}: {}",
            self.name, self.url, self.source
        )
    }
}

impl std::error::Error for FetchFail {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::auto_local_project_view;
    use bpm::lockfile::{Lockfile, PackageEntry};

    #[test]
    fn auto_view_detects_next_anywhere_in_the_resolved_graph() {
        let mut lockfile = Lockfile::new("test");
        lockfile.packages.push(PackageEntry {
            path: "node_modules/tools/node_modules/next".into(),
            name: "next".into(),
            version: "15.0.0".into(),
            ..Default::default()
        });

        assert!(auto_local_project_view(&lockfile));
    }

    #[test]
    fn auto_view_stays_relay_for_non_next_graphs() {
        let mut lockfile = Lockfile::new("test");
        lockfile.packages.push(PackageEntry {
            path: "node_modules/vite".into(),
            name: "vite".into(),
            version: "5.4.0".into(),
            ..Default::default()
        });

        assert!(!auto_local_project_view(&lockfile));
    }
}
