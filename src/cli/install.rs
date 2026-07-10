//! Lockfile and global-bin install orchestration.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use bpm::config::NpmConfig;
use bpm::graph;
use bpm::http::HttpClient;
use bpm::integrity::{ArtifactId, Integrity};
use bpm::lockfile::{LockSource, Lockfile};
use bpm::manifest::PackageManifest;
use bpm::metrics::Metrics;
use bpm::path_safety::{validate_bin_name, validate_bin_target};
use bpm::project_lock::{find_project_lock, validate_npm_direct_install, ProjectLockKind};
use bpm::resolver;
use bpm::resolver::model::PlatformConstraints;
use bpm::resolver::platform::{check_package_platform, PackageReachability, PlatformDisposition};
use bpm::store::{ArtifactStore, StoreError};

use super::fetch::{name_of_spec, open_registry_client, store_root, write_metrics};

pub(super) struct Options {
    pub targets: Vec<String>,
    pub frozen: bool,
    pub registry: Option<String>,
    pub store: Option<PathBuf>,
    pub concurrency: usize,
    pub json_metrics: Option<PathBuf>,
    pub global: bool,
    pub ignore_scripts: bool,
    /// Experimental per-package lifecycle-derived image cache (`--derived-store` / `BPM_DERIVED_STORE`).
    pub derived_store: bool,
    /// Run Git package build-context prepare scripts and consume their images.
    pub git_prepare: bool,
    pub legacy_peer_deps: bool,
    pub cache_mode: bpm::metadata_cache::CacheMode,
    /// Optional verified remote artifact cache endpoint.
    pub remote_cache: Option<String>,
    /// `bpm install -D` / `bpm add --save-dev`: write added packages into
    /// `devDependencies` and remove them from `dependencies`.
    pub save_dev: bool,
    /// `bpm install -E` / `bpm add --save-exact`: save the resolved version as
    /// an exact `X.Y.Z` rather than the default `^X.Y.Z`.
    pub save_exact: bool,
}

