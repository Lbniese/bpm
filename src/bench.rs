use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Scenario
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScenarioKind {
    TrueCold,
    ResolvedCold,
    WarmStore,
    RepeatInstall,
    SecondProjectSameGraph,
    PartialDependencyChange,
    MonorepoCold,
    MonorepoIncremental,
}

impl ScenarioKind {
    pub fn name(&self) -> &'static str {
        match self {
            Self::TrueCold => "true_cold",
            Self::ResolvedCold => "resolved_cold",
            Self::WarmStore => "warm_store",
            Self::RepeatInstall => "repeat_install",
            Self::SecondProjectSameGraph => "second_project_same_graph",
            Self::PartialDependencyChange => "partial_dependency_change",
            Self::MonorepoCold => "monorepo_cold",
            Self::MonorepoIncremental => "monorepo_incremental",
        }
    }

    pub fn describe(&self) -> &'static str {
        match self {
            Self::TrueCold => "no store, no lockfile, no project view — completely fresh",
            Self::ResolvedCold => "lockfile present, empty store, no project view",
            Self::WarmStore => "populated store, lockfile present, no project view",
            Self::RepeatInstall => "populated store, lockfile present, existing project view",
            Self::SecondProjectSameGraph => "second project reusing a populated graph store",
            Self::PartialDependencyChange => "warm project with one dependency changed",
            Self::MonorepoCold => "cold workspace-style project with repeated dependencies",
            Self::MonorepoIncremental => "incremental workspace-style project change",
        }
    }

    pub fn all() -> Vec<ScenarioKind> {
        vec![
            Self::TrueCold,
            Self::ResolvedCold,
            Self::WarmStore,
            Self::RepeatInstall,
            Self::SecondProjectSameGraph,
            Self::PartialDependencyChange,
            Self::MonorepoCold,
            Self::MonorepoIncremental,
        ]
    }
}

fn scenario_uses_lockfile(scenario: ScenarioKind) -> bool {
    !matches!(scenario, ScenarioKind::TrueCold)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Fixture {
    pub name: &'static str,
    pub packages: &'static [&'static str],
}

pub const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "minimal",
        packages: &["left-pad@1.3.0"],
    },
    Fixture {
        name: "small",
        packages: &["left-pad@1.3.0", "is-number@7.0.0"],
    },
    // A medium fixture with a real transitive graph, so warm/cold scenarios
    // exercise extraction + materialization of more than one package.
    Fixture {
        name: "medium",
        packages: &["is-odd@3.0.1", "is-number@7.0.0", "left-pad@1.3.0"],
    },
    Fixture {
        name: "large-frontend",
        packages: &[
            "react@18.3.1",
            "react-dom@18.3.1",
            "webpack@5.99.9",
            "typescript@5.8.3",
        ],
    },
    Fixture {
        name: "many-small-files",
        packages: &["lodash@4.17.21", "glob@10.4.5", "minimatch@9.0.5"],
    },
    Fixture {
        name: "monorepo",
        packages: &["is-odd@3.0.1", "is-number@7.0.0", "left-pad@1.3.0"],
    },
    Fixture {
        name: "lifecycle",
        packages: &["npm-run-path@5.3.0", "cross-spawn@7.0.6"],
    },
    Fixture {
        name: "native-addon",
        packages: &["node-gyp@11.2.0", "bindings@1.5.0"],
    },
];

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stats {
    pub values: Vec<f64>,
    pub median: f64,
    pub p95: f64,
    pub stddev: f64,
}

impl Stats {
    pub fn compute(values: Vec<f64>) -> Self {
        let mut sorted = values.clone();
        sorted.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());

        let len = sorted.len();
        if len == 0 {
            return Stats {
                values,
                median: 0.0,
                p95: 0.0,
                stddev: 0.0,
            };
        }

        let median = if len.is_multiple_of(2) {
            (sorted[len / 2 - 1] + sorted[len / 2]) / 2.0
        } else {
            sorted[len / 2]
        };

        let idx = ((len as f64) * 0.95).ceil() as usize - 1;
        let p95 = sorted[idx.min(len - 1)];

        let mean = sorted.iter().sum::<f64>() / len as f64;
        let variance = sorted.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / len as f64;
        let stddev = variance.sqrt();

        Stats {
            values,
            median,
            p95,
            stddev,
        }
    }
}

// ---------------------------------------------------------------------------
// System info
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SystemInfo {
    pub machine: String,
    pub operating_system: String,
    pub kernel: String,
    pub runtime_versions: BTreeMap<String, String>,
}

impl SystemInfo {
    pub fn capture() -> Self {
        let machine = cmd_stdout_or_default("uname", &["-m"]);

        let operating_system = cmd_stdout_or_default("sw_vers", &["-productVersion"]);

        let kernel = cmd_stdout_or_default("uname", &["-r"]);

        let mut runtime_versions = BTreeMap::new();
        if let Some(v) = capture_version("node", &["--version"]) {
            runtime_versions.insert("node".into(), v);
        }
        if let Some(v) = capture_version("npm", &["--version"]) {
            runtime_versions.insert("npm".into(), v);
        }
        if let Some(v) = capture_version("pnpm", &["--version"]) {
            runtime_versions.insert("pnpm".into(), v);
        }
        if let Some(v) = capture_bpm_version() {
            runtime_versions.insert("bpm".into(), v);
        }

        SystemInfo {
            machine,
            operating_system,
            kernel,
            runtime_versions,
        }
    }
}

fn cmd_stdout_or_default(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn capture_version(cmd: &str, args: &[&str]) -> Option<String> {
    Command::new(cmd).args(args).output().ok().and_then(|o| {
        if o.status.success() {
            Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
        } else {
            None
        }
    })
}

fn bpm_binary() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("bpm"))
}

