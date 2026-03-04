//! Lockfile and global-bin install orchestration.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use bpm::config::NpmConfig;
use bpm::graph;
use bpm::http::HttpClient;
use bpm::integrity::{ArtifactId, Integrity};
use bpm::lockfile::{find_lockfile, Lockfile};
use bpm::manifest::PackageManifest;
use bpm::metrics::Metrics;
use bpm::registry::RegistryClient;
use bpm::resolver;
use bpm::store::{ArtifactStore, StoreError};

use super::fetch::{name_of_spec, store_root, write_metrics};

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
}

pub(super) fn run(options: Options) -> anyhow::Result<()> {
    if let Some(target) = options.target {
        return run_install_bin(&target, options.registry, options.store, options.global);
    }

    let store = ArtifactStore::open(&store_root(options.store)?)?;
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
            let client = RegistryClient::with_client(config, http);
            let workspace_layout = bpm::workspace::discover(&root);
            let workspace_index = bpm::resolver::workspaces::WorkspaceIndex::from_project_root(
                &root,
                &workspace_layout,
            )
            .map_err(|error| anyhow::anyhow!("workspace resolution failed: {error}"))?;
            let lockfile = resolver::resolve_manifest_with_options(
                &manifest,
                &client,
                "bpm",
                Some(&workspace_index),
                if options.legacy_peer_deps {
                    bpm::resolver::peer::PeerMode::LegacyIgnore
                } else {
                    bpm::resolver::peer::PeerMode::Strict
                },
            )
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

    let mut metrics = Metrics::new();
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
    let outcomes: Vec<FetchOutcome> =
        std::thread::scope(|scope| -> anyhow::Result<Vec<FetchOutcome>> {
            use std::sync::mpsc::sync_channel;
            let (send, receive) = sync_channel::<Result<PendingArtifact, FetchFail>>(workers * 2);
            let receive = std::sync::Arc::new(std::sync::Mutex::new(receive));
            let next = std::sync::Arc::new(AtomicUsize::new(0));
            let mut downloaders = Vec::new();
            for _ in 0..workers {
                let send = send.clone();
                let next = next.clone();
                let work = &work;
                let store = &store;
                let http = http.clone();
                downloaders.push(scope.spawn(move || -> Result<Metrics, FetchFail> {
                    let mut local = Metrics::new();
                    loop {
                        let position = next.fetch_add(1, Ordering::Relaxed);
                        if position >= work.len() {
                            break;
                        }
                        let item = &work[position];
                        let result = store
                            .ensure_artifact_with_client(
                                &http,
                                &item.url,
                                item.integrity.as_ref(),
                                &mut local,
                            )
                            .map(|artifact| PendingArtifact {
                                index: item.index,
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
                            break;
                        }
                    }
                    Ok(local)
                }));
            }
            drop(send);
            let extraction_workers = workers.min(work.len().max(1));
            let mut extractors = Vec::new();
            for _ in 0..extraction_workers {
                let receive = receive.clone();
                let store = &store;
                extractors.push(scope.spawn(
                    move || -> Result<(Vec<FetchOutcome>, Metrics), FetchFail> {
                        let mut local = Metrics::new();
                        let mut outcomes = Vec::new();
                        loop {
                            let message = receive.lock().expect("pipeline receiver lock").recv();
                            let Ok(message) = message else {
                                break;
                            };
                            let pending = message?;
                            let image = store
                                .ensure_image(&pending.artifact.id, &mut local)
                                .map_err(|source| FetchFail {
                                    name: pending.name.clone(),
                                    url: pending.url.clone(),
                                    source: Box::new(source),
                                })?;
                            outcomes.push(FetchOutcome {
                                index: pending.index,
                                id: pending.artifact.id,
                                artifact_cached: pending.artifact.cached,
                                image_cached: image.cached,
                            });
                        }
                        Ok((outcomes, local))
                    },
                ));
            }
            for handle in downloaders {
                metrics.extend(
                    &handle
                        .join()
                        .map_err(|_| anyhow::anyhow!("download worker panicked"))??,
                );
            }
            let mut outcomes = Vec::with_capacity(work.len());
            for handle in extractors {
                let (mut values, local) = handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("extract worker panicked"))??;
                metrics.extend(&local);
                outcomes.append(&mut values);
            }
            Ok(outcomes)
        })?;

    let mut outcomes = outcomes;
    outcomes.sort_by_key(|outcome| outcome.index);
    let cached = outcomes
        .iter()
        .filter(|outcome| outcome.artifact_cached && outcome.image_cached)
        .count();
    let fetched = outcomes.len() - cached;
    let mut artifact_ids = vec![None; lockfile.packages.len()];
    for outcome in &outcomes {
        if outcome.index < artifact_ids.len() {
            artifact_ids[outcome.index] = Some(outcome.id);
        }
    }

    let has_workspace_links = lockfile.packages.iter().any(|package| package.link);
    let (volume, relay_count) = if has_workspace_links {
        bpm::materializer::materialize_lockfile(
            &project_root,
            &store,
            &lockfile,
            &artifact_ids,
            bpm::materializer::MaterializeMode::Compatible,
        )?;
        (None, 0usize)
    } else {
        let volume =
            bpm::volume::ensure_graph_volume(&store, &lockfile, &artifact_ids, &mut metrics)?;
        let attach = bpm::volume::attach_project(&project_root, &volume)?;
        (
            Some(volume),
            attach.relays_created + attach.relays_unchanged,
        )
    };
    run_lifecycle_if_enabled(
        &project_root,
        &store,
        &lockfile,
        &artifact_ids,
        options.ignore_scripts,
        &mut metrics,
    );

    let mut plan = graph::build_plan(&lockfile, &artifact_ids);
    plan.graph_id_hex = graph::graph_id_for_project(&lockfile, &project_root).to_hex();
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
        "installed {} package(s) into {} ({} cached, {} fetched; graph volume {}, {} relay(s))",
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
        relay_count
    );
    write_metrics(&metrics, options.json_metrics)
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