pub(super) fn run(mut options: Options) -> anyhow::Result<()> {
    if options.remote_cache.is_none() {
        options.remote_cache = env::var_os("BPM_REMOTE_CACHE")
            .map(PathBuf::from)
            .map(|p| p.to_string_lossy().into_owned());
    }
    // `--derived-store` and `BPM_DERIVED_STORE=1` are equivalent. Honor the
    // env var so callers (and tests) can opt in without a CLI flag, mirroring
    // `BPM_STREAM_INSTALL` / `BPM_PROJECT_VIEW`.
    if !options.derived_store {
        options.derived_store =
            matches!(env::var("BPM_DERIVED_STORE").as_deref(), Ok("1" | "true"));
    }
    if !options.git_prepare {
        options.git_prepare = matches!(env::var("BPM_GIT_PREPARE").as_deref(), Ok("1" | "true"));
    }
    // Global bin linking retains the pre-mutation single-target behavior. Do
    // not silently discard additional targets: users must invoke global
    // installs once per package until multi-package bin ownership exists.
    if options.global {
        match options.targets.as_slice() {
            [] => anyhow::bail!(
                "global install (`-g`) requires a package target; \
                 omit `-g` to install the project lockfile"
            ),
            [target] => {
                return run_global_install(
                    target,
                    options.registry.clone(),
                    options.store.clone(),
                    options.cache_mode,
                    options.remote_cache.as_deref(),
                )
            }
            _ => anyhow::bail!(
                "global install (`-g`) accepts exactly one package target; \
                 run one command per global package"
            ),
        }
    }

    // Local dependency mutation: `bpm install foo` / `bpm add foo` edits
    // package.json, resolves the whole edited graph, writes the selected lock,
    // and installs. Save flags are only meaningful here.
    if !options.targets.is_empty() {
        return super::mutate::run_add(&options);
    }

    let store_root_path = store_root(options.store.clone())?;
    let cwd = env::current_dir()?;
    let (lockfile_path, lockfile, project_root, lock_kind) = match find_project_lock(&cwd)? {
        Some(project_lock) => {
            if project_lock.kind == ProjectLockKind::NpmV3 {
                validate_npm_direct_install(&project_lock.diagnostics)?;
                render_import_diagnostics(&project_lock.diagnostics);
            }
            (
                project_lock.path,
                project_lock.lockfile,
                project_lock.project_root,
                project_lock.kind,
            )
        }
        None if options.frozen => anyhow::bail!(
            "frozen install requires bpm.lock or supported package-lock.json v3 in {} or an ancestor",
            cwd.display()
        ),
        None => {
            let root = bpm::project::find_project_root(&cwd).unwrap_or(cwd.clone());
            let manifest = PackageManifest::from_path(&root.join("package.json"))
                .map_err(|error| anyhow::anyhow!("cannot resolve dependencies: {error}"))?;
            let config = effective_npm_config(&root, options.registry.as_deref())?;
            let http = HttpClient::new(config.clone());
            let client =
                open_registry_client(&store_root_path, config.clone(), http.clone(), options.cache_mode)?;
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
            let store = ArtifactStore::open(&store_root_path)?;
            let mut metrics = Metrics::new();
            // Combined streaming+async: non-blocking resolver feeding the
            // download pipeline through a non-blocking sink adapter.
            if streaming_install_enabled() && async_resolve_enabled() {
                return run_streaming_async_install(
                    &root,
                    &manifest,
                    &workspace_index,
                    peer_mode,
                    options.concurrency,
                    &store,
                    &http,
                    &mut metrics,
                    &options,
                );
            }
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
            // Async resolve: opt-in experimental path using the non-blocking
            // resolver from src/async_resolver.rs. The output bpm.lock is
            // byte-identical to the blocking path; only the I/O model differs.
            if async_resolve_enabled() {
                let lockfile = metrics
                    .measure("dependency_resolution", || {
                        tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .expect("failed to build tokio runtime")
                            .block_on(async {
                                bpm::async_resolver::resolve_manifest_with_workspaces_async(
                                    &manifest,
                                    &bpm::async_resolver::AsyncRegistryClient::new(config),
                                    "bpm",
                                    Some(&workspace_index),
                                )
                                .await
                            })
                    })
                    .map_err(|error| anyhow::anyhow!("dependency resolution failed: {error}"))?;
                let path = root.join(bpm::lockfile::BPM_LOCK_FILE);
                lockfile.write_to(&path)?;
                eprintln!(
                    "resolved {} package(s) (async) and wrote {}",
                    lockfile.packages.len(),
                    path.display()
                );
                (path, lockfile, root, ProjectLockKind::Bpm)
            } else {
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
                (path, lockfile, root, ProjectLockKind::Bpm)
            }
        }
    };
    install_resolved_lockfile(
        &project_root,
        &lockfile_path,
        lockfile,
        lock_kind,
        &options,
        &store_root_path,
    )
}