fn capture_bpm_version() -> Option<String> {
    Command::new(bpm_binary())
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                None
            }
        })
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tool {
    Npm,
    Pnpm,
    Bpm,
    Yarn,
    Bun,
}

impl Tool {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Bpm => "bpm",
            Self::Yarn => "yarn",
            Self::Bun => "bun",
        }
    }

    pub fn all() -> Vec<Tool> {
        vec![Self::Npm, Self::Pnpm, Self::Bpm, Self::Yarn, Self::Bun]
    }

    pub fn detect(self) -> bool {
        match self {
            Self::Bpm => capture_bpm_version().is_some(),
            _ => Command::new(self.name())
                .arg("--version")
                .output()
                .ok()
                .is_some_and(|o| o.status.success()),
        }
    }
}

fn capture_tool_version(tool: Tool) -> Option<String> {
    match tool {
        Tool::Bpm => capture_bpm_version(),
        _ => capture_version(tool.name(), &["--version"]),
    }
}

// ---------------------------------------------------------------------------
// Per-tool results
// ---------------------------------------------------------------------------

/// Aggregated bpm phase/profile metrics captured during timed benchmark runs
/// (bpm only — other tools do not emit `--json-metrics`). Each `Stats` is
/// computed across the per-run samples, so `requests_sent.median` is the
/// median outbound-request count per run and `phase_ms["dependency_resolution"]`
/// is the median summed duration of that phase per run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BpmMetricsSummary {
    pub requests_sent: Stats,
    pub phase_ms: BTreeMap<String, Stats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResults {
    pub tool: String,
    pub wall_clock_ms: Stats,
    pub exit_codes: Vec<i32>,
    /// bpm-only phase timings and outbound request counts, captured via
    /// `--json-metrics` during the timed run. Absent for other tools and for
    /// bpm runs whose metrics file could not be read (e.g. the offline test
    /// runner), so existing baselines without this field still deserialize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bpm_metrics: Option<BpmMetricsSummary>,
}

// ---------------------------------------------------------------------------
// Top-level result
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkResult {
    pub scenario: String,
    pub fixture: String,
    pub system: SystemInfo,
    /// Exact version string of each tool that actually ran this result
    /// (`node`/`npm`/`pnpm`/`bpm` -> version). Recording the toolchain versions
    /// makes a result reproducible: two runs are only comparable when their
    /// versions maps match.
    pub versions: BTreeMap<String, String>,
    pub cache_state: String,
    pub number_of_runs: usize,
    pub tools: Vec<ToolResults>,
}

// ---------------------------------------------------------------------------
// Command specs and execution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub label: &'static str,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub current_dir: PathBuf,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutcome {
    pub exit_code: i32,
}

pub trait CommandRunner {
    fn run(&mut self, command: &CommandSpec) -> anyhow::Result<CommandOutcome>;
}

struct ProcessRunner;

impl CommandRunner for ProcessRunner {
    fn run(&mut self, command: &CommandSpec) -> anyhow::Result<CommandOutcome> {
        let mut process = Command::new(&command.program);
        process
            .args(&command.args)
            .current_dir(&command.current_dir);
        for (key, value) in &command.env {
            process.env(key, value);
        }
        let status = process
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| {
                anyhow::anyhow!(
                    "failed to run {} {}: {e}",
                    command.program.display(),
                    command.args.join(" ")
                )
            })?;
        Ok(CommandOutcome {
            exit_code: status.code().unwrap_or(-1),
        })
    }
}

fn tool_cache_root(tool: Tool, bpm_store: &Path) -> PathBuf {
    bpm_store.join(format!("{}-cache", tool.name()))
}

fn pnpm_store_dir(bpm_store: &Path) -> PathBuf {
    tool_cache_root(Tool::Pnpm, bpm_store).join("store")
}

fn configure_tool_cache_env(tool: Tool, bpm_store: &Path) -> BTreeMap<String, String> {
    let cache = tool_cache_root(tool, bpm_store);
    let mut env = BTreeMap::new();
    match tool {
        Tool::Npm => {
            env.insert(
                "npm_config_cache".into(),
                cache.to_string_lossy().into_owned(),
            );
        }
        Tool::Pnpm => {
            env.insert(
                "npm_config_cache".into(),
                cache.join("npm-cache").to_string_lossy().into_owned(),
            );
        }
        Tool::Yarn => {
            env.insert(
                "YARN_CACHE_FOLDER".into(),
                cache.to_string_lossy().into_owned(),
            );
        }
        Tool::Bun => {
            env.insert(
                "BUN_INSTALL_CACHE_DIR".into(),
                cache.to_string_lossy().into_owned(),
            );
        }
        Tool::Bpm => {}
    }
    env
}

fn build_command_spec(
    label: &'static str,
    program: impl Into<PathBuf>,
    args: impl IntoIterator<Item = impl Into<String>>,
    current_dir: &Path,
    env: BTreeMap<String, String>,
) -> CommandSpec {
    CommandSpec {
        label,
        program: program.into(),
        args: args.into_iter().map(Into::into).collect(),
        current_dir: current_dir.to_path_buf(),
        env,
    }
}

