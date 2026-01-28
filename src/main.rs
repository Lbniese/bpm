//! `bpm` command-line entry point.
//!
//! Commands:
//!
//! * `bpm --version` — prints the built-in package version (handled by clap).
//! * `bpm doctor` — inspects the nearest `package.json` and reports
//!   structured diagnostics.
//! * `bpm fetch <url>` — downloads a package tarball by exact URL, verifies
//!   its SHA-512 integrity, stores it immutably, and (by default) extracts it
//!   into a package image. Repeated `fetch` performs no network or extraction
//!   work (Milestone 1 artifact-store prototype).
//! * `bpm install --frozen` — reads `bpm.lock`, fetches+verifies+extracts every
//!   locked package through the global artifact store, and materializes
//!   `node_modules` with linked executable bins (Milestone 2 frozen installer).
//!
//! Dependency resolution, reusable graph volumes, lifecycle scripts, and the
//! full store database arrive in later milestones.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};

use clap::{Parser, Subcommand};
use serde::Serialize;

use bpm::bench::{self, ScenarioKind, Tool};
use bpm::doctor::run as doctor_run;
use bpm::integrity::{ArtifactId, Integrity};
use bpm::lockfile::{find_lockfile, Lockfile, BPM_LOCK_FILE};
use bpm::manifest::PackageManifest;
use bpm::materializer::materialize;
use bpm::metrics::Metrics;
use bpm::npm_lock::{import as import_lock, ImportReport};
use bpm::store::{ArtifactStore, StoreError};

#[derive(Debug, Parser)]
#[command(
    name = "bpm",
    bin_name = "bpm",
    about = "Bloom Package Manager: an npm-compatible, performance-focused package installer",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Diagnose the current project's package.json.
    Doctor {
        /// Emit machine-readable JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Download, verify, store, and extract a package tarball by exact URL.
    Fetch {
        /// Exact tarball URL to download.
        url: String,
        /// Expected integrity string (`sha512-<base64>`). Recommended; enables
        /// verification and cache-hit reuse without re-downloading.
        #[arg(long)]
        integrity: Option<String>,
        /// Store root (defaults to `$BPM_STORE` or `$HOME/.bpm`).
        #[arg(long)]
        store: Option<PathBuf>,
        /// Do not extract the package image (archive only).
        #[arg(long = "no-extract")]
        no_extract: bool,
        /// Write phase metrics as canonical JSON to `PATH`.
        #[arg(long = "json-metrics")]
        json_metrics: Option<PathBuf>,
    },
    /// Run benchmark scenarios and report timing statistics.
    Bench {
        /// Fixture to benchmark (list to show available).
        #[arg(long, default_value = "minimal")]
        fixture: String,
        /// Scenario to run (default: all).
        #[arg(long)]
        scenario: Option<String>,
        /// Tools to include (comma-separated, default: npm,pnpm,bpm).
        #[arg(long, default_value = "npm,pnpm,bpm")]
        tools: String,
        /// Number of iterations per scenario.
        #[arg(long, default_value_t = 3)]
        runs: usize,
        /// Write JSON results to PATH instead of text.
        #[arg(long)]
        json: Option<PathBuf>,
        /// Write a baseline JSON file to <dir>/<machine>-<date>.json for later
        /// comparison, and print the path. Implies --json output saved to disk.
        #[arg(long = "save-baseline")]
        save_baseline: Option<PathBuf>,
        /// List available scenarios and fixtures.
        #[arg(long)]
        list: bool,
    },
    /// Import an npm `package-lock.json` and emit a canonical `bpm.lock`.
    Import {
        /// Input lockfile path (defaults to `./package-lock.json`).
        path: Option<PathBuf>,
        /// Output `bpm.lock` path (defaults to `<input dir>/bpm.lock`).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Emit machine-readable JSON (lockfile + diagnostics) to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Install the locked dependency graph from `bpm.lock` into `node_modules`.
    Install {
        /// Require `package.json` and `bpm.lock` to agree; never change versions.
        #[arg(long)]
        frozen: bool,
        /// Store root (defaults to `$BPM_STORE` or `$HOME/.bpm`).
        #[arg(long)]
        store: Option<PathBuf>,
        /// Max concurrent fetch + extract workers.
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
        /// Write phase metrics as canonical JSON to `PATH`.
        #[arg(long = "json-metrics")]
        json_metrics: Option<PathBuf>,
        /// Do not run lifecycle scripts (no-op for now; scripts arrive later).
        #[arg(long)]
        ignore_scripts: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Commands::Doctor { json } => match run_doctor(json) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err:#}");
                ExitCode::FAILURE
            }
        },
        Commands::Fetch {
            url,
            integrity,
            store,
            no_extract,
            json_metrics,
        } => match run_fetch(&url, integrity, store, no_extract, json_metrics) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err:#}");
                ExitCode::FAILURE
            }
        },
        Commands::Bench {
            fixture,
            scenario,
            tools,
            runs,
            json,
            save_baseline,
            list,
        } => match run_bench(fixture, scenario, tools, runs, json, save_baseline, list) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err:#}");
                ExitCode::FAILURE
            }
        },
        Commands::Import { path, out, json } => match run_import(path, out, json) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err:#}");
                ExitCode::FAILURE
            }
        },
        Commands::Install {
            frozen,
            store,
            concurrency,
            json_metrics,
            ignore_scripts,
        } => match run_install(frozen, store, concurrency, json_metrics, ignore_scripts) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err:#}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Locate the project from the current directory and print a doctor report.
