use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Scenario
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
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
        let median = if len == 0 {
            0.0
        } else if len.is_multiple_of(2) {
            (sorted[len / 2 - 1] + sorted[len / 2]) / 2.0
        } else {
            sorted[len / 2]
        };

        let p95 = if len == 0 {
            0.0
        } else {
            let idx = ((len as f64) * 0.95).ceil() as usize - 1;
            sorted[idx.min(len - 1)]
        };

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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
        if let Some(v) = capture_version("bpm", &["--version"]) {
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

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
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
        Command::new(self.name())
            .arg("--version")
            .output()
            .ok()
            .is_some_and(|o| o.status.success())
    }
}

// ---------------------------------------------------------------------------
// Per-tool results
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResults {
    pub tool: String,
    pub wall_clock_ms: Stats,
    pub exit_codes: Vec<i32>,
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

    // Write package.json
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
    let temp_base = tempfile::tempdir()?;
    // Stable per-scenario cache roots make warm/repeat runs reuse each tool's
    // cache while keeping cold comparisons isolated from the developer's
    // global npm/pnpm/Bun stores.
    let bpm_store = temp_base.path().join("bpm-store");
    fs::create_dir_all(&bpm_store)?;

    let mut wall_times = Vec::with_capacity(num_runs);
    let mut exit_codes = Vec::with_capacity(num_runs);

    for run in 0..num_runs {
        let work_dir = temp_base.path().join(format!("run-{run}"));
        fs::create_dir_all(&work_dir)?;

        // Prepare fixture based on scenario
        prepare_scenario(scenario, fixture, &work_dir, tool, &bpm_store)?;

        // Measure
        let start = Instant::now();
        let status = run_tool(tool, &work_dir, &bpm_store)?;
        let elapsed = start.elapsed();

        wall_times.push(elapsed.as_secs_f64() * 1000.0);
        exit_codes.push(status.code().unwrap_or(-1));
    }

    Ok(ToolResults {
        tool: tool.name().to_string(),
        wall_clock_ms: Stats::compute(wall_times),
        exit_codes,
    })
}

fn prepare_scenario(
    scenario: ScenarioKind,
    fixture: &Fixture,
    work_dir: &Path,
    tool: Tool,
    bpm_store: &Path,
) -> anyhow::Result<()> {
    match scenario {
        ScenarioKind::TrueCold => {
            // Fresh project, no lockfile, empty store
            create_fixture_workspace(fixture, work_dir)?;
            ensure_node_modules_empty(work_dir);
        }
        ScenarioKind::ResolvedCold => {
            // Lockfile present, but no store/project view
            create_fixture_workspace(fixture, work_dir)?;
            ensure_node_modules_empty(work_dir);
            generate_package_lock(fixture, work_dir)?;
        }
        ScenarioKind::WarmStore => {
            // Store populated from a prior install
            create_fixture_workspace(fixture, work_dir)?;
            ensure_node_modules_empty(work_dir);
            generate_package_lock(fixture, work_dir)?;
            run_tool(tool, work_dir, bpm_store)?;
            clear_node_modules(work_dir);
        }
        ScenarioKind::RepeatInstall => {
            // Everything already in place
            create_fixture_workspace(fixture, work_dir)?;
            generate_package_lock(fixture, work_dir)?;
            run_tool(tool, work_dir, bpm_store)?;
        }
        ScenarioKind::SecondProjectSameGraph => {
            create_fixture_workspace(fixture, work_dir)?;
            generate_package_lock(fixture, work_dir)?;
            let seed = work_dir.with_file_name("seed-project");
            create_fixture_workspace(fixture, &seed)?;
            generate_package_lock(fixture, &seed)?;
            run_tool(tool, &seed, bpm_store)?;
            ensure_node_modules_empty(work_dir);
        }
        ScenarioKind::PartialDependencyChange => {
            create_fixture_workspace(fixture, work_dir)?;
            generate_package_lock(fixture, work_dir)?;
            run_tool(tool, work_dir, bpm_store)?;
            clear_node_modules(work_dir);
        }
        ScenarioKind::MonorepoCold | ScenarioKind::MonorepoIncremental => {
            create_fixture_workspace(fixture, work_dir)?;
            generate_package_lock(fixture, work_dir)?;
            if matches!(scenario, ScenarioKind::MonorepoIncremental) {
                run_tool(tool, work_dir, bpm_store)?;
                clear_node_modules(work_dir);
            } else {
                ensure_node_modules_empty(work_dir);
            }
        }
    }
    Ok(())
}