/// Install an already-resolved lockfile: enforce frozen drift, check the graph
/// plan cache, run the download→extract pipeline, materialize, run lifecycle,
/// and write the install plan. Shared by lockfile-present install and by
/// [`crate::cli::mutate`] (add/remove), which resolve an edited manifest and
/// then hand the resulting graph to this function. Never recursively invokes
/// the BPM binary and never writes a throwaway lock to pass data around.
#[allow(clippy::too_many_arguments)]
pub(super) fn install_resolved_lockfile(
    project_root: &Path,
    lockfile_path: &Path,
    lockfile: Lockfile,
    lock_kind: ProjectLockKind,
    options: &Options,
    store_root_path: &Path,
) -> anyhow::Result<()> {
    let store = ArtifactStore::open(store_root_path)?;
    let mut metrics = Metrics::new();
    if options.frozen {
        enforce_frozen(project_root, &lockfile, lock_kind.filename())?;
    }

    let config = effective_npm_config(project_root, options.registry.as_deref())?;
    let http = HttpClient::new(config.clone());
    let registry = open_registry_client(store_root_path, config, http.clone(), options.cache_mode)?;
    let remote = if options.cache_mode.allows_network() {
        options
            .remote_cache
            .as_deref()
            .map(|base| {
                let token = env::var("BPM_REMOTE_CACHE_TOKEN").ok();
                bpm::remote_cache::RemoteCacheConfig::new(base, token)
                    .map_err(|error| anyhow::anyhow!("invalid remote cache configuration: {error}"))
                    .and_then(|config| {
                        bpm::remote_cache::RemoteCacheClient::new(config).map_err(|error| {
                            anyhow::anyhow!("invalid remote cache configuration: {error}")
                        })
                    })
            })
            .transpose()?
    } else {
        None
    };

    let plan_path = graph::plan_path_for(lockfile_path);
    let cached_plan = graph::read_plan(&plan_path)?;
    let git_prepare_enabled = options.git_prepare
        && lockfile
            .resolution
            .packages
            .values()
            .any(|resolution| matches!(resolution.source, LockSource::Git { .. }));
    let plan_valid = !git_prepare_enabled
        && cached_plan.as_ref().is_some_and(|plan| {
            graph::validate_plan(plan, &lockfile, project_root, &store).is_ok()
        });
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
            graph::graph_id_for_project(&lockfile, project_root).to_hex_short(),
            materialized,
            bins
        );
        metrics.add_requests(http.request_count());
        return write_metrics(&metrics, options.json_metrics.clone());
    }
    metrics.record("plan_cache_miss", std::time::Duration::ZERO);

    let work = build_install_work(&lockfile, options.frozen, lock_kind.filename())?;
    let workers = adaptive_workers(options.concurrency, work.len(), project_root);
    let outcomes = std::thread::scope(|scope| -> anyhow::Result<Vec<FetchOutcome>> {
        let (unit_tx, unit_rx) = std::sync::mpsc::sync_channel::<InstallWork>(workers.max(1) * 2);
        let unit_rx = std::sync::Arc::new(std::sync::Mutex::new(unit_rx));
        let (downloaders, extractors) =
            spawn_fetch_pipeline(scope, &store, &http, remote.as_ref(), unit_rx, workers);
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
    metrics.add_requests(http.request_count());
    finalize_install(
        project_root,
        &store,
        &lockfile,
        &artifact_ids,
        cached,
        fetched,
        &mut metrics,
        options,
        lockfile_path,
        &registry,
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
    derived_store: bool,
    metrics: &mut Metrics,
) -> bpm::lifecycle::LifecycleStats {
    if ignore_scripts {
        metrics.record("lifecycle", std::time::Duration::ZERO);
        return bpm::lifecycle::LifecycleStats::default();
    }
    let policy = bpm::lifecycle::LifecyclePolicy {
        ignore_scripts: false,
        skip_execution,
        use_derived_store: derived_store,
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

fn run_global_install(
    target: &str,
    registry: Option<String>,
    store: Option<PathBuf>,
    cache_mode: bpm::metadata_cache::CacheMode,
    remote_cache: Option<&str>,
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
    let remote = if cache_mode.allows_network() {
        remote_cache
            .map(|base| {
                let token = env::var("BPM_REMOTE_CACHE_TOKEN").ok();
                let config =
                    bpm::remote_cache::RemoteCacheConfig::new(base, token).map_err(|error| {
                        anyhow::anyhow!("invalid remote cache configuration: {error}")
                    })?;
                bpm::remote_cache::RemoteCacheClient::new(config)
                    .map_err(|error| anyhow::anyhow!("invalid remote cache configuration: {error}"))
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
    // Validate all bin names and targets before creating directories or linking.
    for (name, relpath) in &bins {
        validate_bin_name(name)
            .map_err(|e| anyhow::anyhow!("invalid bin name {name:?} in global install: {e}"))?;
        let normalized = relpath.strip_prefix("./").unwrap_or(relpath);
        validate_bin_target(normalized)
            .map_err(|e| anyhow::anyhow!("invalid bin target {normalized:?} in global install: {e}"))?;
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

pub(super) fn effective_npm_config(
    project_root: &Path,
    registry: Option<&str>,
) -> anyhow::Result<NpmConfig> {
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

fn render_import_diagnostics(diagnostics: &[bpm::Diagnostic]) {
    for diagnostic in diagnostics {
        let package = diagnostic
            .package
            .as_deref()
            .map(|value| format!(" (in {value})"))
            .unwrap_or_default();
        eprintln!(
            "{}[{}] {}{}",
            diagnostic.severity.as_str(),
            diagnostic.code,
            diagnostic.message,
            package
        );
    }
}

fn enforce_frozen(
    project_root: &Path,
    lockfile: &Lockfile,
    lock_label: &str,
) -> anyhow::Result<()> {
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
        "frozen install refused: package.json and {lock_label} disagree on root dependencies\n  \
         in package.json but not {lock_label}: {}\n  \
         in {lock_label} but not package.json: {}\n  \
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
        let _ = self.0.send(InstallWork {
            path: unit.path,
            name: unit.name,
            url: unit.url,
            integrity: unit.integrity,
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
    remote: Option<&'env bpm::remote_cache::RemoteCacheClient>,
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
        let remote = remote.cloned();
        downloaders.push(scope.spawn(move || -> Result<Metrics, FetchFail> {
            let mut local = Metrics::new();
            let mut extraction_gone = false;
            while let Ok(item) = unit_rx.lock().expect("unit receiver lock").recv() {
                if extraction_gone {
                    continue;
                }
                let result = if let Some(remote) = remote.as_ref() {
                    store
                        .ensure_artifact_with_remote(
                            &http,
                            remote,
                            &item.url,
                            item.integrity.as_ref(),
                            &mut local,
                        )
                        .map(|result| result.artifact)
                } else {
                    store.ensure_artifact_with_client(
                        &http,
                        &item.url,
                        item.integrity.as_ref(),
                        &mut local,
                    )
                }
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

/// Experimental non-blocking resolver (`BPM_ASYNC_RESOLVE=1`). Uses the async
/// resolver in `src/async_resolver.rs` instead of the blocking one. The
/// resolved `bpm.lock` is byte-for-byte identical to the blocking path; only
/// the I/O model differs. Opt-in while the async path is being measured.
fn async_resolve_enabled() -> bool {
    matches!(
        std::env::var("BPM_ASYNC_RESOLVE").as_deref(),
        Ok("1") | Ok("true")
    )
}

/// A non-blocking variant of [`ChannelSink`] that uses `try_send` instead of
/// `send`, so the async resolver's tokio runtime thread never blocks on
/// pipeline backpressure. Units that cannot be delivered (channel full) are
/// silently dropped and fetched in a post-resolution pass.
struct TryChannelSink(std::sync::mpsc::SyncSender<InstallWork>);

impl resolver::ResolveSink for TryChannelSink {
    fn emit(&self, unit: resolver::ResolvedDownloadUnit) {
        let _ = self.0.try_send(InstallWork {
            path: unit.path,
            name: unit.name,
            url: unit.url,
            integrity: unit.integrity,
        });
    }
}

/// Resolve a fresh manifest with the async resolver while the download/extract
/// pipeline fetches each package via the non-blocking sink.  Any packages that
/// the pipeline missed (channel was full during resolution) are fetched in a
/// post-resolution sequential pass.
#[allow(clippy::too_many_arguments)]
fn run_streaming_async_install(
    root: &Path,
    manifest: &PackageManifest,
    workspace_index: &bpm::resolver::workspaces::WorkspaceIndex,
    _peer_mode: bpm::resolver::peer::PeerMode,
    concurrency: usize,
    store: &ArtifactStore,
    http: &HttpClient,
    metrics: &mut Metrics,
    options: &Options,
) -> anyhow::Result<()> {
    let config = effective_npm_config(root, options.registry.as_deref())?;
    let remote = if options.cache_mode.allows_network() {
        options
            .remote_cache
            .as_deref()
            .map(|base| {
                let token = env::var("BPM_REMOTE_CACHE_TOKEN").ok();
                bpm::remote_cache::RemoteCacheConfig::new(base, token)
                    .map_err(|error| anyhow::anyhow!("invalid remote cache configuration: {error}"))
                    .and_then(|config| {
                        bpm::remote_cache::RemoteCacheClient::new(config).map_err(|error| {
                            anyhow::anyhow!("invalid remote cache configuration: {error}")
                        })
                    })
            })
            .transpose()?
    } else {
        None
    };
    let workers = adaptive_workers(concurrency, usize::MAX, root);
    let (lockfile, mut outcomes) =
        std::thread::scope(|scope| -> anyhow::Result<(Lockfile, Vec<FetchOutcome>)> {
            let (unit_tx, unit_rx) =
                std::sync::mpsc::sync_channel::<InstallWork>(workers.max(1) * 2);
            let unit_rx = std::sync::Arc::new(std::sync::Mutex::new(unit_rx));
            let (downloaders, extractors) =
                spawn_fetch_pipeline(scope, store, http, remote.as_ref(), unit_rx, workers);
            // Run async resolution on a tokio runtime, emitting placed nodes
            // to the non-blocking TryChannelSink.
            let config_clone = config.clone();
            let lockfile = metrics
                .measure("dependency_resolution", || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("failed to build tokio runtime")
                        .block_on(async {
                            let sink = TryChannelSink(unit_tx);
                            let registry =
                                bpm::async_resolver::AsyncRegistryClient::new(config_clone);
                            bpm::async_resolver::resolve_manifest_with_workspaces_async_sink(
                                manifest,
                                &registry,
                                "bpm",
                                Some(workspace_index),
                                Some(&sink as &dyn resolver::ResolveSink),
                            )
                            .await
                        })
                })
                .map_err(|error| anyhow::anyhow!("dependency resolution failed: {error}"))?;
            // Record async resolver diagnostics. The sync resolver counters
            // (resolver_cache_waits, prefetch_fetches, packument_bytes) do not
            // apply to the async path.
            let outcomes = join_pipeline(downloaders, extractors, metrics)?;
            Ok((lockfile, outcomes))
        })?;
    // Post-resolution pass: fetch any packages the pipeline missed.
    fetch_missing_outcomes(&lockfile, &mut outcomes, store, http, metrics)?;
    let path = root.join(bpm::lockfile::BPM_LOCK_FILE);
    lockfile.write_to(&path)?;
    eprintln!(
        "resolved {} package(s) (async+streaming) and wrote {}",
        lockfile.packages.len(),
        path.display()
    );
    metrics.record("plan_cache_miss", std::time::Duration::ZERO);
    let cached = outcomes
        .iter()
        .filter(|outcome| outcome.artifact_cached && outcome.image_cached)
        .count();
    let fetched = outcomes.len() - cached;
    let artifact_ids = outcomes_to_artifact_ids(&outcomes, &lockfile);
    metrics.add_requests(http.request_count());
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
        &bpm::registry::RegistryClient::new(config),
    )
}

/// Fetch any packages in `lockfile` that are missing from `outcomes` and
/// append their fetch results.  Used by the combined streaming+async path
/// when the non-blocking sink dropped units due to channel backpressure.
fn fetch_missing_outcomes(
    lockfile: &Lockfile,
    outcomes: &mut Vec<FetchOutcome>,
    store: &ArtifactStore,
    http: &HttpClient,
    metrics: &mut Metrics,
) -> anyhow::Result<()> {
    // Collect present paths into an owned set so the immutable borrow on
    // `outcomes` is dropped before the mutable push below.
    let present: BTreeSet<String> = outcomes.iter().map(|o| o.path.clone()).collect();
    let mut missing = 0usize;
    for package in &lockfile.packages {
        if package.link || package.resolved.is_empty() || present.contains(&package.path) {
            continue;
        }
        let integrity = package
            .integrity
            .as_deref()
            .map(|v| Integrity::parse(v).map_err(|e| {
                anyhow::anyhow!(
                    "package '{}' at {} has invalid integrity \"{}\": {}",
                    package.name,
                    package.path,
                    v,
                    e
                )
            }))
            .transpose()?;
        let artifact = store.ensure_artifact_with_client(
            http,
            &package.resolved,
            integrity.as_ref(),
            metrics,
        )?;
        let image = store.ensure_image(&artifact.id, metrics)?;
        outcomes.push(FetchOutcome {
            path: package.path.clone(),
            id: artifact.id,
            artifact_cached: artifact.cached,
            image_cached: image.cached,
        });
        missing += 1;
    }
    if missing > 0 {
        metrics.record(
            "post_resolution_fetches",
            std::time::Duration::from_nanos(missing as u64),
        );
    }
    Ok(())
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
    let remote = if options.cache_mode.allows_network() {
        options
            .remote_cache
            .as_deref()
            .map(|base| {
                let token = env::var("BPM_REMOTE_CACHE_TOKEN").ok();
                bpm::remote_cache::RemoteCacheConfig::new(base, token)
                    .map_err(|error| anyhow::anyhow!("invalid remote cache configuration: {error}"))
                    .and_then(|config| {
                        bpm::remote_cache::RemoteCacheClient::new(config).map_err(|error| {
                            anyhow::anyhow!("invalid remote cache configuration: {error}")
                        })
                    })
            })
            .transpose()?
    } else {
        None
    };
    // Work count is unknown until resolution completes, so do not clamp the
    // worker count to it (usize::MAX makes adaptive_workers' clamp a no-op).
    let workers = adaptive_workers(concurrency, usize::MAX, root);
    let (lockfile, outcomes) =
        std::thread::scope(|scope| -> anyhow::Result<(Lockfile, Vec<FetchOutcome>)> {
            let (unit_tx, unit_rx) =
                std::sync::mpsc::sync_channel::<InstallWork>(workers.max(1) * 2);
            let unit_rx = std::sync::Arc::new(std::sync::Mutex::new(unit_rx));
            let (downloaders, extractors) =
                spawn_fetch_pipeline(scope, store, http, remote.as_ref(), unit_rx, workers);
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
            // Of the `dependency_resolution` wall time, how much the resolver
            // thread blocked on network (packument fetches + waiting on
            // in-flight prefetches). The remainder is CPU: parse, placement,
            // peer backtracking, lockfile generation.
            let resolver_net_wait_ns = bpm::registry::take_resolver_fetch_nanos();
            metrics.record(
                "resolver_network_wait",
                std::time::Duration::from_nanos(resolver_net_wait_ns),
            );
            let (hits, waits, inline, prefetch) = bpm::registry::take_resolver_diagnostics();
            let packument_bytes = bpm::registry::take_resolver_fetch_bytes();
            metrics.record_resolver_diagnostics(
                hits,
                waits,
                inline,
                prefetch,
                packument_bytes,
                resolver_net_wait_ns,
            );
            // Record batch-prefetch closure count (packuments fetched before
            // DFS started, separate from inline and pool prefetches).
            let batch_fetches = bpm::registry::take_batch_prefetch_fetches();
            metrics.record_batch_prefetch(batch_fetches);
            // Also record the HTTP transport diagnostics: whether HTTP/2 was
            // observed and the peak in-flight request concurrency.
            metrics.record(
                "http_observed_http2",
                if http.observed_http2() {
                    std::time::Duration::from_nanos(1)
                } else {
                    std::time::Duration::ZERO
                },
            );
            metrics.record(
                "http_peak_concurrency",
                std::time::Duration::from_nanos(http.max_concurrent_requests()),
            );
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
    metrics.add_requests(http.request_count());
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
        client,
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
    registry: &bpm::registry::RegistryClient,
) -> anyhow::Result<()> {
    let git_prepare_enabled = options.git_prepare
        && lockfile
            .resolution
            .packages
            .values()
            .any(|resolution| matches!(resolution.source, LockSource::Git { .. }));
    let has_workspace_links = lockfile.packages.iter().any(|package| package.link);
    // Nested `file:` dependencies need a graph volume: direct symlink
    // materialization cannot place a child under an immutable package image.
    // The volume copies those source links into its image. Top-level workspace
    // links retain the existing direct-materialization path.
    let has_nested_links = lockfile
        .packages
        .iter()
        .any(|package| package.link && package.path.contains("/node_modules/"));
    let direct_materialization = has_workspace_links && !has_nested_links;
    let prepared = if git_prepare_enabled && !options.ignore_scripts && !direct_materialization {
        bpm::lifecycle::prepare_git_packages(
            project_root,
            store,
            lockfile,
            artifact_ids,
            registry,
            metrics,
        )?
    } else {
        BTreeMap::new()
    };
    // Turbopack and similar bundlers enforce that dependency realpaths remain
    // inside the project. Keep the O(top-level) relay fast path for ordinary
    // projects, but use a local hardlink view automatically for Next projects;
    // callers can override this with BPM_PROJECT_VIEW=relay|local.
    let local_project_view = use_local_project_view(lockfile);
    let (volume, mut view_entry_count) = if direct_materialization {
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
        let volume = bpm::volume::ensure_graph_volume_with_prepared(
            store,
            lockfile,
            artifact_ids,
            &prepared,
            metrics,
        )?;
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
        options.derived_store,
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
    let prepared_keys = prepared
        .iter()
        .map(|(path, image)| (path.clone(), *image.key.as_bytes()))
        .collect::<BTreeMap<_, _>>();
    plan.graph_id_hex =
        graph::graph_id_for_project_with_prepared(lockfile, project_root, &prepared_keys).to_hex();
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

fn build_install_work(
    lockfile: &Lockfile,
    frozen: bool,
    lock_label: &str,
) -> anyhow::Result<Vec<InstallWork>> {
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
                "package '{}' at {} in {lock_label} has no integrity; cannot verify a frozen install",
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