fn run_doctor(json: bool) -> anyhow::Result<()> {
    let start = env::current_dir()?;
    let report = doctor_run(&start);

    if json {
        println!("{}", report.render_json());
    } else {
        print!("{}", report.render_text());
    }

    if report.has_error() {
        anyhow::bail!("doctor reported one or more errors");
    }
    Ok(())
}

fn run_fetch(
    url: &str,
    integrity: Option<String>,
    store: Option<PathBuf>,
    no_extract: bool,
    json_metrics: Option<PathBuf>,
) -> anyhow::Result<()> {
    let store_root = store
        .or_else(|| env::var_os("BPM_STORE").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".bpm")))
        .ok_or_else(|| anyhow::anyhow!("no --store given and $BPM_STORE/$HOME is unset"))?;

    let store = ArtifactStore::open(&store_root)?;
    let integrity = integrity.map(|s| Integrity::parse(&s)).transpose()?;

    let mut metrics = Metrics::new();
    let artifact = store.ensure_artifact(url, integrity.as_ref(), &mut metrics)?;
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

    if trace_enabled() {
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

fn run_bench(
    fixture_name: String,
    scenario_name: Option<String>,
    tools_str: String,
    runs: usize,
    json_out: Option<PathBuf>,
    save_baseline: Option<PathBuf>,
    list: bool,
) -> anyhow::Result<()> {
    if list {
        println!("Available scenarios:");
        for s in ScenarioKind::all() {
            println!("  {:<18} {}", s.name(), s.describe());
        }
        println!();
        println!("Available fixtures:");
        for f in bench::FIXTURES {
            println!("  {:<18} packages: {}", f.name, f.packages.join(" "));
        }
        return Ok(());
    }

    let fixture = bench::FIXTURES
        .iter()
        .find(|f| f.name == fixture_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown fixture '{}'; use --list to see available",
                fixture_name
            )
        })?;

    let scenarios: Vec<ScenarioKind> = if let Some(name) = scenario_name {
        let kind = ScenarioKind::all()
            .into_iter()
            .find(|s| s.name() == name)
            .ok_or_else(|| {
                anyhow::anyhow!("unknown scenario '{}'; use --list to see available", name)
            })?;
        vec![kind]
    } else {
        ScenarioKind::all()
    };

    let tools: Vec<Tool> = tools_str
        .split(',')
        .map(|s| match s.trim() {
            "npm" => Ok(Tool::Npm),
            "pnpm" => Ok(Tool::Pnpm),
            "bpm" => Ok(Tool::Bpm),
            other => Err(anyhow::anyhow!("unknown tool '{other}'")),
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .filter(|&c| {
            if c.detect() {
                true
            } else {
                eprintln!("warning: {} not found on $PATH, skipping", c.name());
                false
            }
        })
        .collect();

    if tools.is_empty() {
        anyhow::bail!("no tools available (tried: {tools_str})");
    }

    if runs < 1 {
        anyhow::bail!("--runs must be at least 1");
    }

    eprintln!(
        "benchmark: fixture={fixture_name}, scenarios={}, tools={}, runs={runs}",
        scenarios.len(),
        tools.len(),
    );
    eprintln!();

    let suite = bench::run_suite(&scenarios, fixture, &tools, runs)?;

    if let Some(dir) = save_baseline {
        // Baseline files are machine + date stamped so results from different
        // hosts do not clobber each other: <dir>/<machine>-<yyyymmdd>.json
        fs::create_dir_all(&dir)
            .map_err(|e| anyhow::anyhow!("failed to create baseline dir {}: {e}", dir.display()))?;
        let machine = bench::SystemInfo::capture().machine;
        let date = chrono_like_ymd();
        let stem = format!("{}-{}.json", slugify(&machine), date);
        let path = dir.join(&stem);
        fs::write(&path, suite.to_json())
            .map_err(|e| anyhow::anyhow!("failed to write baseline {}: {e}", path.display()))?;
        println!("baseline written to {}", path.display());
    } else if let Some(path) = json_out {
        fs::write(&path, suite.to_json())
            .map_err(|e| anyhow::anyhow!("failed to write JSON to {}: {e}", path.display()))?;
        eprintln!("results written to {}", path.display());
    } else {
        suite.print_text();
    }

    Ok(())
}

/// Today's date as `YYYYMMDD` without pulling in a date crate (UTC, good
/// enough for a baseline filename stamp).
fn chrono_like_ymd() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    // Days since 1970-01-01 -> civil date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}")
}