fn run_lifecycle_if_enabled(
    project_root: &Path,
    store: &ArtifactStore,
    lockfile: &Lockfile,
    artifact_ids: &[Option<ArtifactId>],
    ignore_scripts: bool,
    metrics: &mut Metrics,
) {
    if ignore_scripts {
        metrics.record("lifecycle", std::time::Duration::ZERO);
        return;
    }
    let policy = bpm::lifecycle::LifecyclePolicy {
        ignore_scripts: false,
    };
    match bpm::lifecycle::run_lifecycle(
        project_root,
        store,
        lockfile,
        artifact_ids,
        policy,
        metrics,
    ) {
        Ok(result) if result.packages_with_scripts > 0 => {
            eprintln!(
                "lifecycle: {} package(s) with scripts ({} phase(s) executed, {} succeeded, {} failed)",
                result.packages_with_scripts,
                result.phases_executed,
                result.phases_succeeded,
                result.phases_failed
            );
            for outcome in result.outcomes {
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
        }
        Ok(_) => {}
        Err(error) => eprintln!("warning: lifecycle phase failed: {error}"),
    }
}

fn run_install_bin(
    target: &str,
    registry: Option<String>,
    store: Option<PathBuf>,
    _global: bool,
) -> anyhow::Result<()> {
    let store = ArtifactStore::open(&store_root(store)?)?;
    let mut metrics = Metrics::new();
    let cwd = env::current_dir()?;
    let project_root = bpm::project::find_project_root(&cwd).unwrap_or(cwd);
    let config = effective_npm_config(&project_root, registry.as_deref())?;
    let http = HttpClient::new(config.clone());
    let registry_client = RegistryClient::with_client(config, http.clone());

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
    let mut declared = BTreeSet::new();
    declared.extend(manifest.dependencies.keys().cloned());
    declared.extend(manifest.dev_dependencies.keys().cloned());
    let locked: BTreeSet<String> = lockfile.root.dependencies.keys().cloned().collect();
    if declared == locked {
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
    index: usize,
    name: String,
    url: String,
    integrity: Option<Integrity>,
}

struct FetchOutcome {
    index: usize,
    id: ArtifactId,
    artifact_cached: bool,
    image_cached: bool,
}

struct PendingArtifact {
    index: usize,
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

fn build_install_work(lockfile: &Lockfile, frozen: bool) -> anyhow::Result<Vec<InstallWork>> {
    let mut work = Vec::new();
    for (index, package) in lockfile.packages.iter().enumerate() {
        if package.link || package.resolved.is_empty() {
            continue;
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
            index,
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