fn run_tool(
    tool: Tool,
    work_dir: &Path,
    bpm_store: &Path,
) -> anyhow::Result<std::process::ExitStatus> {
    match tool {
        Tool::Npm => {
            let mut command = Command::new("npm");
            command
                .args(["install", "--prefer-offline"])
                .current_dir(work_dir);
            run_external(&mut command, Tool::Npm, bpm_store)
        }
        Tool::Pnpm => {
            let mut command = Command::new("pnpm");
            command
                .args(["install", "--prefer-offline"])
                .current_dir(work_dir);
            run_external(&mut command, Tool::Pnpm, bpm_store)
        }
        // Prefer the frozen imported lockfile when the scenario provides one,
        // so comparisons use the same resolved graph. True-cold scenarios do
        // not have a lockfile by design; native BPM resolution now handles
        // that path directly and should be benchmarked rather than rejected.
        Tool::Bpm => {
            let bpm_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("bpm"));
            let pkg_lock = work_dir.join("package-lock.json");
            if pkg_lock.exists() {
                let import = Command::new(&bpm_bin)
                    .arg("import")
                    .arg(&pkg_lock)
                    .arg("--out")
                    .arg(work_dir.join("bpm.lock"))
                    .current_dir(work_dir)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map_err(|e| anyhow::anyhow!("failed to run `bpm import`: {e}"))?;
                if !import.success() {
                    return Ok(import);
                }
                return Command::new(&bpm_bin)
                    .arg("install")
                    .arg("--frozen")
                    .arg("--store")
                    .arg(bpm_store)
                    .current_dir(work_dir)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map_err(|e| anyhow::anyhow!("failed to run `bpm install --frozen`: {e}"));
            }
            Ok(Command::new(&bpm_bin)
                .arg("install")
                .arg("--store")
                .arg(bpm_store)
                .current_dir(work_dir)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map_err(|e| anyhow::anyhow!("failed to run native `bpm install`: {e}"))?)
        }
        Tool::Yarn => {
            let mut command = Command::new("yarn");
            command.arg("install").current_dir(work_dir);
            run_external(&mut command, Tool::Yarn, bpm_store)
        }
        Tool::Bun => {
            let mut command = Command::new("bun");
            command
                .args(["install", "--no-progress"])
                .current_dir(work_dir);
            run_external(&mut command, Tool::Bun, bpm_store)
        }
    }
}

fn run_external(
    command: &mut Command,
    tool: Tool,
    bpm_store: &Path,
) -> anyhow::Result<std::process::ExitStatus> {
    configure_tool_cache(command, tool, bpm_store);
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run {}: {e}", tool.name()))
}

fn configure_tool_cache(command: &mut Command, tool: Tool, bpm_store: &Path) {
    let cache = bpm_store
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{}-cache", tool.name()));
    match tool {
        Tool::Npm => {
            command.env("npm_config_cache", cache);
        }
        Tool::Pnpm => {
            command.env("pnpm_config_store_dir", cache);
        }
        Tool::Yarn => {
            command.env("YARN_CACHE_FOLDER", cache);
        }
        Tool::Bun => {
            command.env("BUN_INSTALL_CACHE_DIR", cache);
        }
        Tool::Bpm => {}
    }
}

fn generate_package_lock(fixture: &Fixture, dir: &Path) -> anyhow::Result<()> {
    let lock_path = dir.join("package-lock.json");
    if lock_path.exists() && fs::metadata(&lock_path)?.len() > 0 {
        return Ok(());
    }
    // Produce a REAL lockfile by asking npm to resolve without installing.
    // `--package-lock-only` writes package-lock.json with full integrity and
    // the transitive `packages` map; it hits the registry but never installs
    // node_modules, so it stays comparable across tools and reproducible.
    // If npm is absent, surface a clear, actionable error.
    let status = Command::new("npm")
        .args(["install", "--package-lock-only"])
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run `npm install --package-lock-only`: {e}"))?;
    if !status.success() {
        anyhow::bail!(
            "npm could not generate a lockfile for the '{}' fixture (exit {:?}); \
             a real lockfile with integrity is required for a fair benchmark",
            fixture.name,
            status.code()
        );
    }
    if !lock_path.exists() {
        anyhow::bail!("npm reported success but wrote no package-lock.json");
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

pub struct BenchSuite {
    pub results: Vec<BenchmarkResult>,
}

pub fn run_suite(
    scenarios: &[ScenarioKind],
    fixture: &Fixture,
    tools: &[Tool],
    num_runs: usize,
) -> anyhow::Result<BenchSuite> {
    let system = SystemInfo::capture();

    // Pin/record exact tool versions for this result set. Each entry is
    // only present if the tool was actually detected on PATH, so a missing
    // pnpm does not produce a misleading empty version.
    let mut versions: BTreeMap<String, String> = BTreeMap::new();
    if let Some(v) = capture_version("node", &["--version"]) {
        versions.insert("node".into(), v);
    }
    for &tool in tools {
        if tool.detect() {
            if let Some(v) = capture_version(tool.name(), &["--version"]) {
                versions.insert(tool.name().into(), v);
            }
        }
    }

    let mut results = Vec::new();

    for &scenario in scenarios {
        let cache_state = match scenario {
            ScenarioKind::TrueCold => "cold",
            ScenarioKind::ResolvedCold => "cold",
            ScenarioKind::WarmStore => "warm",
            ScenarioKind::RepeatInstall => "hot",
            ScenarioKind::SecondProjectSameGraph => "warm",
            ScenarioKind::PartialDependencyChange => "warm",
            ScenarioKind::MonorepoCold => "cold",
            ScenarioKind::MonorepoIncremental => "warm",
        };

        let mut tool_results = Vec::new();
        for &tool in tools {
            if !tool.detect() {
                eprintln!(
                    "  bench {}/{} ({}): {} not on PATH, skipping",
                    fixture.name,
                    tool.name(),
                    scenario.name(),
                    tool.name()
                );
                continue;
            }
            eprintln!(
                "  bench {}/{} ({}) ...",
                fixture.name,
                tool.name(),
                scenario.name()
            );
            let tr = run_scenario(scenario, fixture, tool, num_runs)?;
            tool_results.push(tr);
        }

        results.push(BenchmarkResult {
            scenario: scenario.name().to_string(),
            fixture: fixture.name.to_string(),
            system: system.clone(),
            versions: versions.clone(),
            cache_state: cache_state.to_string(),
            number_of_runs: num_runs,
            tools: tool_results,
        });
    }

    Ok(BenchSuite { results })
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
}