/// Reduce a machine string to a filesystem-safe slug for a baseline filename.
fn slugify(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn trace_enabled() -> bool {
    matches!(
        env::var("BPM_TRACE").ok().as_deref(),
        Some("1") | Some("true")
    )
}

#[derive(Serialize)]
struct ImportJson<'a> {
    wrote: String,
    package_count: usize,
    diagnostics: &'a [bpm::Diagnostic],
    lockfile: &'a Lockfile,
}

/// Import an npm package-lock into a canonical `bpm.lock`.
fn run_import(path: Option<PathBuf>, out: Option<PathBuf>, json: bool) -> anyhow::Result<()> {
    let input = path.unwrap_or_else(|| PathBuf::from("package-lock.json"));
    let text = fs::read_to_string(&input)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", input.display()))?;

    let ImportReport {
        lockfile,
        diagnostics,
    } = import_lock(&text)?;

    let out_path = out.unwrap_or_else(|| {
        input
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join(BPM_LOCK_FILE))
            .unwrap_or_else(|| PathBuf::from(BPM_LOCK_FILE))
    });
    lockfile.write_to(&out_path)?;

    if json {
        let payload = ImportJson {
            wrote: out_path.display().to_string(),
            package_count: lockfile.packages.len(),
            diagnostics: &diagnostics,
            lockfile: &lockfile,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .map_err(|e| anyhow::anyhow!("failed to serialize import result: {e}"))?
        );
    } else {
        println!(
            "imported {} packages into {}",
            lockfile.packages.len(),
            out_path.display()
        );
        for d in &diagnostics {
            let pkg = d
                .package
                .as_deref()
                .map(|p| format!(" (in {p})"))
                .unwrap_or_default();
            eprintln!("{}[{}] {}{}", d.severity.as_str(), d.code, d.message, pkg);
        }
    }

    Ok(())
}

