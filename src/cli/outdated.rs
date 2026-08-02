//! `bpm outdated` — show packages with newer versions available.
//!
//! Queries the registry for the `latest` dist-tag of each locked package and
//! prints a table of packages whose registry version is newer than the locked
//! version. Registry failures for individual packages are warnings, not fatal
//! errors — the command returns partial results.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::PathBuf;

use bpm::config::NpmConfig;
use bpm::http::HttpClient;
use bpm::lockfile::PackageEntry;
use bpm::metadata_cache::{CacheMode, MetadataCache};
use bpm::project_lock::find_project_lock;
use bpm::registry::RegistryClient;
use semver::Version;
use serde::Serialize;

const MAX_OUTDATED_WORKERS: usize = 16;

fn outdated_worker_count(jobs: usize) -> usize {
    if jobs == 0 {
        return 0;
    }
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(8)
        .min(MAX_OUTDATED_WORKERS)
        .min(jobs)
}

pub(super) fn run(
    target: Option<String>,
    registry: Option<String>,
    store: Option<PathBuf>,
    offline: bool,
    json: bool,
) -> anyhow::Result<()> {
    // 1. Find and load the project lockfile.
    let cwd = env::current_dir()?;
    let project_lock = find_project_lock(&cwd)?
        .ok_or_else(|| anyhow::anyhow!("no lockfile found (bpm.lock or package-lock.json)"))?;

    let lockfile = &project_lock.lockfile;
    let project_root = &project_lock.project_root;

    // 2. Build the registry client with effective config.
    let store_root = store_root_or_else(store)?;

    let home = env::var_os("HOME").map(PathBuf::from);
    let config = NpmConfig::load(project_root, home.as_deref())
        .map_err(|e| anyhow::anyhow!("failed to load npm config: {e}"))?;

    let effective_registry =
        registry.or_else(|| env::var_os("BPM_REGISTRY").map(|s| s.to_string_lossy().into_owned()));
    let config = match effective_registry {
        Some(r) => config
            .with_registry_override(&r)
            .map_err(|e| anyhow::anyhow!("invalid registry override: {e}"))?,
        None => config,
    };

    let cache_mode = if offline {
        CacheMode::Offline
    } else {
        CacheMode::Default
    };

    let http = HttpClient::new(config.clone());

    // Best-effort metadata cache — online modes degrade gracefully.
    let metadata_cache = MetadataCache::open(&store_root).ok();
    let mut client = RegistryClient::with_client(config, http);
    if let Some(cache) = metadata_cache {
        client = client.with_metadata_cache(std::sync::Arc::new(cache), cache_mode);
    }

    // 3. Collect packages to check.
    let packages: Vec<&PackageEntry> = if let Some(ref name) = target {
        let matching: Vec<_> = lockfile
            .packages
            .iter()
            .filter(|p| p.name == *name)
            .collect();
        if matching.is_empty() {
            anyhow::bail!("package '{name}' not found in lockfile");
        }
        matching
    } else {
        lockfile.packages.iter().collect()
    };

    // 4. Fetch one packument per unique package name through a bounded scoped
    // worker pool, then compare every physical placement in lockfile order.
    // `RegistryClient` is shared by reference just as it is by the resolver's
    // own prefetch workers.
    let mut rows: Vec<OutdatedRow> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    /// Outcome of one package query: either the fetched packument, or a
    /// warning string explaining why the query was skipped/failed.
    enum FetchOutcome {
        Packument(bpm::registry::Packument),
        Warning(String),
    }

    // Build the list of packages that actually need a registry query (skip
    // link/workspace packages, and validate the current version up front so
    // we don't spend a round-trip on an unparseable version).
    let mut fetch_targets: Vec<(&PackageEntry, String)> = Vec::new();
    let mut unique_names: BTreeSet<String> = BTreeSet::new();
    for package in &packages {
        if package.link || package.resolved.is_empty() {
            continue;
        }
        if Version::parse(&package.version).is_err() {
            warnings.push(format!(
                "warning: could not parse version '{}' for {}",
                package.version, package.name
            ));
            continue;
        }
        let registry_name = lockfile.registry_name_for(package).to_string();
        fetch_targets.push((package, registry_name.clone()));
        unique_names.insert(registry_name);
    }

    let jobs: Vec<String> = unique_names.into_iter().collect();
    let worker_count = outdated_worker_count(jobs.len());
    let fetched: BTreeMap<String, FetchOutcome> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for worker_index in 0..worker_count {
            let worker_jobs: Vec<String> = jobs
                .iter()
                .skip(worker_index)
                .step_by(worker_count)
                .cloned()
                .collect();
            let client_ref = &client;
            handles.push(scope.spawn(move || {
                worker_jobs
                    .into_iter()
                    .map(|name| {
                        let outcome = match client_ref.packument(&name) {
                            Ok(packument) => FetchOutcome::Packument(packument),
                            Err(error) => FetchOutcome::Warning(format!(
                                "warning: failed to fetch metadata for {name}: {error}"
                            )),
                        };
                        (name, outcome)
                    })
                    .collect::<Vec<_>>()
            }));
        }

        let mut results = BTreeMap::new();
        for handle in handles {
            let worker_results = handle
                .join()
                .map_err(|_| anyhow::anyhow!("outdated metadata worker thread panicked"))?;
            results.extend(worker_results);
        }
        Ok::<_, anyhow::Error>(results)
    })?;

    // Iterate in the original deterministic `packages` order so output is
    // independent of fetch completion order.
    let mut warned_fetches = BTreeSet::new();
    for (package, registry_name) in &fetch_targets {
        let current = Version::parse(&package.version).expect("validated before fetch; qed");

        let packument = match fetched.get(registry_name) {
            Some(FetchOutcome::Packument(p)) => p,
            Some(FetchOutcome::Warning(w)) => {
                if warned_fetches.insert(registry_name.as_str()) {
                    warnings.push(w.clone());
                }
                continue;
            }
            None => continue,
        };

        let latest_str = match packument.dist_tags.get("latest") {
            Some(v) => v.clone(),
            None => {
                warnings.push(format!("warning: no 'latest' dist-tag for {registry_name}"));
                continue;
            }
        };

        let latest = match Version::parse(&latest_str) {
            Ok(v) => v,
            Err(_) => {
                warnings.push(format!(
                    "warning: could not parse 'latest' version '{latest_str}' for {registry_name}"
                ));
                continue;
            }
        };

        // Compute the "wanted" version: highest published version satisfying
        // the declared semver range from the root manifest.
        let wanted = if let Some(range_str) = lockfile.root.dependencies.get(package.name.as_str())
        {
            compute_wanted(registry_name, range_str, packument)
                .unwrap_or_else(|| package.version.clone())
        } else {
            // Transitive dependencies fall back to the resolved version.
            package.version.clone()
        };

        if latest > current || wanted != package.version {
            rows.push(OutdatedRow {
                package: package.name.clone(),
                current: package.version.clone(),
                wanted,
                latest: latest_str,
            });
        }
    }

    // 5. Print output.
    if json {
        let map: BTreeMap<&str, &OutdatedRow> =
            rows.iter().map(|r| (r.package.as_str(), r)).collect();
        println!("{}", serde_json::to_string_pretty(&map)?);
    } else {
        print_table(&rows);
    }

    // Print warnings to stderr.
    for warning in &warnings {
        eprintln!("{warning}");
    }

    if rows.is_empty() && warnings.is_empty() {
        println!("All packages are up to date.");
    }

    Ok(())
}

