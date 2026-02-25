//! `bpm` command-line entry point.
//!
//! Commands:
//!
//! * `bpm --version` — prints the built-in package version (handled by clap).
//! * `bpm doctor` — inspects the nearest `package.json` and reports
//!   structured diagnostics.
//! * `bpm fetch <spec|url>` — downloads a package by npm-style spec (`lodash`,
//!   `lodash@4.17.21`, `@scope/pkg@^1`) resolved against the registry, or by an
//!   exact tarball URL / `file://` path; verifies its SHA-512 integrity, stores
//!   it immutably, and (by default) extracts it into a package image. Repeated
//!   `fetch` performs no network or extraction work (Milestone 1 artifact-store
//!   prototype).
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
use bpm::graph;
use bpm::integrity::{ArtifactId, Integrity};
use bpm::lockfile::{find_lockfile, Lockfile, BPM_LOCK_FILE};
use bpm::manifest::PackageManifest;
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
    /// Download, verify, store, and extract a package by spec or exact URL.
    Fetch {
        /// Package spec (`lodash`, `lodash@4.17.21`, `@scope/pkg@^1`) or an
        /// exact tarball URL / `file://` path. Specs are resolved against the
        /// registry; URLs/paths are fetched directly.
        target: String,
        /// Expected integrity string (`sha512-<base64>`). For a spec this
        /// overrides the registry's `dist.integrity`; for a URL it enables
        /// verification and cache-hit reuse without re-downloading.
        #[arg(long)]
        integrity: Option<String>,
        /// Registry base URL for spec resolution (defaults to
        /// `$BPM_REGISTRY` or `https://registry.npmjs.org`). Ignored for URLs.
        #[arg(long)]
        registry: Option<String>,
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
    /// Install packages. With no argument, install the locked dependency graph
    /// from `bpm.lock` into `node_modules`. With a package spec or URL argument
    /// (e.g. `bpm install cowsay`, `bpm install lodash@4.17.21`), fetch that
    /// package and link its declared executables into a global bin directory
    /// (`$BPM_BIN`, else `~/.local/bin`, else `~/bin`) so they appear on PATH.
    Install {
        /// Package spec (`lodash`, `lodash@4.17.21`, `@scope/pkg@^1`) or an
        /// exact tarball URL / `file://` path to fetch and link bins for. When
        /// omitted, install from `bpm.lock` instead.
        target: Option<String>,
        /// Require `package.json` and `bpm.lock` to agree; never change versions.
        /// Only applies to the lockfile install mode (no `target`).
        #[arg(long)]
        frozen: bool,
        /// Registry base URL for spec resolution in `bpm install <spec>` mode
        /// (defaults to `$BPM_REGISTRY` or `https://registry.npmjs.org`).
        #[arg(long)]
        registry: Option<String>,
        /// Store root (defaults to `$BPM_STORE` or `$HOME/.bpm`).
        #[arg(long)]
        store: Option<PathBuf>,
        /// Max concurrent fetch + extract workers.
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
        /// Write phase metrics as canonical JSON to `PATH`.
        #[arg(long = "json-metrics")]
        json_metrics: Option<PathBuf>,
        /// Do not run lifecycle scripts (scripts run by default now; this
        /// skips them for a trust-free install).
        #[arg(long)]
        ignore_scripts: bool,
    },
    /// Run a `package.json` lifecycle script with an npm-compatible environment.
    Run {
        /// Script name to run (e.g. `build`, `test`, `preinstall`).
        script: String,
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
            target,
            integrity,
            registry,
            store,
            no_extract,
            json_metrics,
        } => match run_fetch(
            &target,
            integrity,
            registry,
            store,
            no_extract,
            json_metrics,
        ) {
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
            target,
            frozen,
            registry,
            store,
            concurrency,
            json_metrics,
            ignore_scripts,
        } => match run_install(
            target,
            frozen,
            registry,
            store,
            concurrency,
            json_metrics,
            ignore_scripts,
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("error: {err:#}");
                ExitCode::FAILURE
            }
        },
        Commands::Run { script } => match run_script_cmd(&script) {
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
    target: &str,
    integrity: Option<String>,
    registry: Option<String>,
    store: Option<PathBuf>,
    no_extract: bool,
    json_metrics: Option<PathBuf>,
) -> anyhow::Result<()> {
    let store_root = store
        .or_else(|| env::var_os("BPM_STORE").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".bpm")))
        .ok_or_else(|| anyhow::anyhow!("no --store given and $BPM_STORE/$HOME is unset"))?;

    let store = ArtifactStore::open(&store_root)?;

    let mut metrics = Metrics::new();

    // Decide whether `target` is an npm-style package spec (resolve against the
    // registry) or a direct source (exact URL / file:// / bare path -> fetched
    // as-is). The check is strict: only valid npm names take the spec path, so
    // URLs, file:// paths, and bare local paths keep their existing behavior.
    let (url, integrity): (String, Option<Integrity>) =
        if bpm::registry::is_valid_npm_name(name_of_spec(target)) {
            let registry_base = registry
                .or_else(|| env::var_os("BPM_REGISTRY").map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "https://registry.npmjs.org".to_string());
            let spec = bpm::registry::parse_spec(target)?;
            let resolved = metrics
                .measure("metadata_fetch", || {
                    bpm::registry::resolve(&spec, &registry_base)
                })
                .map_err(|e| {
                    anyhow::anyhow!("failed to resolve '{target}' from {registry_base}: {e}")
                })?;
            // A CLI `--integrity` (if given) wins; otherwise trust the registry.
            let integ = match integrity.as_deref() {
                Some(s) => Integrity::parse(s)?,
                None => Integrity::parse(&resolved.integrity)?,
            };
            eprintln!(
                "resolved {}@{} -> {}",
                resolved.name, resolved.version, resolved.tarball_url
            );
            (resolved.tarball_url, Some(integ))
        } else {
            let integ = integrity.as_deref().map(Integrity::parse).transpose()?;
            (target.to_string(), integ)
        };

    let artifact = store.ensure_artifact(&url, integrity.as_ref(), &mut metrics)?;
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