/// Install the dependency graph described by the nearest `bpm.lock`.
///
/// Fetches+extracts every locked package through the global artifact store
/// (bounded concurrency), then materializes `node_modules` and links bins.
/// With `--frozen`, refuses if `package.json` and `bpm.lock` disagree on the
/// root dependency set (IMPLEMENTATION §17, §18).
fn run_install(
    frozen: bool,
    store: Option<PathBuf>,
    concurrency: usize,
    json_metrics: Option<PathBuf>,
    ignore_scripts: bool,
) -> anyhow::Result<()> {
    // --ignore-scripts is accepted but lifecycle scripts arrive in a later
    // milestone (M5); for now it is an acknowledged no-op.
    let _ = ignore_scripts;

    let store_root = store
        .or_else(|| env::var_os("BPM_STORE").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".bpm")))
        .ok_or_else(|| anyhow::anyhow!("no --store given and $BPM_STORE/$HOME is unset"))?;
    let store = ArtifactStore::open(&store_root)?;

    let cwd = env::current_dir()?;
    let (lockfile_path, lockfile) = match find_lockfile(&cwd)? {
        Some(found) => found,
        None => anyhow::bail!(
            "no bpm.lock found in {} or any parent (run `bpm import` first)",
            cwd.display()
        ),
    };
    let project_root = lockfile_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    if frozen {
        enforce_frozen(&project_root, &lockfile)?;
    }

    // Build the fetchable work list in lockfile order (deterministic). Each
    // item carries its package index so outcomes can be re-sorted into lockfile
    // order before materialization.
    let work = build_install_work(&lockfile, frozen)?;

    let mut metrics = Metrics::new();
    let mut cached = 0usize;
    let mut fetched = 0usize;

    // Fetch + extract with bounded concurrency. Each worker owns its own
    // Metrics (ArtifactStore methods take &self + &mut Metrics), and borrows
    // the shared store + work list via the scope. Outcomes come back owned.
    let n_workers = concurrency.max(1).min(work.len().max(1));
    let next = AtomicUsize::new(0);
    let outcomes: Vec<FetchOutcome> =
        std::thread::scope(|s| -> anyhow::Result<Vec<FetchOutcome>> {
            let handles: Vec<_> = (0..n_workers)
                .map(|_| {
                    let next = &next;
                    let work = &work;
                    let store = &store;
                    s.spawn(move || -> Result<(Vec<FetchOutcome>, Metrics), FetchFail> {
                        let mut local = Metrics::new();
                        let mut out = Vec::new();
                        loop {
                            let pos = next.fetch_add(1, Ordering::Relaxed);
                            if pos >= work.len() {
                                break;
                            }
                            let item = &work[pos];
                            let artifact = store
                                .ensure_artifact(&item.url, item.integrity.as_ref(), &mut local)
                                .map_err(|source| FetchFail {
                                    name: item.name.clone(),
                                    url: item.url.clone(),
                                    source: Box::new(source),
                                })?;
                            let id = artifact.id;
                            let art_cached = artifact.cached;
                            let image = store.ensure_image(&id, &mut local).map_err(|source| {
                                FetchFail {
                                    name: item.name.clone(),
                                    url: item.url.clone(),
                                    source: Box::new(source),
                                }
                            })?;
                            out.push(FetchOutcome {
                                idx: item.idx,
                                id,
                                art_cached,
                                img_cached: image.cached,
                            });
                        }
                        Ok((out, local))
                    })
                })
                .collect();

            let mut all = Vec::with_capacity(work.len());
            for h in handles {
                let (mut out, local_metrics) = h
                    .join()
                    .map_err(|_| anyhow::anyhow!("install worker thread panicked"))??;
                metrics.extend(&local_metrics);
                all.append(&mut out);
            }
            Ok(all)
        })?;

    // Restore deterministic lockfile order before materializing.
    let mut outcomes = outcomes;
    outcomes.sort_by_key(|o| o.idx);
    for o in &outcomes {
        if o.art_cached && o.img_cached {
            cached += 1;
        } else {
            fetched += 1;
        }
    }

    // Materialize (sequential): pair each package entry with its ArtifactId.
    let resolved: Vec<(_, ArtifactId)> = outcomes
        .iter()
        .map(|o| (&lockfile.packages[o.idx], o.id))
        .collect();
    let mat_stats = materialize(&project_root, &store, &resolved)?;

    println!(
        "installed {} package(s) into {} ({} cached, {} fetched; {} bin(s) linked, {} collision(s))",
        mat_stats.packages_materialized,
        project_root.join("node_modules").display(),
        cached,
        fetched,
        mat_stats.bins_linked,
        mat_stats.bins_collisions,
    );

    if trace_enabled() {
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

/// `--frozen` drift guard: refuse if the set of root dependency names declared
/// in `package.json` (`dependencies` ∪ `devDependencies`) differs from the set
/// recorded in `bpm.lock`'s root entry. A missing/unreadable `package.json` is
/// treated as a warning, not an error (a project may ship only a lockfile).
fn enforce_frozen(project_root: &Path, lockfile: &Lockfile) -> anyhow::Result<()> {
    let manifest_path = project_root.join("package.json");
    let manifest = match PackageManifest::from_path(&manifest_path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "warning: --frozen given but no readable package.json at {} ({e}); skipping drift check",
                project_root.display()
            );
            return Ok(());
        }
    };

    let mut declared: BTreeSet<String> = BTreeSet::new();
    declared.extend(manifest.dependencies.keys().cloned());
    declared.extend(manifest.dev_dependencies.keys().cloned());
    let locked: BTreeSet<String> = lockfile.root.dependencies.keys().cloned().collect();

    if declared == locked {
        return Ok(());
    }

    let only_manifest: Vec<&String> = declared.difference(&locked).collect();
    let only_lock: Vec<&String> = locked.difference(&declared).collect();
    anyhow::bail!(
        "frozen install refused: package.json and bpm.lock disagree on root dependencies\n  \
         in package.json but not bpm.lock: {}\n  \
         in bpm.lock but not package.json: {}\n  \
         re-run `bpm import` after editing package.json",
        if only_manifest.is_empty() {
            "(none)".to_string()
        } else {
            only_manifest
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        },
        if only_lock.is_empty() {
            "(none)".to_string()
        } else {
            only_lock
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        },
    );
}