/// Compute the highest published version satisfying `range_str` for `name`.
fn compute_wanted(
    registry_name: &str,
    range_str: &str,
    packument: &bpm::registry::Packument,
) -> Option<String> {
    use bpm::registry::{parse_spec, select_version};
    let spec = match range_str.strip_prefix("npm:") {
        Some(alias_spec) => parse_spec(alias_spec).ok()?,
        None => parse_spec(&format!("{registry_name}@{range_str}")).ok()?,
    };
    let version = select_version(&spec.name, &spec.req, packument).ok()?;
    Some(version.to_string())
}

/// One row in the outdated table.
#[derive(Debug, Clone, Serialize)]
struct OutdatedRow {
    package: String,
    current: String,
    wanted: String,
    latest: String,
}

/// Print the outdated table in human-readable format, matching npm's convention.
fn print_table(rows: &[OutdatedRow]) {
    println!(
        "{:<24} {:<12} {:<12} {:<12}",
        "Package", "Current", "Wanted", "Latest"
    );
    for row in rows {
        println!(
            "{:<24} {:<12} {:<12} {:<12}",
            row.package, row.current, row.wanted, row.latest
        );
    }
}

/// Resolve the store root from CLI flag, env var, or home directory default.
fn store_root_or_else(store: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    store
        .or_else(|| env::var_os("BPM_STORE").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".bpm")))
        .ok_or_else(|| anyhow::anyhow!("no --store given and $BPM_STORE/$HOME is unset"))
}