/// The name portion of a possibly-versioned spec, for the spec-vs-source check.
/// `lodash` / `@scope/pkg` -> themselves; `lodash@1.2.3` -> `lodash`.
fn name_of_spec(target: &str) -> &str {
    match target.rfind('@') {
        Some(0) | None => target,
        Some(i) => &target[..i],
    }
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
    target: Option<String>,
    frozen: bool,
    registry: Option<String>,
    store: Option<PathBuf>,
    concurrency: usize,
    json_metrics: Option<PathBuf>,
    ignore_scripts: bool,
) -> anyhow::Result<()> {
    // --ignore-scripts is accepted but lifecycle scripts arrive in a later
    // milestone (M5); for now it is an acknowledged no-op.
    let _ = ignore_scripts;

    // `bpm install <spec|url>` — fetch a single package and link its declared
    // executables into a global bin directory. This is a distinct mode from the
    // lockfile graph install below.
    if let Some(target) = target {
        return run_install_bin(&target, registry, store);
    }

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

    let mut metrics = Metrics::new();

    // Graph-plan cache (Milestone 3): if a valid plan for this graph already
    // exists and the project's node_modules still matches it, this is an
    // unchanged repeat install — skip fetch/extract/materialize entirely.
    let plan_path = graph::plan_path_for(&lockfile_path);
    let cached_plan = graph::read_plan(&plan_path)?;
    let plan_valid = match cached_plan.as_ref() {
        Some(plan) => graph::validate_plan(plan, &lockfile, &project_root, &store).is_ok(),
        None => false,
    };

    if plan_valid {
        // Cache hit: the graph + materialized state are unchanged, so there is
        // no resolution or plan construction work to do. Record the hit so
        // metrics/trace reflect the skip.
        metrics.record("plan_cache_hit", std::time::Duration::ZERO);
        let plan = cached_plan.as_ref().unwrap();
        let materialized = plan
            .entries
            .iter()
            .filter(|e| !e.link && !e.resolved.is_empty() && !e.artifact_hex.is_empty())
            .count();
        let bins = plan.entries.iter().map(|e| e.bin.len()).sum::<usize>();
        println!(
            "nothing to install — graph {} unchanged ({} package(s), {} bin(s) already materialized)",
            graph::graph_id_for_project(&lockfile, &project_root).to_hex_short(),
            materialized,
            bins,
        );
        if trace_enabled() {
            metrics
                .print_trace(&mut io::stderr())
                .map_err(|e| anyhow::anyhow!("failed to write trace: {e}"))?;
        }
        if let Some(path) = json_metrics {
            fs::write(&path, metrics.to_json()).map_err(|e| {
                anyhow::anyhow!("failed to write metrics to {}: {e}", path.display())
            })?;
        }
        return Ok(());
    }

    // Cache miss: record it and proceed to build the work list + install.
    metrics.record("plan_cache_miss", std::time::Duration::ZERO);

    // Build the fetchable work list in lockfile order (deterministic). Each
    // item carries its package index so outcomes can be re-sorted into lockfile
    // order before materialization.
    let work = build_install_work(&lockfile, frozen)?;

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

    // Pair each package entry with its ArtifactId for the graph volume.
    let mut artifact_ids: Vec<Option<ArtifactId>> =
        (0..lockfile.packages.len()).map(|_| None).collect();
    for o in &outcomes {
        if o.idx < artifact_ids.len() {
            artifact_ids[o.idx] = Some(o.id);
        }
    }

    // Build (or reuse) the reusable graph volume for this graph id, then attach
    // the project to it via shallow top-level relays. A second project that
    // shares the graph id hits `ensure_graph_volume` as a cache (no rebuild).
    let volume = bpm::volume::ensure_graph_volume(&store, &lockfile, &artifact_ids, &mut metrics)?;
    let attach = bpm::volume::attach_project(&project_root, &volume)?;

    // Lifecycle scripts (Milestone 5): run permitted scripts for installed
    // packages in isolated sandboxes. `--ignore-scripts` skips the whole phase.
    if !ignore_scripts {
        let policy = bpm::lifecycle::LifecyclePolicy {
            ignore_scripts: false,
        };
        match bpm::lifecycle::run_lifecycle(
            &project_root,
            &store,
            &lockfile,
            &artifact_ids,
            policy,
            &mut metrics,
        ) {
            Ok(lc) => {
                if lc.packages_with_scripts > 0 {
                    eprintln!(
                        "lifecycle: {} package(s) with scripts ({} phase(s) executed, {} succeeded, {} failed)",
                        lc.packages_with_scripts,
                        lc.phases_executed,
                        lc.phases_succeeded,
                        lc.phases_failed,
                    );
                    for o in &lc.outcomes {
                        let mark = if o.exit_code == Some(0) { "ok" } else { "FAIL" };
                        eprintln!("  [{mark}] {}.{}) {}", o.package, o.phase, o.command);
                    }
                }
            }
            Err(e) => eprintln!("warning: lifecycle phase failed: {e}"),
        }
    } else {
        metrics.record("lifecycle", std::time::Duration::ZERO);
    }

    // Compile and persist the install plan so the next run can skip resolution
    // and materialization when the graph + project state are unchanged. The graph
    // id folds in the workspace layout (if any), so a workspace-tree change
    // invalidates the cached plan/volume.
    let mut plan = graph::build_plan(&lockfile, &artifact_ids);
    plan.graph_id_hex = graph::graph_id_for_project(&lockfile, &project_root).to_hex();
    if let Err(e) = graph::write_plan(&plan, &plan_path) {
        eprintln!("warning: failed to write plan {}: {e}", plan_path.display());
    }

    // The volume's materialize stats only count work done on a build; on a cache
    // reuse (volume.cached) they are zero. Report the project's package count from
    // the authoritative lockfile so the message is stable across build vs reuse.
    let package_count = lockfile
        .packages
        .iter()
        .filter(|p| !p.link && !p.resolved.is_empty())
        .count();

    println!(
        "installed {} package(s) into {} ({} cached, {} fetched; graph volume {}, {} relay(s))",
        package_count,
        project_root.join("node_modules").display(),
        cached,
        fetched,
        if volume.cached { "reused" } else { "built" },
        attach.relays_created + attach.relays_unchanged,
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

/// `bpm install <spec|url>` — fetch a single package, extract it, and link its
/// declared executables into a global bin directory so they appear on PATH.
///
/// Resolution mirrors [`run_fetch`]: a valid npm name is resolved against the
/// registry (honoring `$BPM_REGISTRY`), while a URL/`file://`/bare path is used
/// as-is. The package's extracted image `package.json` `bin` field (a string or
/// name->path map) drives which executables are linked. Each bin becomes a
/// symlink in the bin dir pointing at the absolute image path, and the target
/// file is made executable.
fn run_install_bin(
    target: &str,
    registry: Option<String>,
    store: Option<PathBuf>,
) -> anyhow::Result<()> {
    let store_root = store
        .or_else(|| env::var_os("BPM_STORE").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".bpm")))
        .ok_or_else(|| anyhow::anyhow!("no --store given and $BPM_STORE/$HOME is unset"))?;
    let store = ArtifactStore::open(&store_root)?;

    let mut metrics = Metrics::new();

    // Resolve `target` to a concrete (url, integrity) pair, exactly like fetch.
    let (url, integrity): (String, Option<Integrity>) =
        if bpm::registry::is_valid_npm_name(name_of_spec(target)) {
            let registry_base = registry
                .or_else(|| env::var_os("BPM_REGISTRY").map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "https://registry.npmjs.org".to_string());
            let spec = bpm::registry::parse_spec(target)?;
            let resolved = metrics
                .measure("metadata_fetch", || {
                    bpm::registry::resolve(&spec, &registry_base)
                })
                .map_err(|e| {
                    anyhow::anyhow!("failed to resolve '{target}' from {registry_base}: {e}")
                })?;
            eprintln!(
                "resolved {}@{} -> {}",
                resolved.name, resolved.version, resolved.tarball_url
            );
            let integ = Integrity::parse(&resolved.integrity)?;
            (resolved.tarball_url, Some(integ))
        } else {
            (target.to_string(), None)
        };

    let artifact = store.ensure_artifact(&url, integrity.as_ref(), &mut metrics)?;
    let image = store.ensure_image(&artifact.id, &mut metrics)?;
    println!(
        "fetched {} ({}) -> {}",
        artifact.id,
        if artifact.cached { "cached" } else { "stored" },
        image.path.display()
    );

    // Read the package's declared executables from its package.json.
    let manifest_path = image.path.join("package.json");
    let manifest = PackageManifest::from_path(&manifest_path)
        .map_err(|e| anyhow::anyhow!("could not read {}: {e}", manifest_path.display()))?;
    let bins: Vec<(String, String)> = match &manifest.bin {
        Some(bpm::manifest::BinField::Map(m)) => {
            m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        }
        Some(bpm::manifest::BinField::One(p)) => {
            let name = manifest.name.clone().unwrap_or_else(|| target.to_string());
            vec![(name, p.clone())]
        }
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
        .map_err(|e| anyhow::anyhow!("could not create {}: {e}", bin_dir.display()))?;

    let mut linked = Vec::new();
    for (name, relpath) in &bins {
        let relpath = relpath.strip_prefix("./").unwrap_or(relpath);
        let target_file = image.path.join(relpath);
        if !target_file.exists() {
            eprintln!(
                "warning: bin '{}' points at missing file {}; skipping",
                name,
                target_file.display()
            );
            continue;
        }
        // Make the executable bit stick (npm convention), idempotent.
        set_executable(&target_file);
        let link = bin_dir.join(name);
        link_bin(&link, &target_file)?;
        linked.push(name.clone());
    }

    if linked.is_empty() {
        anyhow::bail!("no bins were linked for {}", target);
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

/// The directory bins are linked into, in priority order:
/// `$BPM_BIN` -> `~/.local/bin` -> `~/bin`.
fn bin_dir() -> anyhow::Result<PathBuf> {
    if let Some(p) = env::var_os("BPM_BIN") {
        return Ok(PathBuf::from(p));
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

/// `true` if any PATH entry equals `bin_dir`.
fn bin_dir_on_path() -> bool {
    let bin_dir = match bin_dir() {
        Ok(d) => d,
        Err(_) => return false,
    };
    env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|e| e == bin_dir))
        .unwrap_or(false)
}

/// Point `link` at `target`, replacing any existing entry (idempotent on a
/// correct target). Uses a symlink so the immutable store image is the source
/// of truth and `bpm install` is repeatable.
fn link_bin(link: &Path, target: &Path) -> anyhow::Result<()> {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("could not create {}: {e}", parent.display()))?;
    }
    if let Ok(existing) = fs::read_link(link) {
        if same_file_path(&existing, target) {
            return Ok(());
        }
    }
    let _ = fs::remove_file(link);
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).map_err(|e| {
        anyhow::anyhow!(
            "could not symlink {} -> {}: {e}",
            link.display(),
            target.display()
        )
    })?;
    #[cfg(not(unix))]
    anyhow::bail!("bin linking is only supported on Unix-like systems");
    Ok(())
}