/// One unit of install work: a non-link package with a resolved URL.
struct InstallWork {
    idx: usize,
    name: String,
    url: String,
    integrity: Option<Integrity>,
}

/// Result of fetching+extracting one package.
struct FetchOutcome {
    idx: usize,
    id: ArtifactId,
    art_cached: bool,
    img_cached: bool,
}

/// A fetch/extract failure, carrying enough context for an actionable error
/// (package name + url + the underlying store error, which itself includes
/// expected/computed integrity on mismatch). The store error is boxed to keep
/// the worker-thread `Result`'s `Err` variant small (clippy::result_large_err).
#[derive(Debug)]
struct FetchFail {
    name: String,
    url: String,
    source: Box<StoreError>,
}

/// Build the fetchable work list from the lockfile, in package order.
///
/// Entries with `link == true` or an empty `resolved` are skipped (workspaces
/// and link/file entries are not store-backed). Integrity is required under
/// `--frozen`; without it the package is still fetched anonymously but cannot
/// be verified (rare, non-frozen path).
fn build_install_work(lockfile: &Lockfile, frozen: bool) -> anyhow::Result<Vec<InstallWork>> {
    let mut work = Vec::new();
    for (idx, pkg) in lockfile.packages.iter().enumerate() {
        if pkg.link || pkg.resolved.is_empty() {
            continue;
        }
        let integrity = match pkg.integrity.as_deref() {
            Some(s) => Some(Integrity::parse(s).map_err(|e| {
                anyhow::anyhow!(
                    "package '{}' at {} has invalid integrity \"{s}\": {e}",
                    pkg.name,
                    pkg.path
                )
            })?),
            None => {
                if frozen {
                    anyhow::bail!(
                        "package '{}' at {} has no integrity; cannot verify a frozen install \
                         (re-run `bpm import`)",
                        pkg.name,
                        pkg.path
                    );
                }
                None
            }
        };
        work.push(InstallWork {
            idx,
            name: pkg.name.clone(),
            url: pkg.resolved.clone(),
            integrity,
        });
    }
    Ok(work)
}

impl std::fmt::Display for FetchFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
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