pub fn lock_setup_command_specs(tool: Tool, work_dir: &Path, bpm_store: &Path) -> Vec<CommandSpec> {
    match tool {
        Tool::Npm => vec![build_command_spec(
            "setup_lockfile",
            "npm",
            ["install", "--package-lock-only"],
            work_dir,
            configure_tool_cache_env(Tool::Npm, bpm_store),
        )],
        Tool::Pnpm => vec![build_command_spec(
            "setup_lockfile",
            "pnpm",
            [
                "install".to_string(),
                "--lockfile-only".to_string(),
                "--store-dir".to_string(),
                pnpm_store_dir(bpm_store).to_string_lossy().into_owned(),
            ],
            work_dir,
            configure_tool_cache_env(Tool::Pnpm, bpm_store),
        )],
        Tool::Bpm => vec![
            build_command_spec(
                "setup_lockfile",
                "npm",
                ["install", "--package-lock-only"],
                work_dir,
                configure_tool_cache_env(Tool::Npm, bpm_store),
            ),
            build_command_spec(
                "setup_bpm_lock",
                bpm_binary(),
                [
                    "import".to_string(),
                    work_dir
                        .join("package-lock.json")
                        .to_string_lossy()
                        .into_owned(),
                    "--out".to_string(),
                    work_dir.join("bpm.lock").to_string_lossy().into_owned(),
                ],
                work_dir,
                BTreeMap::new(),
            ),
        ],
        // Exploratory fallback until these managers gain their own native-lock
        // setup path in the harness. Competitive scorecards use npm/pnpm/bpm.
        Tool::Yarn | Tool::Bun => vec![build_command_spec(
            "setup_lockfile",
            "npm",
            ["install", "--package-lock-only"],
            work_dir,
            configure_tool_cache_env(Tool::Npm, bpm_store),
        )],
    }
}

pub fn install_command_spec(
    tool: Tool,
    work_dir: &Path,
    bpm_store: &Path,
    scenario: ScenarioKind,
    json_metrics: Option<&Path>,
) -> CommandSpec {
    match tool {
        Tool::Npm => build_command_spec(
            "install",
            "npm",
            ["install", "--prefer-offline"],
            work_dir,
            configure_tool_cache_env(Tool::Npm, bpm_store),
        ),
        Tool::Pnpm => build_command_spec(
            "install",
            "pnpm",
            [
                "install".to_string(),
                "--prefer-offline".to_string(),
                "--store-dir".to_string(),
                pnpm_store_dir(bpm_store).to_string_lossy().into_owned(),
            ],
            work_dir,
            configure_tool_cache_env(Tool::Pnpm, bpm_store),
        ),
        Tool::Bpm => {
            let mut args = vec!["install".to_string()];
            if scenario_uses_lockfile(scenario) {
                args.push("--frozen".to_string());
            }
            args.push("--store".to_string());
            args.push(bpm_store.to_string_lossy().into_owned());
            if let Some(path) = json_metrics {
                args.push("--json-metrics".to_string());
                args.push(path.to_string_lossy().into_owned());
            }
            build_command_spec("install", bpm_binary(), args, work_dir, BTreeMap::new())
        }
        Tool::Yarn => build_command_spec(
            "install",
            "yarn",
            ["install"],
            work_dir,
            configure_tool_cache_env(Tool::Yarn, bpm_store),
        ),
        Tool::Bun => build_command_spec(
            "install",
            "bun",
            ["install", "--no-progress"],
            work_dir,
            configure_tool_cache_env(Tool::Bun, bpm_store),
        ),
    }
}

// ---------------------------------------------------------------------------
// Fixture workspace preparation
// ---------------------------------------------------------------------------

pub fn fixture_dir(name: &str) -> PathBuf {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    repo_root.join("fixtures").join(name)
}

fn create_fixture_workspace(fixture: &Fixture, work_dir: &Path) -> anyhow::Result<()> {
    let fixt_dir = fixture_dir(fixture.name);
    if fixt_dir.exists() {
        copy_dir(&fixt_dir, work_dir)?;
    } else {
        generate_fixture_files(fixture, work_dir)?;
    }
    Ok(())
}