/// Component-wise path equality (ignores trailing separators / OS flavor).
fn same_file_path(a: &Path, b: &Path) -> bool {
    a.components().eq(b.components())
}

/// Add owner/group/other execute bits to `path` (best-effort, Unix-only).
#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path) {
        let mut perms = meta.permissions();
        let mode = perms.mode();
        perms.set_mode(mode | 0o111);
        let _ = fs::set_permissions(path, perms);
    }
}
#[cfg(not(unix))]
fn set_executable(_path: &Path) {}

/// Run a `package.json` script by name with an npm-compatible environment.
fn run_script_cmd(script: &str) -> anyhow::Result<()> {
    let cwd = env::current_dir()?;
    let manifest = PackageManifest::from_path(&cwd.join("package.json"))
        .map_err(|e| anyhow::anyhow!("no readable package.json in {}: {e}", cwd.display()))?;
    let command = manifest
        .scripts
        .get(script)
        .ok_or_else(|| anyhow::anyhow!("script '{script}' is not defined in package.json"))?;

    let bin = cwd.join("node_modules").join(".bin");
    let mut child = std::process::Command::new("sh");
    child.arg("-c").arg(command).current_dir(&cwd);
    // npm-compatible environment (IMPLEMENTATION §14).
    child.env("npm_lifecycle_event", script);
    child.env("npm_lifecycle_script", command);
    child.env(
        "npm_package_name",
        manifest.name.clone().unwrap_or_default(),
    );
    child.env(
        "npm_package_version",
        manifest.version.clone().unwrap_or_default(),
    );
    child.env("npm_config_user_agent", "bpm/0.1.0");
    child.env("npm_execpath", "bpm");
    child.env("INIT_CWD", &cwd);
    child.env("NODE", which("node").unwrap_or_else(|| "node".into()));
    if let Some(path) = env::var_os("PATH") {
        let mut new_path = std::ffi::OsString::from(&bin);
        new_path.push(std::path::MAIN_SEPARATOR.to_string());
        new_path.push(&path);
        child.env("PATH", new_path);
    }
    let status = child
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run script: {e}"))?;
    if !status.success() {
        anyhow::bail!("script '{script}' exited with status {:?}", status.code());
    }
    Ok(())
}

fn which(tool: &str) -> Option<String> {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool}"))
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
}
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