fn generate_fixture_files(fixture: &Fixture, dir: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(dir.join("node_modules"))?;

    let deps: BTreeMap<&str, &str> = fixture
        .packages
        .iter()
        .map(|p| {
            let parts: Vec<&str> = p.splitn(2, '@').collect();
            (parts[0], parts[1])
        })
        .collect();

    let pkg_json = serde_json::json!({
        "name": format!("bench-{}", fixture.name),
        "version": "1.0.0",
        "dependencies": deps,
    });
    fs::write(
        dir.join("package.json"),
        serde_json::to_string_pretty(&pkg_json)?,
    )?;

    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if kind.is_dir() {
            copy_dir(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Benchmark runner
// ---------------------------------------------------------------------------

pub fn run_scenario(
    scenario: ScenarioKind,
    fixture: &Fixture,
    tool: Tool,
    num_runs: usize,
) -> anyhow::Result<ToolResults> {
    let mut runner = ProcessRunner;
    run_scenario_with_runner(scenario, fixture, tool, num_runs, &mut runner)
}

/// Shape of a bpm `--json-metrics` file (only the fields the harness needs).
#[derive(Debug, Deserialize)]
struct BpmMetricsFile {
    #[serde(default)]
    phases: BTreeMap<String, f64>,
    #[serde(default)]
    counters: BpmMetricsCounters,
}

#[derive(Debug, Default, Deserialize)]
struct BpmMetricsCounters {
    #[serde(default)]
    requests_sent: u64,
}

/// Best-effort read+parse of a bpm metrics file. Returns `None` on any I/O or
/// parse failure so a missing/unreadable file (e.g. the offline test runner, or
/// a run that exited before writing metrics) never fails the benchmark.
fn read_bpm_metrics(path: &Path) -> Option<BpmMetricsFile> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Aggregate per-run request counts and per-phase summed durations into the bpm
/// metrics summary. Pure (no I/O) so it is directly unit-testable.
pub fn aggregate_bpm_metrics(
    request_counts: Vec<f64>,
    phase_samples: BTreeMap<String, Vec<f64>>,
) -> BpmMetricsSummary {
    let phase_ms = phase_samples
        .into_iter()
        .map(|(name, samples)| (name, Stats::compute(samples)))
        .collect();
    BpmMetricsSummary {
        requests_sent: Stats::compute(request_counts),
        phase_ms,
    }
}

pub fn run_scenario_with_runner(
    scenario: ScenarioKind,
    fixture: &Fixture,
    tool: Tool,
    num_runs: usize,
    runner: &mut dyn CommandRunner,
) -> anyhow::Result<ToolResults> {
    if num_runs == 0 {
        anyhow::bail!("benchmark runs must be at least 1");
    }

    let temp_base = tempfile::tempdir()?;
    let shared_store = temp_base.path().join("bpm-store");
    fs::create_dir_all(&shared_store)?;

    let mut wall_times = Vec::with_capacity(num_runs);
    let mut exit_codes = Vec::with_capacity(num_runs);
    // bpm-only: per-run outbound request counts and per-phase summed durations,
    // captured from each timed run's `--json-metrics` file.
    let mut request_counts: Vec<f64> = Vec::with_capacity(num_runs);
    let mut phase_samples: BTreeMap<String, Vec<f64>> = BTreeMap::new();

    for run_index in 0..num_runs {
        let work_dir = temp_base.path().join(format!("run-{run_index}"));
        fs::create_dir_all(&work_dir)?;
        let run_store = if matches!(
            scenario,
            ScenarioKind::TrueCold | ScenarioKind::ResolvedCold | ScenarioKind::MonorepoCold
        ) {
            temp_base.path().join(format!("bpm-store-cold-{run_index}"))
        } else {
            shared_store.clone()
        };
        fs::create_dir_all(&run_store)?;

        prepare_scenario_with_runner(
            scenario,
            fixture,
            tool,
            &work_dir,
            &run_store,
            run_index + 1,
            runner,
        )?;

        // Only bpm emits `--json-metrics`; capture it per run so phase timings
        // and outbound request counts land in the benchmark output.
        let metrics_path = if tool == Tool::Bpm {
            Some(work_dir.join("bpm-timed-metrics.json"))
        } else {
            None
        };
        let timed_command = install_command_spec(
            tool,
            &work_dir,
            &run_store,
            scenario,
            metrics_path.as_deref(),
        );
        let start = Instant::now();
        let outcome = runner.run(&timed_command)?;
        let elapsed = start.elapsed();
        if outcome.exit_code != 0 {
            anyhow::bail!(
                "timed benchmark failed: tool={}, fixture={}, scenario={}, run={}, exit_code={}",
                tool.name(),
                fixture.name,
                scenario.name(),
                run_index + 1,
                outcome.exit_code
            );
        }

        wall_times.push(elapsed.as_secs_f64() * 1000.0);
        exit_codes.push(outcome.exit_code);

        if let Some(path) = &metrics_path {
            if let Some(file) = read_bpm_metrics(path) {
                request_counts.push(file.counters.requests_sent as f64);
                for (name, ms) in file.phases {
                    phase_samples.entry(name).or_default().push(ms);
                }
            }
        }
    }

    let bpm_metrics = if tool == Tool::Bpm && !request_counts.is_empty() {
        Some(aggregate_bpm_metrics(request_counts, phase_samples))
    } else {
        None
    };

    Ok(ToolResults {
        tool: tool.name().to_string(),
        wall_clock_ms: Stats::compute(wall_times),
        exit_codes,
        bpm_metrics,
    })
}

fn prepare_scenario_with_runner(
    scenario: ScenarioKind,
    fixture: &Fixture,
    tool: Tool,
    work_dir: &Path,
    bpm_store: &Path,
    run_number: usize,
    runner: &mut dyn CommandRunner,
) -> anyhow::Result<()> {
    match scenario {
        ScenarioKind::TrueCold => {
            create_fixture_workspace(fixture, work_dir)?;
            ensure_node_modules_empty(work_dir);
        }
        ScenarioKind::ResolvedCold => {
            create_fixture_workspace(fixture, work_dir)?;
            ensure_node_modules_empty(work_dir);
            run_setup_commands(
                fixture, scenario, tool, work_dir, bpm_store, run_number, runner,
            )?;
        }
        ScenarioKind::WarmStore => {
            create_fixture_workspace(fixture, work_dir)?;
            ensure_node_modules_empty(work_dir);
            run_setup_commands(
                fixture, scenario, tool, work_dir, bpm_store, run_number, runner,
            )?;
            seed_install(
                fixture, scenario, tool, work_dir, bpm_store, run_number, runner,
            )?;
            clear_node_modules(work_dir);
        }
        ScenarioKind::RepeatInstall => {
            create_fixture_workspace(fixture, work_dir)?;
            run_setup_commands(
                fixture, scenario, tool, work_dir, bpm_store, run_number, runner,
            )?;
            seed_install(
                fixture, scenario, tool, work_dir, bpm_store, run_number, runner,
            )?;
        }
        ScenarioKind::SecondProjectSameGraph => {
            create_fixture_workspace(fixture, work_dir)?;
            run_setup_commands(
                fixture, scenario, tool, work_dir, bpm_store, run_number, runner,
            )?;

            let seed = work_dir.with_file_name("seed-project");
            create_fixture_workspace(fixture, &seed)?;
            run_setup_commands(
                fixture, scenario, tool, &seed, bpm_store, run_number, runner,
            )?;
            seed_install(
                fixture, scenario, tool, &seed, bpm_store, run_number, runner,
            )?;
            ensure_node_modules_empty(work_dir);
        }
        ScenarioKind::PartialDependencyChange => {
            create_fixture_workspace(fixture, work_dir)?;
            run_setup_commands(
                fixture, scenario, tool, work_dir, bpm_store, run_number, runner,
            )?;
            seed_install(
                fixture, scenario, tool, work_dir, bpm_store, run_number, runner,
            )?;
            clear_node_modules(work_dir);
        }
        ScenarioKind::MonorepoCold | ScenarioKind::MonorepoIncremental => {
            create_fixture_workspace(fixture, work_dir)?;
            run_setup_commands(
                fixture, scenario, tool, work_dir, bpm_store, run_number, runner,
            )?;
            if matches!(scenario, ScenarioKind::MonorepoIncremental) {
                seed_install(
                    fixture, scenario, tool, work_dir, bpm_store, run_number, runner,
                )?;
                clear_node_modules(work_dir);
            } else {
                ensure_node_modules_empty(work_dir);
            }
        }
    }
    Ok(())
}

fn run_setup_commands(
    fixture: &Fixture,
    scenario: ScenarioKind,
    tool: Tool,
    work_dir: &Path,
    bpm_store: &Path,
    run_number: usize,
    runner: &mut dyn CommandRunner,
) -> anyhow::Result<()> {
    for command in lock_setup_command_specs(tool, work_dir, bpm_store) {
        let outcome = runner.run(&command)?;
        if outcome.exit_code != 0 {
            anyhow::bail!(
                "benchmark setup failed: tool={}, fixture={}, scenario={}, run={}, step={}, exit_code={}",
                tool.name(),
                fixture.name,
                scenario.name(),
                run_number,
                command.label,
                outcome.exit_code
            );
        }
    }
    Ok(())
}

fn seed_install(
    fixture: &Fixture,
    scenario: ScenarioKind,
    tool: Tool,
    work_dir: &Path,
    bpm_store: &Path,
    run_number: usize,
    runner: &mut dyn CommandRunner,
) -> anyhow::Result<()> {
    let command = install_command_spec(tool, work_dir, bpm_store, scenario, None);
    let outcome = runner.run(&command)?;
    if outcome.exit_code != 0 {
        anyhow::bail!(
            "benchmark setup failed: tool={}, fixture={}, scenario={}, run={}, step=seed_install, exit_code={}",
            tool.name(),
            fixture.name,
            scenario.name(),
            run_number,
            outcome.exit_code
        );
    }
    Ok(())
}

fn ensure_node_modules_empty(dir: &Path) {
    let nm = dir.join("node_modules");
    if nm.exists() {
        let _ = fs::remove_dir_all(&nm);
    }
    let _ = fs::create_dir_all(&nm);
}

fn clear_node_modules(dir: &Path) {
    let nm = dir.join("node_modules");
    if nm.exists() {
        let _ = fs::remove_dir_all(&nm);
    }
}

// ---------------------------------------------------------------------------
// Run all benchmarks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RunSuiteOptions {
    pub num_runs: usize,
    pub require_tools: bool,
}

impl RunSuiteOptions {
    pub fn new(num_runs: usize) -> Self {
        Self {
            num_runs,
            require_tools: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BenchSuite {
    pub results: Vec<BenchmarkResult>,
}

pub fn run_suite(
    scenarios: &[ScenarioKind],
    fixture: &Fixture,
    tools: &[Tool],
    options: &RunSuiteOptions,
) -> anyhow::Result<BenchSuite> {
    run_suite_with_availability(scenarios, fixture, tools, options, Tool::detect)
}

pub fn run_suite_with_availability<F>(
    scenarios: &[ScenarioKind],
    fixture: &Fixture,
    tools: &[Tool],
    options: &RunSuiteOptions,
    mut is_available: F,
) -> anyhow::Result<BenchSuite>
where
    F: FnMut(Tool) -> bool,
{
    if options.num_runs == 0 {
        anyhow::bail!("benchmark runs must be at least 1");
    }

    let mut available_tools = Vec::new();
    let mut missing_tools = Vec::new();
    for &tool in tools {
        if is_available(tool) {
            available_tools.push(tool);
        } else {
            missing_tools.push(tool);
        }
    }

    if options.require_tools && !missing_tools.is_empty() {
        anyhow::bail!(
            "required benchmark tools missing from $PATH: {}",
            missing_tools
                .iter()
                .map(Tool::name)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !options.require_tools {
        for tool in &missing_tools {
            eprintln!("warning: {} not found on $PATH, skipping", tool.name());
        }
    }
    if available_tools.is_empty() {
        anyhow::bail!(
            "no benchmark tools available (tried: {})",
            tools.iter().map(Tool::name).collect::<Vec<_>>().join(",")
        );
    }

    let system = SystemInfo::capture();
    let versions = collect_versions(&available_tools);
    if options.require_tools {
        if !versions.contains_key("node") {
            anyhow::bail!("strict benchmark result is missing the node version");
        }
        for tool in &available_tools {
            if !versions.contains_key(tool.name()) {
                anyhow::bail!(
                    "strict benchmark result is missing the {} version",
                    tool.name()
                );
            }
        }
    }

    let mut results = Vec::new();
    for &scenario in scenarios {
        let cache_state = match scenario {
            ScenarioKind::TrueCold | ScenarioKind::ResolvedCold | ScenarioKind::MonorepoCold => {
                "cold"
            }
            ScenarioKind::WarmStore
            | ScenarioKind::SecondProjectSameGraph
            | ScenarioKind::PartialDependencyChange
            | ScenarioKind::MonorepoIncremental => "warm",
            ScenarioKind::RepeatInstall => "hot",
        };

        let mut tool_results = Vec::new();
        for &tool in &available_tools {
            eprintln!(
                "  bench {}/{} ({}) ...",
                fixture.name,
                tool.name(),
                scenario.name()
            );
            tool_results.push(run_scenario(scenario, fixture, tool, options.num_runs)?);
        }

        let result = BenchmarkResult {
            scenario: scenario.name().to_string(),
            fixture: fixture.name.to_string(),
            system: system.clone(),
            versions: versions.clone(),
            cache_state: cache_state.to_string(),
            number_of_runs: options.num_runs,
            tools: tool_results,
        };
        validate_result(&result)?;
        if options.require_tools {
            validate_strict_result(&result, &available_tools)?;
        }
        results.push(result);
    }

    Ok(BenchSuite { results })
}

fn collect_versions(tools: &[Tool]) -> BTreeMap<String, String> {
    let mut versions = BTreeMap::new();
    if let Some(v) = capture_version("node", &["--version"]) {
        versions.insert("node".into(), v);
    }
    for &tool in tools {
        if let Some(v) = capture_tool_version(tool) {
            versions.insert(tool.name().into(), v);
        }
    }
    versions
}

fn validate_result(result: &BenchmarkResult) -> anyhow::Result<()> {
    for tool in &result.tools {
        if tool.exit_codes.len() != result.number_of_runs {
            anyhow::bail!(
                "result invariant failed for {}/{}/{}: expected {} exit codes, found {}",
                result.fixture,
                result.scenario,
                tool.tool,
                result.number_of_runs,
                tool.exit_codes.len()
            );
        }
        if let Some(code) = tool.exit_codes.iter().copied().find(|code| *code != 0) {
            anyhow::bail!(
                "result invariant failed for {}/{}/{}: nonzero exit code {} present in successful result",
                result.fixture,
                result.scenario,
                tool.tool,
                code
            );
        }
    }
    Ok(())
}

fn validate_strict_result(
    result: &BenchmarkResult,
    requested_tools: &[Tool],
) -> anyhow::Result<()> {
    let mut seen = BTreeMap::new();
    for tool in &result.tools {
        seen.insert(tool.tool.as_str(), tool);
    }
    for requested in requested_tools {
        if !seen.contains_key(requested.name()) {
            anyhow::bail!(
                "strict benchmark result missing requested tool {} for {}/{}",
                requested.name(),
                result.fixture,
                result.scenario
            );
        }
    }
    if result.tools.len() != requested_tools.len() {
        anyhow::bail!(
            "strict benchmark result for {}/{} expected {} tools, found {}",
            result.fixture,
            result.scenario,
            requested_tools.len(),
            result.tools.len()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Baseline comparison
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CompareOptions {
    pub regression_envelope: f64,
    pub informational: bool,
}

impl Default for CompareOptions {
    fn default() -> Self {
        Self {
            regression_envelope: 2.0,
            informational: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ComparisonRow {
    pub fixture: String,
    pub scenario: String,
    pub tool: String,
    pub baseline_median_ms: f64,
    pub current_median_ms: f64,
    pub ratio: f64,
    pub baseline_machine: String,
    pub current_machine: String,
    pub baseline_versions: BTreeMap<String, String>,
    pub current_versions: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ResultKey {
    fixture: String,
    scenario: String,
    tool: String,
}

pub fn compare_results_against_baseline(
    baseline: &[BenchmarkResult],
    current: &[BenchmarkResult],
    options: &CompareOptions,
) -> anyhow::Result<Vec<ComparisonRow>> {
    let baseline_index = index_results(baseline, "baseline")?;
    let current_index = index_results(current, "current")?;

    let mut rows = Vec::new();
    for (key, current_entry) in &current_index {
        let Some(baseline_entry) = baseline_index.get(key) else {
            anyhow::bail!(
                "baseline missing comparison key fixture={}, scenario={}, tool={}",
                key.fixture,
                key.scenario,
                key.tool
            );
        };

        validate_entry_exit_codes("baseline", baseline_entry.result, baseline_entry.tool)?;
        validate_entry_exit_codes("current", current_entry.result, current_entry.tool)?;

        let system_matches = baseline_entry.result.system == current_entry.result.system;
        let versions_match = baseline_entry.result.versions == current_entry.result.versions;
        if (!system_matches || !versions_match) && !options.informational {
            anyhow::bail!(
                "baseline comparison requires matching machine/system and versions for fixture={}, scenario={}, tool={}; baseline_machine={} current_machine={} baseline_versions={:?} current_versions={:?}",
                key.fixture,
                key.scenario,
                key.tool,
                baseline_entry.result.system.machine,
                current_entry.result.system.machine,
                baseline_entry.result.versions,
                current_entry.result.versions
            );
        }

        let baseline_median = baseline_entry.tool.wall_clock_ms.median;
        let current_median = current_entry.tool.wall_clock_ms.median;
        let ratio = if baseline_median == 0.0 {
            if current_median == 0.0 {
                1.0
            } else {
                f64::INFINITY
            }
        } else {
            current_median / baseline_median
        };

        if ratio > options.regression_envelope {
            anyhow::bail!(
                "benchmark regression exceeds envelope for fixture={}, scenario={}, tool={}: baseline={:.3}ms current={:.3}ms ratio={:.3} limit={:.3} baseline_machine={} current_machine={} baseline_versions={:?} current_versions={:?}",
                key.fixture,
                key.scenario,
                key.tool,
                baseline_median,
                current_median,
                ratio,
                options.regression_envelope,
                baseline_entry.result.system.machine,
                current_entry.result.system.machine,
                baseline_entry.result.versions,
                current_entry.result.versions
            );
        }

        rows.push(ComparisonRow {
            fixture: key.fixture.clone(),
            scenario: key.scenario.clone(),
            tool: key.tool.clone(),
            baseline_median_ms: baseline_median,
            current_median_ms: current_median,
            ratio,
            baseline_machine: baseline_entry.result.system.machine.clone(),
            current_machine: current_entry.result.system.machine.clone(),
            baseline_versions: baseline_entry.result.versions.clone(),
            current_versions: current_entry.result.versions.clone(),
        });
    }

    rows.sort_by(|a, b| {
        (&a.fixture, &a.scenario, &a.tool).cmp(&(&b.fixture, &b.scenario, &b.tool))
    });
    Ok(rows)
}

struct IndexedEntry<'a> {
    result: &'a BenchmarkResult,
    tool: &'a ToolResults,
}

fn index_results<'a>(
    results: &'a [BenchmarkResult],
    label: &str,
) -> anyhow::Result<BTreeMap<ResultKey, IndexedEntry<'a>>> {
    let mut index = BTreeMap::new();
    for result in results {
        for tool in &result.tools {
            let key = ResultKey {
                fixture: result.fixture.clone(),
                scenario: result.scenario.clone(),
                tool: tool.tool.clone(),
            };
            if index
                .insert(key.clone(), IndexedEntry { result, tool })
                .is_some()
            {
                anyhow::bail!(
                    "{} contains duplicate comparison key fixture={}, scenario={}, tool={}",
                    label,
                    key.fixture,
                    key.scenario,
                    key.tool
                );
            }
        }
    }
    Ok(index)
}

fn validate_entry_exit_codes(
    label: &str,
    result: &BenchmarkResult,
    tool: &ToolResults,
) -> anyhow::Result<()> {
    if let Some(code) = tool.exit_codes.iter().copied().find(|code| *code != 0) {
        anyhow::bail!(
            "{} result has nonzero exit code for fixture={}, scenario={}, tool={}: {}",
            label,
            result.fixture,
            result.scenario,
            tool.tool,
            code
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Optional BPM profiling
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BpmProfileEntry {
    pub fixture: String,
    pub scenario: String,
    pub tool: String,
    pub metrics_file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BpmProfileManifest {
    pub fixture: String,
    pub diagnostic_only: bool,
    pub note: String,
    pub system: SystemInfo,
    pub versions: BTreeMap<String, String>,
    pub profiles: Vec<BpmProfileEntry>,
}

pub fn bpm_profile_filename(fixture: &str, scenario: ScenarioKind) -> String {
    format!("{}--{}--bpm-profile.json", fixture, scenario.name())
}

pub fn profile_bpm_scenarios(
    scenarios: &[ScenarioKind],
    fixture: &Fixture,
    output_dir: &Path,
    system: &SystemInfo,
    versions: &BTreeMap<String, String>,
) -> anyhow::Result<BpmProfileManifest> {
    fs::create_dir_all(output_dir)?;
    let mut runner = ProcessRunner;
    let mut profiles = Vec::new();

    for &scenario in scenarios {
        let metrics_file = bpm_profile_filename(fixture.name, scenario);
        let metrics_path = output_dir.join(&metrics_file);
        profile_bpm_scenario(scenario, fixture, &metrics_path, &mut runner)?;
        profiles.push(BpmProfileEntry {
            fixture: fixture.name.to_string(),
            scenario: scenario.name().to_string(),
            tool: Tool::Bpm.name().to_string(),
            metrics_file,
        });
    }

    let manifest = BpmProfileManifest {
        fixture: fixture.name.to_string(),
        diagnostic_only: true,
        note: "Diagnostic-only BPM phase profile. Summed phase durations can overlap and are not a second wall-clock scorecard.".to_string(),
        system: system.clone(),
        versions: versions.clone(),
        profiles,
    };
    write_bpm_profile_manifest(output_dir, &manifest)?;
    Ok(manifest)
}

pub fn write_bpm_profile_manifest(
    output_dir: &Path,
    manifest: &BpmProfileManifest,
) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(output_dir)?;
    let path = output_dir.join("manifest.json");
    fs::write(&path, serde_json::to_string_pretty(manifest)?)?;
    Ok(path)
}

fn profile_bpm_scenario(
    scenario: ScenarioKind,
    fixture: &Fixture,
    metrics_path: &Path,
    runner: &mut dyn CommandRunner,
) -> anyhow::Result<()> {
    let temp_base = tempfile::tempdir()?;
    let shared_store = temp_base.path().join("bpm-store");
    fs::create_dir_all(&shared_store)?;

    let work_dir = temp_base.path().join("profile-run");
    fs::create_dir_all(&work_dir)?;
    let run_store = if matches!(
        scenario,
        ScenarioKind::TrueCold | ScenarioKind::ResolvedCold | ScenarioKind::MonorepoCold
    ) {
        temp_base.path().join("bpm-store-cold-profile")
    } else {
        shared_store
    };
    fs::create_dir_all(&run_store)?;

    prepare_scenario_with_runner(
        scenario,
        fixture,
        Tool::Bpm,
        &work_dir,
        &run_store,
        1,
        runner,
    )?;
    let command = install_command_spec(
        Tool::Bpm,
        &work_dir,
        &run_store,
        scenario,
        Some(metrics_path),
    );
    let outcome = runner.run(&command)?;
    if outcome.exit_code != 0 {
        anyhow::bail!(
            "bpm profile run failed: fixture={}, scenario={}, exit_code={}",
            fixture.name,
            scenario.name(),
            outcome.exit_code
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

impl BenchSuite {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.results).expect("serialize benchmark results")
    }

    pub fn print_text(&self) {
        for result in &self.results {
            println!("--- {} / {} ---", result.fixture, result.scenario);
            println!("  cache: {}", result.cache_state);
            println!("  runs:  {}", result.number_of_runs);
            for tool in &result.tools {
                println!(
                    "  {}: median={:.1}ms  p95={:.1}ms  σ={:.1}ms",
                    tool.tool,
                    tool.wall_clock_ms.median,
                    tool.wall_clock_ms.p95,
                    tool.wall_clock_ms.stddev,
                );
                if let Some(metrics) = &tool.bpm_metrics {
                    println!(
                        "     requests: median={:.0}  p95={:.0}  (per run)",
                        metrics.requests_sent.median, metrics.requests_sent.p95,
                    );
                    let mut phases: Vec<(&String, &Stats)> = metrics.phase_ms.iter().collect();
                    phases.sort_by(|a, b| b.1.median.partial_cmp(&a.1.median).unwrap());
                    for (name, stats) in phases.iter().take(6) {
                        println!(
                            "     phase {:<24}: median={:.1}ms  p95={:.1}ms",
                            name, stats.median, stats.p95,
                        );
                    }
                }
            }
            println!();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_single_value() {
        let s = Stats::compute(vec![42.0]);
        assert!((s.median - 42.0).abs() < 0.001);
        assert!((s.p95 - 42.0).abs() < 0.001);
        assert!((s.stddev - 0.0).abs() < 0.001);
    }

    #[test]
    fn stats_median_even() {
        let s = Stats::compute(vec![1.0, 2.0, 3.0, 10.0]);
        assert!((s.median - 2.5).abs() < 0.001);
    }

    #[test]
    fn stats_median_odd() {
        let s = Stats::compute(vec![1.0, 2.0, 100.0]);
        assert!((s.median - 2.0).abs() < 0.001);
    }

    #[test]
    fn stats_p95() {
        let mut vals: Vec<f64> = (0..100).map(|i| i as f64).collect();
        let s = Stats::compute(vals.clone());
        vals.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
        let expected_p95 = vals[(vals.len() as f64 * 0.95).ceil() as usize - 1];
        assert!((s.p95 - expected_p95).abs() < 0.001);
    }

    #[test]
    fn stats_empty_values_return_zeroes() {
        let s = Stats::compute(vec![]);
        assert_eq!(s.median, 0.0);
        assert_eq!(s.p95, 0.0);
        assert_eq!(s.stddev, 0.0);
    }

    #[test]
    fn scenario_names() {
        assert_eq!(ScenarioKind::all().len(), 8);
        assert_eq!(ScenarioKind::TrueCold.name(), "true_cold");
        assert_eq!(ScenarioKind::ResolvedCold.name(), "resolved_cold");
        assert_eq!(ScenarioKind::WarmStore.name(), "warm_store");
        assert_eq!(ScenarioKind::RepeatInstall.name(), "repeat_install");
    }

    #[test]
    fn fixture_list() {
        assert!(!FIXTURES.is_empty());
        for f in FIXTURES {
            assert!(!f.packages.is_empty());
        }
    }

    #[test]
    fn bpm_timed_install_uses_frozen_for_lock_scenarios_only() {
        let work_dir = Path::new("/tmp/work");
        let store = Path::new("/tmp/store");
        let locked =
            install_command_spec(Tool::Bpm, work_dir, store, ScenarioKind::ResolvedCold, None);
        assert!(locked.args.contains(&"--frozen".to_string()));

        let cold = install_command_spec(Tool::Bpm, work_dir, store, ScenarioKind::TrueCold, None);
        assert!(!cold.args.contains(&"--frozen".to_string()));
    }

    #[test]
    fn aggregate_bpm_metrics_reports_per_run_requests_and_phases() {
        let request_counts = vec![12.0, 12.0, 13.0];
        let mut phase_samples = BTreeMap::new();
        phase_samples.insert(
            "dependency_resolution".to_string(),
            vec![100.0, 120.0, 140.0],
        );
        phase_samples.insert("artifact_download".to_string(), vec![5.0, 6.0, 7.0]);

        let summary = aggregate_bpm_metrics(request_counts, phase_samples);

        assert_eq!(summary.requests_sent.values.len(), 3);
        assert!((summary.requests_sent.median - 12.0).abs() < 0.001);
        let resolve = summary.phase_ms.get("dependency_resolution").unwrap();
        assert!((resolve.median - 120.0).abs() < 0.001);
        let download = summary.phase_ms.get("artifact_download").unwrap();
        assert!((download.median - 6.0).abs() < 0.001);
    }

    #[test]
    fn tool_results_round_trips_with_and_without_bpm_metrics() {
        // Without bpm_metrics: existing reference baselines still deserialize.
        let without = serde_json::json!({
            "tool": "bpm",
            "wall_clock_ms": {"values": [1.0], "median": 1.0, "p95": 1.0, "stddev": 0.0},
            "exit_codes": [0],
        });
        let parsed: ToolResults = serde_json::from_value(without).unwrap();
        assert!(parsed.bpm_metrics.is_none());

        // With bpm_metrics: round-trips and is omitted when None.
        let with = ToolResults {
            tool: "bpm".to_string(),
            wall_clock_ms: Stats::compute(vec![1.0]),
            exit_codes: vec![0],
            bpm_metrics: Some(BpmMetricsSummary {
                requests_sent: Stats::compute(vec![5.0]),
                phase_ms: BTreeMap::from([(
                    "dependency_resolution".to_string(),
                    Stats::compute(vec![10.0]),
                )]),
            }),
        };
        let json = serde_json::to_string(&with).unwrap();
        assert!(json.contains("bpm_metrics"));
        assert!(json.contains("requests_sent"));
        let back: ToolResults = serde_json::from_str(&json).unwrap();
        assert!((back.bpm_metrics.unwrap().requests_sent.median - 5.0).abs() < 0.001);

        let none = ToolResults {
            tool: "npm".to_string(),
            wall_clock_ms: Stats::compute(vec![1.0]),
            exit_codes: vec![0],
            bpm_metrics: None,
        };
        assert!(!serde_json::to_string(&none)
            .unwrap()
            .contains("bpm_metrics"));
    }
}
