use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::net::SocketAddr;
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
        sorted.sort_unstable_by(|a, b| a.total_cmp(b));

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
        let probe = ProbeEnvironment::new().ok();
        let mut cache = VersionProbeCache::default();
        capture_system_info(probe.as_ref(), &mut cache)
    }
}

fn capture_system_info(
    probe: Option<&ProbeEnvironment>,
    cache: &mut VersionProbeCache,
) -> SystemInfo {
    let machine = cmd_stdout_or_default("uname", &["-m"]);
    let operating_system = cmd_stdout_or_default("sw_vers", &["-productVersion"]);
    let kernel = cmd_stdout_or_default("uname", &["-r"]);

    let mut runtime_versions = BTreeMap::new();
    if let Some(probe) = probe {
        if let Some(v) = cache.node_version(probe) {
            runtime_versions.insert("node".into(), v);
        }
        for (name, tool) in [("npm", Tool::Npm), ("pnpm", Tool::Pnpm), ("bpm", Tool::Bpm)] {
            if let Some(v) = cache.tool_version(probe, tool) {
                runtime_versions.insert(name.into(), v);
            }
        }
    }

    SystemInfo {
        machine,
        operating_system,
        kernel,
        runtime_versions,
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
    let probe = ProbeEnvironment::new().ok()?;
    capture_version_with_probe(&probe, cmd, args)
}

fn capture_version_with_probe(
    probe: &ProbeEnvironment,
    cmd: &str,
    args: &[&str],
) -> Option<String> {
    let env_tool = match cmd {
        "npm" => Tool::Npm,
        "pnpm" => Tool::Pnpm,
        "yarn" => Tool::Yarn,
        "bun" => Tool::Bun,
        _ => Tool::Bpm,
    };
    let command = probe.command_spec_for_program(cmd, env_tool, args);
    capture_version_from_spec(&command)
}

fn bpm_binary() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("bpm"))
}

fn capture_bpm_version() -> Option<String> {
    let probe = ProbeEnvironment::new().ok()?;
    capture_bpm_version_with_probe(&probe)
}

fn capture_bpm_version_with_probe(probe: &ProbeEnvironment) -> Option<String> {
    let command = probe.command_spec(Tool::Bpm);
    capture_version_from_spec(&command)
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
        capture_tool_version(self).is_some()
    }
}

fn capture_tool_version(tool: Tool) -> Option<String> {
    match tool {
        Tool::Bpm => capture_bpm_version(),
        _ => capture_version(tool.name(), &["--version"]),
    }
}

fn capture_tool_version_with_probe(probe: &ProbeEnvironment, tool: Tool) -> Option<String> {
    match tool {
        Tool::Bpm => capture_bpm_version_with_probe(probe),
        _ => {
            let command = probe.command_spec_for_program(tool.name(), tool, &["--version"]);
            capture_version_from_spec(&command)
        }
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
    /// Numeric scalar diagnostics (for example peak HTTP concurrency) kept
    /// separate from duration phases so the report preserves their units.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub counters: BTreeMap<String, Stats>,
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
    /// Cross-tool network shape captured by the parity proxy when
    /// `bpm bench --profile-parity` is set. Absent otherwise so existing
    /// baselines without this field still deserialize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<crate::parity_proxy::NetworkShape>,
    /// One network shape per logical sample. This is intentionally separate
    /// from `network`, whose aggregate remains for older consumers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network_samples: Vec<crate::parity_proxy::NetworkShape>,
}

// ---------------------------------------------------------------------------
// Benchmark protocol
// ---------------------------------------------------------------------------

pub const BENCHMARK_PROTOCOL_VERSION: u32 = 1;

/// Identity of the benchmark measurement protocol. Results without this
/// field are readable historical data, but are not valid cross-tool evidence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BenchmarkProtocol {
    pub protocol_version: u32,
    pub cache_isolation_mode: String,
    pub lifecycle_policy: String,
    pub execution_mode: String,
    pub post_install_validation: String,
    pub round_tool_order: Vec<Vec<String>>,
    /// Parity proxy runs intentionally have a different wall-clock transport
    /// and must never be accepted by the superiority gate.
    #[serde(default)]
    pub profile_parity: bool,
}

impl BenchmarkProtocol {
    fn current(
        ignore_scripts: bool,
        profile_parity: bool,
        round_tool_order: Vec<Vec<String>>,
    ) -> Self {
        Self {
            protocol_version: BENCHMARK_PROTOCOL_VERSION,
            cache_isolation_mode: "per-sample-home-v1".into(),
            lifecycle_policy: if ignore_scripts {
                "scripts-off".into()
            } else {
                "scripts-on".into()
            },
            execution_mode: "round-robin-rotated-v1".into(),
            post_install_validation: "fixture-smoke-v1".into(),
            round_tool_order,
            profile_parity,
        }
    }
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
    /// Optional for backward deserialization of protocol-less historical
    /// baselines. Newly generated results always populate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<BenchmarkProtocol>,
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
    /// Exact inherited environment names to remove before applying `env`.
    /// Names are collected without reading their values, so hostile config and
    /// credential-bearing variables never enter a command spec.
    pub env_remove: BTreeSet<OsString>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutcome {
    pub exit_code: i32,
}

pub trait CommandRunner {
    fn run(&mut self, command: &CommandSpec) -> anyhow::Result<CommandOutcome>;
}

fn apply_command_spec(process: &mut Command, command: &CommandSpec) {
    process
        .args(&command.args)
        .current_dir(&command.current_dir);
    for key in &command.env_remove {
        process.env_remove(key);
    }
    for (key, value) in &command.env {
        process.env(key, value);
    }
}

fn capture_version_from_spec(command: &CommandSpec) -> Option<String> {
    let mut process = Command::new(&command.program);
    apply_command_spec(&mut process, command);
    let output = process.output().ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

struct ProcessRunner;

impl CommandRunner for ProcessRunner {
    fn run(&mut self, command: &CommandSpec) -> anyhow::Result<CommandOutcome> {
        let mut process = Command::new(&command.program);
        apply_command_spec(&mut process, command);
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

/// Environment roots private to one benchmark sample. The process environment
/// is not cleared: PATH, loader variables, and proxy conventions remain
/// inherited, while all package-manager home/config/cache/state lookups are
/// redirected below the sample or an explicitly isolated shared cache root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleEnvironment {
    pub sample_home: PathBuf,
    pub cache_root: PathBuf,
    inherited_names: Option<BTreeSet<OsString>>,
}

impl SampleEnvironment {
    pub fn new(sample_home: impl Into<PathBuf>) -> Self {
        let sample_home = sample_home.into();
        Self {
            cache_root: sample_home.join("tool-cache"),
            sample_home,
            inherited_names: None,
        }
    }

    /// Construct an environment with explicit inherited names for deterministic
    /// command-spec tests. Production environments use the live process names.
    pub fn with_inherited_names(
        sample_home: impl Into<PathBuf>,
        inherited_names: impl IntoIterator<Item = OsString>,
    ) -> Self {
        let sample_home = sample_home.into();
        Self {
            cache_root: sample_home.join("tool-cache"),
            sample_home,
            inherited_names: Some(inherited_names.into_iter().collect()),
        }
    }

    /// Construct an environment with a private sample home and a cache root
    /// whose lifetime can span warm/hot samples for exactly one tool.
    pub fn with_cache_root(
        sample_home: impl Into<PathBuf>,
        cache_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            sample_home: sample_home.into(),
            cache_root: cache_root.into(),
            inherited_names: None,
        }
    }

    fn xdg_cache_home(&self) -> PathBuf {
        self.sample_home.join("xdg-cache")
    }

    fn xdg_config_home(&self) -> PathBuf {
        self.sample_home.join("xdg-config")
    }

    fn xdg_data_home(&self) -> PathBuf {
        self.sample_home.join("xdg-data")
    }

    fn xdg_state_home(&self) -> PathBuf {
        self.sample_home.join("xdg-state")
    }

    fn pnpm_home(&self) -> PathBuf {
        self.sample_home.join("pnpm-home")
    }

    /// Corepack state follows the selected cache lifetime: a cold sample gets
    /// a fresh directory, while warm/hot samples share it only with the
    /// corresponding tool's isolated cache root.
    fn corepack_home(&self) -> PathBuf {
        self.cache_root.join("corepack-home")
    }

    fn npmrc(&self) -> PathBuf {
        self.sample_home.join(".npmrc")
    }

    fn global_npmrc(&self) -> PathBuf {
        self.sample_home.join(".npmrc-global")
    }

    fn tool_cache(&self, tool: Tool) -> PathBuf {
        self.cache_root.join(tool.name())
    }

    /// Create every path that a sample command can use before spawning it.
    pub fn prepare(&self, tool: Tool, bpm_store: &Path) -> anyhow::Result<()> {
        for path in [
            self.sample_home.clone(),
            self.cache_root.clone(),
            self.xdg_cache_home(),
            self.xdg_config_home(),
            self.xdg_data_home(),
            self.xdg_state_home(),
            self.pnpm_home(),
            self.corepack_home(),
            self.tool_cache(Tool::Npm),
            self.tool_cache(Tool::Pnpm),
            self.tool_cache(Tool::Yarn),
            self.tool_cache(Tool::Bun),
            self.tool_cache(tool),
            bpm_store.to_path_buf(),
            pnpm_store_dir(bpm_store),
        ] {
            fs::create_dir_all(path)?;
        }
        // Always replace both config files. A reused sample root must not
        // retain a hostile value from an earlier setup, and no operator
        // config is ever read or copied into either file.
        fs::write(self.npmrc(), "")?;
        fs::write(self.global_npmrc(), "")?;
        Ok(())
    }

    fn command_env_removals(&self) -> BTreeSet<OsString> {
        self.inherited_names
            .as_ref()
            .map(|names| package_manager_config_names(names.iter().cloned()))
            .unwrap_or_else(inherited_package_manager_config_names)
    }

    fn command_env(&self, tool: Tool, bpm_store: &Path) -> BTreeMap<String, String> {
        let mut env = BTreeMap::from([
            (
                "HOME".into(),
                self.sample_home.to_string_lossy().into_owned(),
            ),
            (
                "XDG_CACHE_HOME".into(),
                self.xdg_cache_home().to_string_lossy().into_owned(),
            ),
            (
                "XDG_CONFIG_HOME".into(),
                self.xdg_config_home().to_string_lossy().into_owned(),
            ),
            (
                "XDG_DATA_HOME".into(),
                self.xdg_data_home().to_string_lossy().into_owned(),
            ),
            (
                "XDG_STATE_HOME".into(),
                self.xdg_state_home().to_string_lossy().into_owned(),
            ),
            (
                "PNPM_HOME".into(),
                self.pnpm_home().to_string_lossy().into_owned(),
            ),
            (
                "COREPACK_HOME".into(),
                self.corepack_home().to_string_lossy().into_owned(),
            ),
            ("COREPACK_NPM_REGISTRY".into(), controlled_registry_url()),
            ("COREPACK_ENV_FILE".into(), "0".into()),
            (
                "npm_config_userconfig".into(),
                self.npmrc().to_string_lossy().into_owned(),
            ),
            (
                "npm_config_globalconfig".into(),
                self.global_npmrc().to_string_lossy().into_owned(),
            ),
            ("BPM_STORE".into(), bpm_store.to_string_lossy().into_owned()),
        ]);

        match tool {
            Tool::Npm => {
                env.insert(
                    "npm_config_cache".into(),
                    self.tool_cache(Tool::Npm).to_string_lossy().into_owned(),
                );
            }
            Tool::Pnpm => {
                env.insert(
                    "npm_config_cache".into(),
                    self.tool_cache(Tool::Pnpm).to_string_lossy().into_owned(),
                );
            }
            Tool::Yarn => {
                env.insert(
                    "YARN_CACHE_FOLDER".into(),
                    self.tool_cache(Tool::Yarn).to_string_lossy().into_owned(),
                );
            }
            Tool::Bun => {
                env.insert(
                    "BUN_INSTALL_CACHE_DIR".into(),
                    self.tool_cache(Tool::Bun).to_string_lossy().into_owned(),
                );
            }
            Tool::Bpm => {}
        }
        env
    }
}

fn tool_cache_root(tool: Tool, bpm_store: &Path) -> PathBuf {
    bpm_store.join(format!("{}-cache", tool.name()))
}

fn pnpm_store_dir(bpm_store: &Path) -> PathBuf {
    tool_cache_root(Tool::Pnpm, bpm_store).join("store")
}

fn build_sample_command_spec(
    label: &'static str,
    program: impl Into<PathBuf>,
    args: impl IntoIterator<Item = impl Into<String>>,
    current_dir: &Path,
    env: BTreeMap<String, String>,
    sample_env: &SampleEnvironment,
) -> CommandSpec {
    build_command_spec_with_removals(
        label,
        program,
        args,
        current_dir,
        env,
        sample_env.command_env_removals(),
    )
}

fn build_command_spec_with_removals(
    label: &'static str,
    program: impl Into<PathBuf>,
    args: impl IntoIterator<Item = impl Into<String>>,
    current_dir: &Path,
    env: BTreeMap<String, String>,
    env_remove: BTreeSet<OsString>,
) -> CommandSpec {
    CommandSpec {
        label,
        program: program.into(),
        args: args.into_iter().map(Into::into).collect(),
        current_dir: current_dir.to_path_buf(),
        env,
        env_remove,
    }
}

const CONTROLLED_PACKAGE_MANAGER_ENV_NAMES: &[&str] = &[
    "NPM_CONFIG_USERCONFIG",
    "NPM_CONFIG_GLOBALCONFIG",
    "NPM_CONFIG_REGISTRY",
    "NPM_CONFIG_CACHE",
    "NPM_CONFIG_AUTH",
    "NODE_AUTH_TOKEN",
    "NPM_TOKEN",
    "NPM_AUTH_TOKEN",
    "PNPM_HOME",
    "YARN_CACHE_FOLDER",
    "BUN_INSTALL_CACHE_DIR",
    "BPM_STORE",
    "BPM_REGISTRY",
];

/// Return whether an inherited variable can override package-manager config.
/// Matching is ASCII-case-insensitive even on Unix (where the environment is
/// case-sensitive), because npm normalizes config variable names and Windows
/// treats environment names case-insensitively.
fn is_package_manager_config_name(name: &OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_lowercase();
    ["npm_config_", "pnpm_", "yarn_", "bun_", "bpm_", "corepack_"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || matches!(
            name.as_str(),
            "node_auth_token" | "npm_token" | "npm_auth_token"
        )
}

/// Build removals from names only. Values are deliberately not read or copied.
fn package_manager_config_names<I>(names: I) -> BTreeSet<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    let mut removals: BTreeSet<OsString> = names
        .into_iter()
        .filter(|name| is_package_manager_config_name(name))
        .collect();
    // These canonical spellings cover the controlled names on
    // case-insensitive process environments even if they were absent when the
    // command spec was built.
    removals.extend(
        CONTROLLED_PACKAGE_MANAGER_ENV_NAMES
            .iter()
            .map(OsString::from),
    );
    removals.extend(CONTROLLED_COREPACK_ENV_NAMES.iter().map(OsString::from));
    removals
}

fn inherited_package_manager_config_names() -> BTreeSet<OsString> {
    package_manager_config_names(std::env::vars_os().map(|(name, _)| name))
}

const CONTROLLED_COREPACK_ENV_NAMES: &[&str] = &[
    "COREPACK_HOME",
    "COREPACK_ENV_FILE",
    "COREPACK_NPM_REGISTRY",
    "COREPACK_NPM_TOKEN",
    "COREPACK_NPM_USERNAME",
    "COREPACK_NPM_PASSWORD",
    "COREPACK_ENABLE_NETWORK",
    "COREPACK_DEFAULT_TO_LATEST",
    "COREPACK_ENABLE_PROJECT_SPEC",
    "COREPACK_ENABLE_STRICT",
    "COREPACK_INTEGRITY_KEYS",
    "COREPACK_ENABLE_DOWNLOAD_PROMPT",
];

fn is_probe_extra_config_name(name: &OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_lowercase();
    name == "node_options" || name == "node_path" || name.starts_with("corepack_")
}

fn probe_package_manager_config_names<I>(names: I) -> BTreeSet<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    let mut removals: BTreeSet<OsString> = names
        .into_iter()
        .filter(|name| is_package_manager_config_name(name) || is_probe_extra_config_name(name))
        .collect();
    removals.extend(
        CONTROLLED_PACKAGE_MANAGER_ENV_NAMES
            .iter()
            .map(OsString::from),
    );
    removals.extend(CONTROLLED_COREPACK_ENV_NAMES.iter().map(OsString::from));
    removals.extend([OsString::from("NODE_OPTIONS"), OsString::from("NODE_PATH")]);
    removals
}

fn inherited_probe_config_names() -> BTreeSet<OsString> {
    probe_package_manager_config_names(std::env::vars_os().map(|(name, _)| name))
}

#[derive(Default)]
struct VersionProbeCache {
    node: Option<Option<String>>,
    tools: BTreeMap<Tool, Option<String>>,
}

impl VersionProbeCache {
    fn node_version(&mut self, probe: &ProbeEnvironment) -> Option<String> {
        if let Some(version) = &self.node {
            return version.clone();
        }
        let version = capture_version_with_probe(probe, "node", &["--version"]);
        self.node = Some(version.clone());
        version
    }

    fn tool_version(&mut self, probe: &ProbeEnvironment, tool: Tool) -> Option<String> {
        if let Some(version) = self.tools.get(&tool) {
            return version.clone();
        }
        let version = capture_tool_version_with_probe(probe, tool);
        self.tools.insert(tool, version.clone());
        version
    }
}

/// The short-lived environment used by availability and version probes. It is
/// deliberately separate from the caller's current project and home: probe
/// commands get a private cwd, HOME/XDG roots, npmrc files, registry, and
/// caches, while the process environment remains inherited for PATH, loader,
/// and proxy requirements.
struct ProbeEnvironment {
    _temp_root: tempfile::TempDir,
    sample_env: SampleEnvironment,
    work_dir: PathBuf,
    bpm_store: PathBuf,
}

impl ProbeEnvironment {
    fn new() -> anyhow::Result<Self> {
        let temp_root = tempfile::tempdir()?;
        let root = temp_root.path().to_path_buf();
        let sample_env = SampleEnvironment::new(root.join("home"));
        let work_dir = root.join("project");
        let bpm_store = root.join("bpm-store");

        fs::create_dir_all(&work_dir)?;
        sample_env.prepare(Tool::Bpm, &bpm_store)?;
        // The project file is the only non-empty npmrc: it contains the
        // controlled public registry and no user or auth material.
        fs::write(
            work_dir.join(".npmrc"),
            crate::parity_proxy::public_registry_npmrc_content(),
        )?;

        Ok(Self {
            _temp_root: temp_root,
            sample_env,
            work_dir,
            bpm_store,
        })
    }

    fn command_env(&self, tool: Tool) -> BTreeMap<String, String> {
        let mut env = self.sample_env.command_env(tool, &self.bpm_store);
        // Version probes use the public registry for npm's own config because
        // they run outside a prepared fixture. Normal sample commands leave
        // npm_config_registry unset so the fixture's project .npmrc remains
        // authoritative for package downloads.
        env.insert("npm_config_registry".into(), controlled_registry_url());
        env
    }

    fn command_spec(&self, tool: Tool) -> CommandSpec {
        self.command_spec_for_program(tool_program(tool), tool, &["--version"])
    }

    fn command_spec_for_program(
        &self,
        program: impl Into<PathBuf>,
        env_tool: Tool,
        args: &[&str],
    ) -> CommandSpec {
        build_command_spec_with_removals(
            "version_probe",
            program,
            args.iter().copied(),
            &self.work_dir,
            self.command_env(env_tool),
            inherited_probe_config_names(),
        )
    }
}

fn tool_program(tool: Tool) -> PathBuf {
    match tool {
        Tool::Bpm => bpm_binary(),
        _ => PathBuf::from(tool.name()),
    }
}

fn controlled_registry_url() -> String {
    crate::parity_proxy::public_registry_npmrc_content()
        .trim()
        .strip_prefix("registry=")
        .unwrap_or_default()
        .to_string()
}

/// Construct a version-probe command spec under `probe_root` without reading
/// any environment values. This pure constructor is also used by regression
/// tests with an explicitly hostile inherited-name set.
pub fn version_probe_command_spec(
    tool: Tool,
    probe_root: &Path,
    inherited_names: &[OsString],
) -> CommandSpec {
    let sample_env = SampleEnvironment::new(probe_root.join("home"));
    let work_dir = probe_root.join("project");
    let bpm_store = probe_root.join("bpm-store");
    let mut env = sample_env.command_env(tool, &bpm_store);
    // Keep version discovery on the public benchmark registry without
    // allowing inherited policy, network, or credential variables to return.
    env.insert("npm_config_registry".into(), controlled_registry_url());
    build_command_spec_with_removals(
        "version_probe",
        tool_program(tool),
        ["--version"],
        &work_dir,
        env,
        probe_package_manager_config_names(inherited_names.iter().cloned()),
    )
}

/// Backward-compatible command-spec helper. Production benchmark execution
/// uses `lock_setup_command_specs_with_env` so every sample has an explicit
/// environment.
pub fn lock_setup_command_specs(tool: Tool, work_dir: &Path, bpm_store: &Path) -> Vec<CommandSpec> {
    let sample_env = SampleEnvironment::new(work_dir.join(".sample-home"));
    lock_setup_command_specs_with_env(tool, work_dir, bpm_store, &sample_env)
}

pub fn lock_setup_command_specs_with_env(
    tool: Tool,
    work_dir: &Path,
    bpm_store: &Path,
    sample_env: &SampleEnvironment,
) -> Vec<CommandSpec> {
    match tool {
        Tool::Npm => vec![build_sample_command_spec(
            "setup_lockfile",
            "npm",
            ["install", "--package-lock-only"],
            work_dir,
            sample_env.command_env(Tool::Npm, bpm_store),
            sample_env,
        )],
        Tool::Pnpm => vec![build_sample_command_spec(
            "setup_lockfile",
            "pnpm",
            [
                "install".to_string(),
                "--lockfile-only".to_string(),
                "--store-dir".to_string(),
                pnpm_store_dir(bpm_store).to_string_lossy().into_owned(),
            ],
            work_dir,
            sample_env.command_env(Tool::Pnpm, bpm_store),
            sample_env,
        )],
        Tool::Bpm => vec![
            build_sample_command_spec(
                "setup_lockfile",
                "npm",
                ["install", "--package-lock-only"],
                work_dir,
                sample_env.command_env(Tool::Npm, bpm_store),
                sample_env,
            ),
            build_sample_command_spec(
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
                sample_env.command_env(Tool::Bpm, bpm_store),
                sample_env,
            ),
        ],
        // Exploratory fallback until these managers gain their own native-lock
        // setup path in the harness. Competitive scorecards use npm/pnpm/bpm.
        Tool::Yarn | Tool::Bun => vec![build_sample_command_spec(
            "setup_lockfile",
            "npm",
            ["install", "--package-lock-only"],
            work_dir,
            sample_env.command_env(Tool::Npm, bpm_store),
            sample_env,
        )],
    }
}

/// Backward-compatible command-spec helper. New benchmark samples call
/// `install_command_spec_with_env` directly.
pub fn install_command_spec(
    tool: Tool,
    work_dir: &Path,
    bpm_store: &Path,
    scenario: ScenarioKind,
    json_metrics: Option<&Path>,
    ignore_scripts: bool,
) -> CommandSpec {
    let sample_env = SampleEnvironment::new(work_dir.join(".sample-home"));
    install_command_spec_with_env(
        tool,
        work_dir,
        bpm_store,
        scenario,
        json_metrics,
        ignore_scripts,
        &sample_env,
    )
}

pub fn install_command_spec_with_env(
    tool: Tool,
    work_dir: &Path,
    bpm_store: &Path,
    scenario: ScenarioKind,
    json_metrics: Option<&Path>,
    ignore_scripts: bool,
    sample_env: &SampleEnvironment,
) -> CommandSpec {
    // Appends `--ignore-scripts` to a tool's install args when the harness is
    // running the lifecycle-parity sweep. Every manager accepts the
    // same flag, so this normalizes lifecycle handling across npm/pnpm/bpm for
    // a fair, scripts-free cold-path comparison.
    let append_ignore_scripts = |args: &mut Vec<String>| {
        if ignore_scripts {
            args.push("--ignore-scripts".to_string());
        }
    };
    match tool {
        Tool::Npm => {
            let mut args = vec!["install".to_string(), "--prefer-offline".to_string()];
            append_ignore_scripts(&mut args);
            build_sample_command_spec(
                "install",
                "npm",
                args,
                work_dir,
                sample_env.command_env(Tool::Npm, bpm_store),
                sample_env,
            )
        }
        Tool::Pnpm => {
            let mut args = vec![
                "install".to_string(),
                "--prefer-offline".to_string(),
                "--store-dir".to_string(),
                pnpm_store_dir(bpm_store).to_string_lossy().into_owned(),
            ];
            append_ignore_scripts(&mut args);
            build_sample_command_spec(
                "install",
                "pnpm",
                args,
                work_dir,
                sample_env.command_env(Tool::Pnpm, bpm_store),
                sample_env,
            )
        }
        Tool::Bpm => {
            // npm and pnpm benchmark installs both use prefer-offline; keep
            // BPM on the same cache policy so warm comparisons measure the
            // package managers rather than different metadata freshness
            // defaults. True-cold runs still have an empty per-run store.
            let mut args = vec!["install".to_string(), "--prefer-offline".to_string()];
            if scenario_uses_lockfile(scenario) {
                args.push("--frozen".to_string());
            }
            args.push("--store".to_string());
            args.push(bpm_store.to_string_lossy().into_owned());
            if let Some(path) = json_metrics {
                args.push("--json-metrics".to_string());
                args.push(path.to_string_lossy().into_owned());
            }
            append_ignore_scripts(&mut args);
            build_sample_command_spec(
                "install",
                bpm_binary(),
                args,
                work_dir,
                sample_env.command_env(Tool::Bpm, bpm_store),
                sample_env,
            )
        }
        Tool::Yarn => {
            let mut args = vec!["install".to_string()];
            append_ignore_scripts(&mut args);
            build_sample_command_spec(
                "install",
                "yarn",
                args,
                work_dir,
                sample_env.command_env(Tool::Yarn, bpm_store),
                sample_env,
            )
        }
        Tool::Bun => {
            let mut args = vec!["install".to_string(), "--no-progress".to_string()];
            append_ignore_scripts(&mut args);
            build_sample_command_spec(
                "install",
                "bun",
                args,
                work_dir,
                sample_env.command_env(Tool::Bun, bpm_store),
                sample_env,
            )
        }
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

fn create_tool_fixture_workspace(
    fixture: &Fixture,
    tool: Tool,
    work_dir: &Path,
    bpm_store: &Path,
    project_npmrc: &str,
    sample_env: &SampleEnvironment,
    expected_pnpm_version: Option<&str>,
) -> anyhow::Result<()> {
    create_fixture_workspace(fixture, work_dir)?;
    // Write after copying the fixture so a checked-in project `.npmrc` cannot
    // override the benchmark's controlled public or per-sample proxy registry.
    fs::write(work_dir.join(".npmrc"), project_npmrc)?;
    if tool == Tool::Pnpm {
        configure_pnpm_build_policy(
            fixture,
            work_dir,
            bpm_store,
            sample_env,
            expected_pnpm_version,
        )?;
    }
    Ok(())
}

/// Return whether the version resolved by a prepared fixture is the exact
/// version captured in result provenance. `None` represents a failed probe,
/// and an empty version is never accepted.
pub fn pnpm_version_matches(expected: Option<&str>, resolved: Option<&str>) -> bool {
    let (Some(expected), Some(resolved)) = (expected, resolved) else {
        return false;
    };
    let expected = expected.trim();
    let resolved = resolved.trim();
    !expected.is_empty() && expected == resolved
}

/// Build the untimed pnpm version command used while preparing a fixture's
/// build policy. It deliberately shares the exact sample environment used by
/// setup, seed, timed install, and smoke.
pub fn pnpm_build_policy_version_command_spec(
    work_dir: &Path,
    bpm_store: &Path,
    sample_env: &SampleEnvironment,
) -> CommandSpec {
    build_sample_command_spec(
        "pnpm_build_policy_version",
        "pnpm",
        ["--version"],
        work_dir,
        sample_env.command_env(Tool::Pnpm, bpm_store),
        sample_env,
    )
}

fn configure_pnpm_build_policy(
    fixture: &Fixture,
    work_dir: &Path,
    bpm_store: &Path,
    sample_env: &SampleEnvironment,
    expected_pnpm_version: Option<&str>,
) -> anyhow::Result<()> {
    // Keep this untimed probe on the same sanitized environment as setup,
    // seed, install, and smoke. It runs in the prepared fixture cwd before the
    // timed install, and never inherits the operator's pnpm policy.
    let version_command = pnpm_build_policy_version_command_spec(work_dir, bpm_store, sample_env);
    let resolved_pnpm_version = capture_version_from_spec(&version_command);
    if expected_pnpm_version.is_some()
        && !pnpm_version_matches(expected_pnpm_version, resolved_pnpm_version.as_deref())
    {
        let expected = expected_pnpm_version.unwrap_or_default();
        let resolved = resolved_pnpm_version.as_deref().unwrap_or("<unresolved>");
        anyhow::bail!(
            "pnpm version verification failed before timing: fixture={}, expected_provenance_version={}, resolved_fixture_version={}",
            fixture.name,
            expected,
            resolved
        );
    }
    let pnpm_major = resolved_pnpm_version.as_deref().and_then(|version| {
        version
            .split('.')
            .next()
            .and_then(|major| major.parse::<u32>().ok())
    });

    // pnpm 11 made dependency build approval strict and moved the setting to
    // pnpm-workspace.yaml. Keep benchmark lifecycle behavior enabled for the
    // checked-in native-build fixtures while leaving pnpm 10 baselines alone.
    if pnpm_major < Some(11) {
        return Ok(());
    }

    let allow_builds: &[&str] = match fixture.name {
        "large-frontend" => &["esbuild"],
        "native-addon" => &["node-gyp"],
        _ => &[],
    };
    if allow_builds.is_empty() {
        return Ok(());
    }

    let mut settings = String::from("allowBuilds:\n");
    for package in allow_builds {
        settings.push_str("  ");
        settings.push_str(package);
        settings.push_str(": true\n");
    }
    fs::write(work_dir.join("pnpm-workspace.yaml"), settings)?;
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
    ignore_scripts: bool,
    parity_addr: Option<SocketAddr>,
) -> anyhow::Result<ToolResults> {
    let mut runner = ProcessRunner;
    run_scenario_with_runner(
        scenario,
        fixture,
        tool,
        num_runs,
        ignore_scripts,
        parity_addr,
        &mut runner,
    )
}

/// Shape of a bpm `--json-metrics` file (only the fields the harness needs).
#[derive(Debug, Deserialize)]
struct BpmMetricsFile {
    #[serde(default)]
    phases: BTreeMap<String, f64>,
    #[serde(default)]
    counters: BTreeMap<String, serde_json::Value>,
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
    aggregate_bpm_metrics_with_counters(request_counts, phase_samples, BTreeMap::new())
}

/// Aggregate benchmark metrics while retaining numeric scalar diagnostics.
pub fn aggregate_bpm_metrics_with_counters(
    request_counts: Vec<f64>,
    phase_samples: BTreeMap<String, Vec<f64>>,
    counter_samples: BTreeMap<String, Vec<f64>>,
) -> BpmMetricsSummary {
    let phase_ms = phase_samples
        .into_iter()
        .map(|(name, samples)| (name, Stats::compute(samples)))
        .collect();
    let counters = counter_samples
        .into_iter()
        .map(|(name, samples)| (name, Stats::compute(samples)))
        .collect();
    BpmMetricsSummary {
        requests_sent: Stats::compute(request_counts),
        phase_ms,
        counters,
    }
}

struct SampleExecution {
    wall_clock_ms: f64,
    exit_code: i32,
    bpm_metrics: Option<BpmMetricsFile>,
}

#[derive(Default)]
struct ToolAccumulator {
    wall_times: Vec<f64>,
    exit_codes: Vec<i32>,
    request_counts: Vec<f64>,
    phase_samples: BTreeMap<String, Vec<f64>>,
    counter_samples: BTreeMap<String, Vec<f64>>,
    network_samples: Vec<crate::parity_proxy::NetworkShape>,
    network_records: Vec<crate::parity_proxy::NetRecord>,
}

impl ToolAccumulator {
    fn record_execution(&mut self, tool: Tool, execution: SampleExecution) {
        self.wall_times.push(execution.wall_clock_ms);
        self.exit_codes.push(execution.exit_code);
        if tool != Tool::Bpm {
            return;
        }
        if let Some(file) = execution.bpm_metrics {
            self.request_counts.push(
                file.counters
                    .get("requests_sent")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or_default(),
            );
            for (name, value) in file.counters {
                if name != "requests_sent" {
                    if let Some(value) = value.as_f64() {
                        self.counter_samples.entry(name).or_default().push(value);
                    }
                }
            }
            for (name, ms) in file.phases {
                self.phase_samples.entry(name).or_default().push(ms);
            }
        }
    }

    fn record_network(&mut self, records: Vec<crate::parity_proxy::NetRecord>) {
        self.network_samples
            .push(crate::parity_proxy::compute_network_shape(&records));
        self.network_records.extend(records);
    }

    fn finish(self, tool: Tool) -> ToolResults {
        let bpm_metrics = if tool == Tool::Bpm && !self.request_counts.is_empty() {
            Some(aggregate_bpm_metrics_with_counters(
                self.request_counts,
                self.phase_samples,
                self.counter_samples,
            ))
        } else {
            None
        };
        let network = if self.network_records.is_empty() {
            None
        } else {
            Some(crate::parity_proxy::compute_network_shape(
                &self.network_records,
            ))
        };
        ToolResults {
            tool: tool.name().to_string(),
            wall_clock_ms: Stats::compute(self.wall_times),
            exit_codes: self.exit_codes,
            bpm_metrics,
            network,
            network_samples: self.network_samples,
        }
    }
}

fn scenario_is_cold(scenario: ScenarioKind) -> bool {
    matches!(
        scenario,
        ScenarioKind::TrueCold | ScenarioKind::ResolvedCold | ScenarioKind::MonorepoCold
    )
}

/// Build the untimed fixture validation command when the copied fixture has a
/// `scripts.smoke` entry. npm only dispatches the already-installed script; the
/// command is deliberately outside the timed install interval.
pub fn fixture_smoke_command_spec(
    _fixture: &Fixture,
    tool: Tool,
    work_dir: &Path,
    bpm_store: &Path,
    sample_env: &SampleEnvironment,
) -> anyhow::Result<Option<CommandSpec>> {
    let package_json = work_dir.join("package.json");
    let bytes = fs::read(&package_json)?;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes)?;
    let has_smoke = manifest
        .get("scripts")
        .and_then(|scripts| scripts.get("smoke"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|script| !script.is_empty());
    if !has_smoke {
        return Ok(None);
    }
    Ok(Some(build_sample_command_spec(
        "fixture_smoke",
        "npm",
        ["run", "smoke", "--if-present"],
        work_dir,
        sample_env.command_env(tool, bpm_store),
        sample_env,
    )))
}

#[allow(clippy::too_many_arguments)]
fn run_one_sample(
    scenario: ScenarioKind,
    fixture: &Fixture,
    tool: Tool,
    logical_run_index: usize,
    sample_root: &Path,
    run_store: &Path,
    cache_root: &Path,
    ignore_scripts: bool,
    parity_addr: Option<SocketAddr>,
    expected_pnpm_version: Option<&str>,
    runner: &mut dyn CommandRunner,
) -> anyhow::Result<SampleExecution> {
    let work_dir = sample_root.join("project");
    let sample_env =
        SampleEnvironment::with_cache_root(sample_root.join("home"), cache_root.to_path_buf());
    fs::create_dir_all(&work_dir)?;
    sample_env.prepare(tool, run_store)?;
    let project_npmrc = parity_addr
        .map(crate::parity_proxy::parity_npmrc_content)
        .unwrap_or_else(|| crate::parity_proxy::public_registry_npmrc_content().to_string());

    prepare_scenario_with_runner(
        scenario,
        fixture,
        tool,
        &work_dir,
        run_store,
        logical_run_index + 1,
        ignore_scripts,
        &project_npmrc,
        &sample_env,
        expected_pnpm_version,
        runner,
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "sample preparation failed: tool={}, fixture={}, scenario={}, run={}, error={error}",
            tool.name(),
            fixture.name,
            scenario.name(),
            logical_run_index + 1
        )
    })?;

    let metrics_path = if tool == Tool::Bpm {
        Some(work_dir.join("bpm-timed-metrics.json"))
    } else {
        None
    };
    let timed_command = install_command_spec_with_env(
        tool,
        &work_dir,
        run_store,
        scenario,
        metrics_path.as_deref(),
        ignore_scripts,
        &sample_env,
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
            logical_run_index + 1,
            outcome.exit_code
        );
    }

    let bpm_metrics = metrics_path.as_deref().and_then(read_bpm_metrics);
    if let Some(smoke_command) =
        fixture_smoke_command_spec(fixture, tool, &work_dir, run_store, &sample_env)?
    {
        let smoke_outcome = runner.run(&smoke_command)?;
        if smoke_outcome.exit_code != 0 {
            anyhow::bail!(
                "fixture smoke validation failed: tool={}, fixture={}, scenario={}, run={}, exit_code={}",
                tool.name(),
                fixture.name,
                scenario.name(),
                logical_run_index + 1,
                smoke_outcome.exit_code
            );
        }
    }

    Ok(SampleExecution {
        wall_clock_ms: elapsed.as_secs_f64() * 1000.0,
        exit_code: outcome.exit_code,
        bpm_metrics,
    })
}

pub fn run_scenario_with_runner(
    scenario: ScenarioKind,
    fixture: &Fixture,
    tool: Tool,
    num_runs: usize,
    ignore_scripts: bool,
    parity_addr: Option<SocketAddr>,
    runner: &mut dyn CommandRunner,
) -> anyhow::Result<ToolResults> {
    if num_runs == 0 {
        anyhow::bail!("benchmark runs must be at least 1");
    }

    let temp_base = tempfile::tempdir()?;
    let shared_tool_root = temp_base.path().join("stores").join(tool.name());
    let shared_store = shared_tool_root.join("store");
    let shared_cache = shared_tool_root.join("cache");
    let mut accumulator = ToolAccumulator::default();
    for run_index in 0..num_runs {
        let sample_root = temp_base
            .path()
            .join(format!("sample-{}-{run_index}", tool.name()));
        let (run_store, cache_root) = if scenario_is_cold(scenario) {
            (
                sample_root.join("store"),
                sample_root.join("home").join("tool-cache"),
            )
        } else {
            (shared_store.clone(), shared_cache.clone())
        };
        let execution = run_one_sample(
            scenario,
            fixture,
            tool,
            run_index,
            &sample_root,
            &run_store,
            &cache_root,
            ignore_scripts,
            parity_addr,
            None,
            runner,
        )?;
        accumulator.record_execution(tool, execution);
    }
    Ok(accumulator.finish(tool))
}

#[allow(clippy::too_many_arguments)]
fn prepare_scenario_with_runner(
    scenario: ScenarioKind,
    fixture: &Fixture,
    tool: Tool,
    work_dir: &Path,
    bpm_store: &Path,
    run_number: usize,
    ignore_scripts: bool,
    project_npmrc: &str,
    sample_env: &SampleEnvironment,
    expected_pnpm_version: Option<&str>,
    runner: &mut dyn CommandRunner,
) -> anyhow::Result<()> {
    match scenario {
        ScenarioKind::TrueCold => {
            create_tool_fixture_workspace(
                fixture,
                tool,
                work_dir,
                bpm_store,
                project_npmrc,
                sample_env,
                expected_pnpm_version,
            )?;
            ensure_node_modules_empty(work_dir);
        }
        ScenarioKind::ResolvedCold => {
            create_tool_fixture_workspace(
                fixture,
                tool,
                work_dir,
                bpm_store,
                project_npmrc,
                sample_env,
                expected_pnpm_version,
            )?;
            ensure_node_modules_empty(work_dir);
            run_setup_commands(
                fixture, scenario, tool, work_dir, bpm_store, run_number, sample_env, runner,
            )?;
        }
        ScenarioKind::WarmStore => {
            create_tool_fixture_workspace(
                fixture,
                tool,
                work_dir,
                bpm_store,
                project_npmrc,
                sample_env,
                expected_pnpm_version,
            )?;
            ensure_node_modules_empty(work_dir);
            run_setup_commands(
                fixture, scenario, tool, work_dir, bpm_store, run_number, sample_env, runner,
            )?;
            seed_install(
                fixture,
                scenario,
                tool,
                work_dir,
                bpm_store,
                run_number,
                ignore_scripts,
                sample_env,
                runner,
            )?;
            clear_node_modules(work_dir);
        }
        ScenarioKind::RepeatInstall => {
            create_tool_fixture_workspace(
                fixture,
                tool,
                work_dir,
                bpm_store,
                project_npmrc,
                sample_env,
                expected_pnpm_version,
            )?;
            run_setup_commands(
                fixture, scenario, tool, work_dir, bpm_store, run_number, sample_env, runner,
            )?;
            seed_install(
                fixture,
                scenario,
                tool,
                work_dir,
                bpm_store,
                run_number,
                ignore_scripts,
                sample_env,
                runner,
            )?;
        }
        ScenarioKind::SecondProjectSameGraph => {
            create_tool_fixture_workspace(
                fixture,
                tool,
                work_dir,
                bpm_store,
                project_npmrc,
                sample_env,
                expected_pnpm_version,
            )?;
            run_setup_commands(
                fixture, scenario, tool, work_dir, bpm_store, run_number, sample_env, runner,
            )?;

            let seed = work_dir.with_file_name("seed-project");
            create_tool_fixture_workspace(
                fixture,
                tool,
                &seed,
                bpm_store,
                project_npmrc,
                sample_env,
                expected_pnpm_version,
            )?;
            run_setup_commands(
                fixture, scenario, tool, &seed, bpm_store, run_number, sample_env, runner,
            )?;
            seed_install(
                fixture,
                scenario,
                tool,
                &seed,
                bpm_store,
                run_number,
                ignore_scripts,
                sample_env,
                runner,
            )?;
            ensure_node_modules_empty(work_dir);
        }
        ScenarioKind::PartialDependencyChange => {
            create_tool_fixture_workspace(
                fixture,
                tool,
                work_dir,
                bpm_store,
                project_npmrc,
                sample_env,
                expected_pnpm_version,
            )?;
            run_setup_commands(
                fixture, scenario, tool, work_dir, bpm_store, run_number, sample_env, runner,
            )?;
            seed_install(
                fixture,
                scenario,
                tool,
                work_dir,
                bpm_store,
                run_number,
                ignore_scripts,
                sample_env,
                runner,
            )?;
            clear_node_modules(work_dir);
        }
        ScenarioKind::MonorepoCold | ScenarioKind::MonorepoIncremental => {
            create_tool_fixture_workspace(
                fixture,
                tool,
                work_dir,
                bpm_store,
                project_npmrc,
                sample_env,
                expected_pnpm_version,
            )?;
            run_setup_commands(
                fixture, scenario, tool, work_dir, bpm_store, run_number, sample_env, runner,
            )?;
            if matches!(scenario, ScenarioKind::MonorepoIncremental) {
                seed_install(
                    fixture,
                    scenario,
                    tool,
                    work_dir,
                    bpm_store,
                    run_number,
                    ignore_scripts,
                    sample_env,
                    runner,
                )?;
                clear_node_modules(work_dir);
            } else {
                ensure_node_modules_empty(work_dir);
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_setup_commands(
    fixture: &Fixture,
    scenario: ScenarioKind,
    tool: Tool,
    work_dir: &Path,
    bpm_store: &Path,
    run_number: usize,
    sample_env: &SampleEnvironment,
    runner: &mut dyn CommandRunner,
) -> anyhow::Result<()> {
    for command in lock_setup_command_specs_with_env(tool, work_dir, bpm_store, sample_env) {
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

#[allow(clippy::too_many_arguments)]
fn seed_install(
    fixture: &Fixture,
    scenario: ScenarioKind,
    tool: Tool,
    work_dir: &Path,
    bpm_store: &Path,
    run_number: usize,
    ignore_scripts: bool,
    sample_env: &SampleEnvironment,
    runner: &mut dyn CommandRunner,
) -> anyhow::Result<()> {
    let command = install_command_spec_with_env(
        tool,
        work_dir,
        bpm_store,
        scenario,
        None,
        ignore_scripts,
        sample_env,
    );
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
    /// Run every tool's install with `--ignore-scripts` (lifecycle
    /// parity sweep). Default off: the headline baseline must keep lifecycle
    /// ON to reflect real-world behavior.
    pub ignore_scripts: bool,
    /// Route every sample through the counting parity proxy and capture
    /// per-sample network shapes. Default off: the proxy normalizes transport
    /// to HTTP/1.1, so it measures network *shape*, not production wall-clock.
    pub profile_parity: bool,
}

impl RunSuiteOptions {
    pub fn new(num_runs: usize) -> Self {
        Self {
            num_runs,
            require_tools: false,
            ignore_scripts: false,
            profile_parity: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BenchSuite {
    pub results: Vec<BenchmarkResult>,
}

struct ProbeSnapshot {
    availability: BTreeMap<Tool, bool>,
    system: SystemInfo,
    versions: BTreeMap<String, String>,
}

impl ProbeSnapshot {
    /// Capture availability, host provenance, and tool versions with one
    /// private probe root. Version results are cached so detection and
    /// provenance do not execute the same `--version` command twice.
    fn capture(scenarios: &[ScenarioKind], fixture: &Fixture, tools: &[Tool]) -> Self {
        let Ok(probe) = ProbeEnvironment::new() else {
            return Self {
                availability: tools.iter().copied().map(|tool| (tool, false)).collect(),
                system: SystemInfo::capture(),
                versions: BTreeMap::new(),
            };
        };

        let mut cache = VersionProbeCache::default();
        let system = capture_system_info(Some(&probe), &mut cache);
        let availability: BTreeMap<Tool, bool> = tools
            .iter()
            .copied()
            .map(|tool| {
                let available = cache.tool_version(&probe, tool).is_some();
                (tool, available)
            })
            .collect();
        let available_tools: Vec<Tool> = tools
            .iter()
            .copied()
            .filter(|tool| availability.get(tool).copied().unwrap_or(false))
            .collect();
        let npm_invoked_by_harness = harness_invokes_npm(fixture, scenarios, &available_tools);
        let versions = collect_versions_from_cache(
            &probe,
            &mut cache,
            &available_tools,
            npm_invoked_by_harness,
        );

        Self {
            availability,
            system,
            versions,
        }
    }
}

fn validate_unique_tools(tools: &[Tool]) -> anyhow::Result<()> {
    let mut seen = BTreeSet::new();
    for tool in tools {
        if !seen.insert(*tool) {
            anyhow::bail!(
                "benchmark tool list contains duplicate tool '{}'; list each tool once",
                tool.name()
            );
        }
    }
    Ok(())
}

pub fn run_suite(
    scenarios: &[ScenarioKind],
    fixture: &Fixture,
    tools: &[Tool],
    options: &RunSuiteOptions,
) -> anyhow::Result<BenchSuite> {
    validate_unique_tools(tools)?;
    let mut runner = ProcessRunner;
    let probes = ProbeSnapshot::capture(scenarios, fixture, tools);
    run_suite_with_availability_with_runner_and_probes(
        scenarios,
        fixture,
        tools,
        options,
        |_| false,
        &mut runner,
        Some(probes),
    )
}

pub fn run_suite_with_availability<F>(
    scenarios: &[ScenarioKind],
    fixture: &Fixture,
    tools: &[Tool],
    options: &RunSuiteOptions,
    is_available: F,
) -> anyhow::Result<BenchSuite>
where
    F: FnMut(Tool) -> bool,
{
    let mut runner = ProcessRunner;
    run_suite_with_availability_with_runner(
        scenarios,
        fixture,
        tools,
        options,
        is_available,
        &mut runner,
    )
}

/// Testable production suite orchestration. Samples are executed in rotated
/// round-robin order, while each accumulator retains logical run order.
pub fn run_suite_with_availability_with_runner<F>(
    scenarios: &[ScenarioKind],
    fixture: &Fixture,
    tools: &[Tool],
    options: &RunSuiteOptions,
    is_available: F,
    runner: &mut dyn CommandRunner,
) -> anyhow::Result<BenchSuite>
where
    F: FnMut(Tool) -> bool,
{
    run_suite_with_availability_with_runner_and_probes(
        scenarios,
        fixture,
        tools,
        options,
        is_available,
        runner,
        None,
    )
}

fn run_suite_with_availability_with_runner_and_probes<F>(
    scenarios: &[ScenarioKind],
    fixture: &Fixture,
    tools: &[Tool],
    options: &RunSuiteOptions,
    mut is_available: F,
    runner: &mut dyn CommandRunner,
    probes: Option<ProbeSnapshot>,
) -> anyhow::Result<BenchSuite>
where
    F: FnMut(Tool) -> bool,
{
    validate_unique_tools(tools)?;
    let probes = probes.as_ref();
    if options.num_runs == 0 {
        anyhow::bail!("benchmark runs must be at least 1");
    }

    let mut available_tools = Vec::new();
    let mut missing_tools = Vec::new();
    for &tool in tools {
        let available = probes
            .and_then(|probes| probes.availability.get(&tool).copied())
            .unwrap_or_else(|| is_available(tool));
        if available {
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

    let npm_invoked_by_harness = harness_invokes_npm(fixture, scenarios, &available_tools);
    let (system, versions) = if let Some(probes) = probes {
        (probes.system.clone(), probes.versions.clone())
    } else {
        let system = SystemInfo::capture();
        let versions = collect_versions(&available_tools, npm_invoked_by_harness);
        (system, versions)
    };
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
        if npm_invoked_by_harness && !versions.contains_key("npm") {
            anyhow::bail!(
                "strict benchmark result is missing the npm version although the benchmark harness invokes npm"
            );
        }
    }

    // The production path captures provenance before any fixture is prepared.
    // A pnpm sample must resolve that exact version again in its real fixture
    // cwd and isolated environment; otherwise fail before setup or timing.
    // The runner-only helper intentionally leaves this optional so direct
    // pnpm installations and portable offline tests do not need a host pnpm.
    let expected_pnpm_version = if available_tools.contains(&Tool::Pnpm) {
        match versions.get("pnpm") {
            Some(version) if !version.trim().is_empty() => Some(version.as_str()),
            Some(_) | None if probes.is_some() => {
                anyhow::bail!(
                    "production benchmark is missing pnpm version provenance for an available pnpm sample"
                );
            }
            _ => None,
        }
    } else {
        None
    };

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

        // Each parity sample owns its proxy and closes that proxy before its
        // records enter the tool accumulator. There is no shared scenario log
        // that a late request can cross into or out of.
        let scenario_temp = tempfile::tempdir()?;
        let shared_stores = scenario_temp.path().join("stores");
        fs::create_dir_all(&shared_stores)?;
        let mut accumulators: BTreeMap<Tool, ToolAccumulator> = available_tools
            .iter()
            .copied()
            .map(|tool| (tool, ToolAccumulator::default()))
            .collect();
        let round_tool_order: Vec<Vec<String>> = (0..options.num_runs)
            .map(|round| {
                let shift = round % available_tools.len();
                available_tools
                    .iter()
                    .cycle()
                    .skip(shift)
                    .take(available_tools.len())
                    .map(|tool| tool.name().to_string())
                    .collect()
            })
            .collect();

        for round in 0..options.num_runs {
            let shift = round % available_tools.len();
            for offset in 0..available_tools.len() {
                let tool = available_tools[(shift + offset) % available_tools.len()];
                eprintln!(
                    "  bench {}/{} ({}, round {}) ...",
                    fixture.name,
                    tool.name(),
                    scenario.name(),
                    round + 1
                );
                let sample_root = scenario_temp
                    .path()
                    .join(format!("sample-{}-{round}", tool.name()));
                let shared_tool_root = shared_stores.join(tool.name());
                let (run_store, cache_root) = if scenario_is_cold(scenario) {
                    (
                        sample_root.join("store"),
                        sample_root.join("home").join("tool-cache"),
                    )
                } else {
                    (
                        shared_tool_root.join("store"),
                        shared_tool_root.join("cache"),
                    )
                };
                let parity_proxy = if options.profile_parity {
                    match crate::parity_proxy::ParityProxy::start() {
                        Ok(proxy) => {
                            eprintln!(
                                "  parity proxy on {} (HTTP/1.1; measures network shape, not production timing)",
                                proxy.addr()
                            );
                            Some(proxy)
                        }
                        Err(error) => {
                            anyhow::bail!("failed to start parity proxy: {error}");
                        }
                    }
                } else {
                    None
                };
                let parity_addr = parity_proxy.as_ref().map(|proxy| proxy.addr());
                let execution = run_one_sample(
                    scenario,
                    fixture,
                    tool,
                    round,
                    &sample_root,
                    &run_store,
                    &cache_root,
                    options.ignore_scripts,
                    parity_addr,
                    expected_pnpm_version,
                    runner,
                )?;
                let network_records = if let Some(proxy) = parity_proxy {
                    Some(proxy.finish().map_err(|error| {
                        anyhow::anyhow!(
                            "parity sample drain failed: fixture={}, scenario={}, tool={}, run={}, error={error}",
                            fixture.name,
                            scenario.name(),
                            tool.name(),
                            round + 1
                        )
                    })?)
                } else {
                    None
                };
                let accumulator = accumulators
                    .get_mut(&tool)
                    .expect("available tool has an accumulator");
                accumulator.record_execution(tool, execution);
                if let Some(records) = network_records {
                    accumulator.record_network(records);
                }
            }
        }

        let tool_results = available_tools
            .iter()
            .map(|tool| {
                accumulators
                    .remove(tool)
                    .expect("available tool has an accumulator")
                    .finish(*tool)
            })
            .collect();
        let result = BenchmarkResult {
            scenario: scenario.name().to_string(),
            fixture: fixture.name.to_string(),
            system: system.clone(),
            versions: versions.clone(),
            cache_state: cache_state.to_string(),
            number_of_runs: options.num_runs,
            protocol: Some(BenchmarkProtocol::current(
                options.ignore_scripts,
                options.profile_parity,
                round_tool_order,
            )),
            tools: tool_results,
        };
        validate_result(&result)?;
        if options.require_tools {
            validate_strict_result(&result, &available_tools)?;
            if options.profile_parity && scenario == ScenarioKind::TrueCold && options.num_runs > 1
            {
                for tool in &result.tools {
                    validate_cold_network_samples(
                        &tool.tool,
                        &tool.network_samples,
                        result.number_of_runs,
                    )?;
                }
            }
        }
        results.push(result);
    }

    Ok(BenchSuite { results })
}

fn collect_versions(tools: &[Tool], npm_invoked_by_harness: bool) -> BTreeMap<String, String> {
    let Ok(probe) = ProbeEnvironment::new() else {
        return BTreeMap::new();
    };
    let mut cache = VersionProbeCache::default();
    collect_versions_from_cache(&probe, &mut cache, tools, npm_invoked_by_harness)
}

fn collect_versions_from_cache(
    probe: &ProbeEnvironment,
    cache: &mut VersionProbeCache,
    tools: &[Tool],
    npm_invoked_by_harness: bool,
) -> BTreeMap<String, String> {
    let mut versions = BTreeMap::new();
    if let Some(v) = cache.node_version(probe) {
        versions.insert("node".into(), v);
    }
    // npm is both a scored tool and a harness dependency: BPM lock setup and
    // fixture smoke validation invoke it even when npm is absent from the
    // requested scorecard. Capture that provenance before any timed sample.
    if npm_invoked_by_harness || tools.contains(&Tool::Npm) {
        if let Some(v) = cache.tool_version(probe, Tool::Npm) {
            versions.insert("npm".into(), v);
        }
    }
    for &tool in tools {
        if let Some(v) = cache.tool_version(probe, tool) {
            versions.insert(tool.name().into(), v);
        }
    }
    versions
}

fn fixture_has_smoke_script(fixture: &Fixture) -> bool {
    let package_json = fixture_dir(fixture.name).join("package.json");
    let Ok(bytes) = fs::read(package_json) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    manifest
        .get("scripts")
        .and_then(|scripts| scripts.get("smoke"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|script| !script.is_empty())
}

fn harness_invokes_npm(fixture: &Fixture, scenarios: &[ScenarioKind], tools: &[Tool]) -> bool {
    if tools.is_empty() {
        return false;
    }
    let smoke_validation = fixture_has_smoke_script(fixture);
    let npm_lock_setup = scenarios
        .iter()
        .any(|scenario| scenario_uses_lockfile(*scenario))
        && tools
            .iter()
            .any(|tool| matches!(tool, Tool::Npm | Tool::Bpm | Tool::Yarn | Tool::Bun));
    smoke_validation || npm_lock_setup
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
        if tool.wall_clock_ms.values.len() != result.number_of_runs {
            anyhow::bail!(
                "result invariant failed for {}/{}/{}: expected {} wall-clock samples, found {}",
                result.fixture,
                result.scenario,
                tool.tool,
                result.number_of_runs,
                tool.wall_clock_ms.values.len()
            );
        }
        if let Some((run, value)) = tool
            .wall_clock_ms
            .values
            .iter()
            .enumerate()
            .find(|(_, value)| !value.is_finite() || **value <= 0.0)
        {
            anyhow::bail!(
                "result invariant failed for {}/{}/{}: invalid wall-clock sample at run {}: {}",
                result.fixture,
                result.scenario,
                tool.tool,
                run + 1,
                value
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
        if !tool.network_samples.is_empty() && tool.network_samples.len() != result.number_of_runs {
            anyhow::bail!(
                "result invariant failed for {}/{}/{}: expected {} network samples, found {}",
                result.fixture,
                result.scenario,
                tool.tool,
                result.number_of_runs,
                tool.network_samples.len()
            );
        }
    }
    Ok(())
}

/// Validate the per-sample parity records for a strict true-cold cell. This
/// is a cache-leak tripwire, not a performance ranking.
pub fn validate_cold_network_samples(
    tool: &str,
    samples: &[crate::parity_proxy::NetworkShape],
    expected_runs: usize,
) -> anyhow::Result<()> {
    if samples.len() != expected_runs {
        anyhow::bail!(
            "cold parity validation failed: tool={}, expected {} network samples, found {}",
            tool,
            expected_runs,
            samples.len()
        );
    }
    for (run, sample) in samples.iter().enumerate() {
        if sample.request_count == 0 {
            anyhow::bail!(
                "cold parity validation failed: tool={}, run={}, zero requests",
                tool,
                run + 1
            );
        }
    }
    let median = Stats::compute(
        samples
            .iter()
            .map(|sample| sample.request_count as f64)
            .collect(),
    )
    .median;
    let minimum = median * 0.9;
    for (run, sample) in samples.iter().enumerate() {
        if (sample.request_count as f64) < minimum {
            anyhow::bail!(
                "cold parity validation failed: tool={}, run={}, request_count={}, median_request_count={:.3}, minimum_allowed={:.3}",
                tool,
                run + 1,
                sample.request_count,
                median,
                minimum
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

#[derive(Debug, Clone)]
pub struct CrossToolGateOptions {
    pub target: Tool,
    pub max_median_ratio: f64,
    pub max_p95_ratio: f64,
    pub require_tools: bool,
    pub runs: usize,
    pub ignore_scripts: bool,
    pub profile_parity: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CrossToolComparisonRow {
    pub fixture: String,
    pub scenario: String,
    pub target_tool: String,
    pub bpm_median_ms: f64,
    pub bpm_p95_ms: f64,
    pub target_median_ms: f64,
    pub target_p95_ms: f64,
    pub paired_median_ratio: f64,
    pub paired_p95_ratio: f64,
    pub protocol_version: u32,
    pub versions: BTreeMap<String, String>,
}

fn validate_current_protocol(
    protocol: &BenchmarkProtocol,
    result: &BenchmarkResult,
    number_of_runs: usize,
) -> anyhow::Result<()> {
    if protocol.protocol_version != BENCHMARK_PROTOCOL_VERSION {
        anyhow::bail!(
            "benchmark protocol is legacy or unsupported: version={}, expected={}",
            protocol.protocol_version,
            BENCHMARK_PROTOCOL_VERSION
        );
    }
    if protocol.cache_isolation_mode != "per-sample-home-v1"
        || protocol.execution_mode != "round-robin-rotated-v1"
        || protocol.post_install_validation != "fixture-smoke-v1"
        || !matches!(
            protocol.lifecycle_policy.as_str(),
            "scripts-on" | "scripts-off"
        )
    {
        anyhow::bail!("benchmark protocol fields are not current");
    }
    if protocol.round_tool_order.len() != number_of_runs {
        anyhow::bail!(
            "benchmark protocol has {} round orders, expected {}",
            protocol.round_tool_order.len(),
            number_of_runs
        );
    }
    let mut result_tools = BTreeSet::new();
    for tool in &result.tools {
        if !result_tools.insert(tool.tool.clone()) {
            anyhow::bail!(
                "benchmark result has duplicate ToolResults for fixture={}, scenario={}, tool={}",
                result.fixture,
                result.scenario,
                tool.tool
            );
        }
    }

    let Some(first_order) = protocol.round_tool_order.first() else {
        anyhow::bail!("benchmark protocol has no round tool order");
    };
    if first_order.is_empty() {
        anyhow::bail!("benchmark protocol has an empty tool order");
    }
    let first_tools: BTreeSet<String> = first_order.iter().cloned().collect();
    if first_tools.len() != first_order.len() {
        anyhow::bail!("benchmark protocol first round contains duplicate tool entries");
    }
    if first_tools != result_tools {
        anyhow::bail!(
            "benchmark protocol first round tool membership does not exactly match result tools for fixture={}, scenario={}",
            result.fixture,
            result.scenario
        );
    }

    for (round, order) in protocol.round_tool_order.iter().enumerate() {
        let order_tools: BTreeSet<String> = order.iter().cloned().collect();
        if order_tools.len() != order.len() {
            anyhow::bail!(
                "benchmark protocol round {} contains duplicate tool entries",
                round + 1
            );
        }
        if order_tools != result_tools {
            anyhow::bail!(
                "benchmark protocol round {} tool membership does not exactly match result tools",
                round + 1
            );
        }
        let shift = round % first_order.len();
        let expected: Vec<&String> = first_order
            .iter()
            .cycle()
            .skip(shift)
            .take(first_order.len())
            .collect();
        if order
            .iter()
            .zip(expected.iter())
            .any(|(actual, expected)| actual != *expected)
        {
            anyhow::bail!(
                "benchmark protocol round order is not a deterministic rotation at round {}",
                round + 1
            );
        }
    }
    Ok(())
}

fn validate_gate_tool_samples(
    result: &BenchmarkResult,
    tool: &ToolResults,
    runs: usize,
) -> anyhow::Result<()> {
    if tool.exit_codes.len() != runs || tool.wall_clock_ms.values.len() != runs {
        anyhow::bail!(
            "cross-tool gate requires {} successful samples for fixture={}, scenario={}, tool={}",
            runs,
            result.fixture,
            result.scenario,
            tool.tool
        );
    }
    if let Some((run, code)) = tool
        .exit_codes
        .iter()
        .enumerate()
        .find(|(_, code)| **code != 0)
    {
        anyhow::bail!(
            "cross-tool gate found nonzero exit code for fixture={}, scenario={}, tool={}, run={}, exit_code={}",
            result.fixture,
            result.scenario,
            tool.tool,
            run + 1,
            code
        );
    }
    if let Some((run, value)) = tool
        .wall_clock_ms
        .values
        .iter()
        .enumerate()
        .find(|(_, value)| !value.is_finite() || **value <= 0.0)
    {
        anyhow::bail!(
            "cross-tool gate found invalid wall-clock sample for fixture={}, scenario={}, tool={}, run={}, value={}",
            result.fixture,
            result.scenario,
            tool.tool,
            run + 1,
            value
        );
    }
    Ok(())
}

/// Compare BPM and a target tool using paired logical rounds from one result.
/// This function has no process or filesystem dependencies and is the pure
/// analysis boundary used by the CLI gate and its deterministic tests.
pub fn compare_results_against_tool(
    results: &[BenchmarkResult],
    options: &CrossToolGateOptions,
) -> anyhow::Result<Vec<CrossToolComparisonRow>> {
    if !options.require_tools {
        anyhow::bail!("cross-tool gate requires --require-tools");
    }
    if options.runs < 7 {
        anyhow::bail!("cross-tool gate requires --runs >= 7");
    }
    if !options.ignore_scripts {
        anyhow::bail!("cross-tool gate requires explicit --ignore-scripts");
    }
    if options.profile_parity {
        anyhow::bail!("cross-tool gate cannot run with --profile-parity");
    }
    if !(options.max_median_ratio.is_finite() && options.max_median_ratio > 0.0) {
        anyhow::bail!("--max-median-ratio must be a positive finite number");
    }
    if !(options.max_p95_ratio.is_finite() && options.max_p95_ratio > 0.0) {
        anyhow::bail!("--max-p95-ratio must be a positive finite number");
    }
    if options.target == Tool::Bpm {
        anyhow::bail!("--require-faster-than cannot target bpm");
    }
    if results.is_empty() {
        anyhow::bail!("cross-tool gate has no benchmark result cells");
    }

    let mut protocol: Option<BenchmarkProtocol> = None;
    let mut rows = Vec::with_capacity(results.len());
    for result in results {
        if result.number_of_runs != options.runs {
            anyhow::bail!(
                "cross-tool gate requires {} runs for fixture={}, scenario={}, found {}",
                options.runs,
                result.fixture,
                result.scenario,
                result.number_of_runs
            );
        }
        let current_protocol = result.protocol.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "cross-tool gate rejects protocol-less result for fixture={}, scenario={}",
                result.fixture,
                result.scenario
            )
        })?;
        for required_tool in [Tool::Bpm, options.target] {
            if !result
                .tools
                .iter()
                .any(|tool| tool.tool == required_tool.name())
            {
                anyhow::bail!(
                    "cross-tool gate missing tool {} for fixture={}, scenario={}",
                    required_tool.name(),
                    result.fixture,
                    result.scenario
                );
            }
        }
        validate_current_protocol(&current_protocol, result, result.number_of_runs)?;
        if current_protocol.lifecycle_policy != "scripts-off" {
            anyhow::bail!(
                "cross-tool gate requires scripts-off protocol for fixture={}, scenario={}",
                result.fixture,
                result.scenario
            );
        }
        if current_protocol.profile_parity {
            anyhow::bail!(
                "cross-tool gate rejects parity-proxy wall timing for fixture={}, scenario={}",
                result.fixture,
                result.scenario
            );
        }
        if let Some(previous) = &protocol {
            if previous != &current_protocol {
                anyhow::bail!(
                    "cross-tool gate protocol mismatch for fixture={}, scenario={}",
                    result.fixture,
                    result.scenario
                );
            }
        } else {
            protocol = Some(current_protocol.clone());
        }

        let bpm = result
            .tools
            .iter()
            .find(|tool| tool.tool == Tool::Bpm.name());
        let target = result
            .tools
            .iter()
            .find(|tool| tool.tool == options.target.name());
        let Some(bpm) = bpm else {
            anyhow::bail!(
                "cross-tool gate missing tool bpm for fixture={}, scenario={}",
                result.fixture,
                result.scenario
            );
        };
        let Some(target) = target else {
            anyhow::bail!(
                "cross-tool gate missing tool {} for fixture={}, scenario={}",
                options.target.name(),
                result.fixture,
                result.scenario
            );
        };
        validate_gate_tool_samples(result, bpm, options.runs)?;
        validate_gate_tool_samples(result, target, options.runs)?;
        for tool_name in [Tool::Bpm.name(), options.target.name()] {
            if !result.versions.contains_key(tool_name) {
                anyhow::bail!(
                    "cross-tool gate result is missing version for tool {} for fixture={}, scenario={}",
                    tool_name,
                    result.fixture,
                    result.scenario
                );
            }
        }

        let ratios: Vec<f64> = bpm
            .wall_clock_ms
            .values
            .iter()
            .zip(&target.wall_clock_ms.values)
            .enumerate()
            .map(|(run, (bpm_ms, target_ms))| {
                let ratio = *bpm_ms / *target_ms;
                if !ratio.is_finite() || ratio <= 0.0 {
                    return Err(anyhow::anyhow!(
                        "cross-tool gate produced invalid paired ratio for fixture={}, scenario={}, run={}, bpm_ms={}, target_ms={}",
                        result.fixture,
                        result.scenario,
                        run + 1,
                        bpm_ms,
                        target_ms
                    ));
                }
                Ok(ratio)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let ratio_stats = Stats::compute(ratios);
        let protocol_version = protocol
            .as_ref()
            .expect("protocol set before comparing a result")
            .protocol_version;
        let versions = result.versions.clone();
        let row = CrossToolComparisonRow {
            fixture: result.fixture.clone(),
            scenario: result.scenario.clone(),
            target_tool: options.target.name().to_string(),
            bpm_median_ms: bpm.wall_clock_ms.median,
            bpm_p95_ms: bpm.wall_clock_ms.p95,
            target_median_ms: target.wall_clock_ms.median,
            target_p95_ms: target.wall_clock_ms.p95,
            paired_median_ratio: ratio_stats.median,
            paired_p95_ratio: ratio_stats.p95,
            protocol_version,
            versions,
        };
        if row.paired_median_ratio > options.max_median_ratio
            || row.paired_p95_ratio > options.max_p95_ratio
        {
            anyhow::bail!(
                "cross-tool gate failed: fixture={} scenario={} bpm_median={:.3}ms target={} target_median={:.3}ms bpm_p95={:.3}ms target_p95={:.3}ms paired_median_ratio={:.6} paired_p95_ratio={:.6} protocol_version={} versions={:?} max_median_ratio={:.6} max_p95_ratio={:.6}",
                row.fixture,
                row.scenario,
                row.bpm_median_ms,
                row.target_tool,
                row.target_median_ms,
                row.bpm_p95_ms,
                row.target_p95_ms,
                row.paired_median_ratio,
                row.paired_p95_ratio,
                row.protocol_version,
                row.versions,
                options.max_median_ratio,
                options.max_p95_ratio
            );
        }
        rows.push(row);
    }
    Ok(rows)
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
        // BPM version may differ between baseline and current (comparing two
        // BPM builds is the gate's purpose); every other host/runtime version
        // must still match in strict mode.
        let comparable = environments_comparable(baseline_entry.result, current_entry.result);
        if (!system_matches || !versions_match) && !comparable && !options.informational {
            anyhow::bail!(
                "baseline comparison requires a matching machine/system and matching non-bpm runtime versions for fixture={}, scenario={}, tool={}; baseline_machine={} current_machine={} baseline_runtime={:?} current_runtime={:?} baseline_versions={:?} current_versions={:?}",
                key.fixture,
                key.scenario,
                key.tool,
                baseline_entry.result.system.machine,
                current_entry.result.system.machine,
                baseline_entry.result.system.runtime_versions,
                current_entry.result.system.runtime_versions,
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

        if ratio > options.regression_envelope && !options.informational {
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

/// Whether two results share a comparable environment, ignoring only the BPM
/// version (the intentional subject of a regression comparison). Compares
/// `machine`, `operating_system`, `kernel`, and every recorded runtime/result
/// version except `bpm`. Does not mutate the maps.
fn environments_comparable(baseline: &BenchmarkResult, current: &BenchmarkResult) -> bool {
    let (bs, cs) = (&baseline.system, &current.system);
    if bs.machine != cs.machine
        || bs.operating_system != cs.operating_system
        || bs.kernel != cs.kernel
    {
        return false;
    }
    if !maps_equal_excluding_bpm(&bs.runtime_versions, &cs.runtime_versions) {
        return false;
    }
    if !maps_equal_excluding_bpm(&baseline.versions, &current.versions) {
        return false;
    }
    true
}

/// Compare two version maps while ignoring the `bpm` key in both, without
/// mutating either map.
fn maps_equal_excluding_bpm(a: &BTreeMap<String, String>, b: &BTreeMap<String, String>) -> bool {
    a.iter()
        .filter(|(k, _)| k.as_str() != "bpm")
        .eq(b.iter().filter(|(k, _)| k.as_str() != "bpm"))
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
    let sample_root = temp_base.path().join("profile-sample");
    let work_dir = sample_root.join("project");
    let run_store = sample_root.join("store");
    let sample_env = SampleEnvironment::new(sample_root.join("home"));
    fs::create_dir_all(&work_dir)?;
    sample_env.prepare(Tool::Bpm, &run_store)?;
    let project_npmrc = crate::parity_proxy::public_registry_npmrc_content();

    prepare_scenario_with_runner(
        scenario,
        fixture,
        Tool::Bpm,
        &work_dir,
        &run_store,
        1,
        false,
        project_npmrc,
        &sample_env,
        None,
        runner,
    )?;
    let command = install_command_spec_with_env(
        Tool::Bpm,
        &work_dir,
        &run_store,
        scenario,
        Some(metrics_path),
        false,
        &sample_env,
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
    if let Some(smoke_command) =
        fixture_smoke_command_spec(fixture, Tool::Bpm, &work_dir, &run_store, &sample_env)?
    {
        let smoke_outcome = runner.run(&smoke_command)?;
        if smoke_outcome.exit_code != 0 {
            anyhow::bail!(
                "fixture smoke validation failed: tool=bpm, fixture={}, scenario={}, run=1, exit_code={}",
                fixture.name,
                scenario.name(),
                smoke_outcome.exit_code
            );
        }
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
                    phases.sort_by(|a, b| b.1.median.total_cmp(&a.1.median));
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

    /// `Stats::compute` must be total/panic-free even if a timing sample is
    /// malformed (`NaN` or `±inf`). The bench harness previously sorted with
    /// `partial_cmp(b).unwrap()`, which would panic on any such sample instead
    /// of reporting results. `total_cmp` makes the sort total.
    #[test]
    fn stats_compute_is_total_for_nan_and_inf_samples() {
        // Must not panic.
        let _ = Stats::compute(vec![1.0, f64::NAN, 2.0]);
        let _ = Stats::compute(vec![f64::NAN, f64::NAN]);
        let _ = Stats::compute(vec![1.0, f64::INFINITY, 2.0]);
        let _ = Stats::compute(vec![1.0, f64::NEG_INFINITY, 2.0]);
        let _ = Stats::compute(vec![f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1.0]);
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
        let locked = install_command_spec(
            Tool::Bpm,
            work_dir,
            store,
            ScenarioKind::ResolvedCold,
            None,
            false,
        );
        assert!(locked.args.contains(&"--frozen".to_string()));

        let cold = install_command_spec(
            Tool::Bpm,
            work_dir,
            store,
            ScenarioKind::TrueCold,
            None,
            false,
        );
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
    fn aggregate_bpm_metrics_keeps_scalar_counters_separate_from_phases() {
        let counters = BTreeMap::from([(
            "resolver_peak_http_concurrency".to_string(),
            vec![4.0, 8.0, 8.0],
        )]);
        let summary =
            aggregate_bpm_metrics_with_counters(vec![3.0, 3.0, 3.0], BTreeMap::new(), counters);
        assert_eq!(
            summary
                .counters
                .get("resolver_peak_http_concurrency")
                .unwrap()
                .median,
            8.0
        );
        assert!(summary.phase_ms.is_empty());
    }

    #[test]
    fn hostile_package_manager_config_names_are_removed_without_copying_values() {
        let removals = package_manager_config_names([
            OsString::from("NPM_CONFIG_USERCONFIG"),
            OsString::from("nPm_CoNfIg_ReGiStRy"),
            OsString::from("NPM_CONFIG_GLOBALCONFIG"),
            OsString::from("NODE_AUTH_TOKEN"),
            OsString::from("NPM_TOKEN"),
            OsString::from("PNPM_HOME"),
            OsString::from("YARN_NPM_AUTH_TOKEN"),
            OsString::from("BUN_INSTALL_CACHE_DIR"),
            OsString::from("BPM_REGISTRY"),
            OsString::from("PATH"),
            OsString::from("HTTPS_PROXY"),
            OsString::from("DYLD_LIBRARY_PATH"),
        ]);

        for name in [
            "NPM_CONFIG_USERCONFIG",
            "nPm_CoNfIg_ReGiStRy",
            "NPM_CONFIG_GLOBALCONFIG",
            "NODE_AUTH_TOKEN",
            "NPM_TOKEN",
            "PNPM_HOME",
            "YARN_NPM_AUTH_TOKEN",
            "BUN_INSTALL_CACHE_DIR",
            "BPM_REGISTRY",
        ] {
            assert!(
                removals.contains(&OsString::from(name)),
                "hostile package-manager variable was not removed: {name}"
            );
        }
        for name in ["PATH", "HTTPS_PROXY", "DYLD_LIBRARY_PATH"] {
            assert!(
                !removals.contains(&OsString::from(name)),
                "process requirement was incorrectly removed: {name}"
            );
        }

        let spec = install_command_spec(
            Tool::Npm,
            Path::new("/tmp/work"),
            Path::new("/tmp/store"),
            ScenarioKind::TrueCold,
            None,
            false,
        );
        assert!(spec
            .env_remove
            .contains(&OsString::from("NPM_CONFIG_GLOBALCONFIG")));
        assert!(spec.env_remove.contains(&OsString::from("NPM_CONFIG_AUTH")));
        assert!(spec.env.contains_key("npm_config_userconfig"));
        assert!(spec.env.contains_key("npm_config_globalconfig"));
    }

    #[test]
    fn probe_environment_prepares_private_config_and_registry() {
        let probe = ProbeEnvironment::new().unwrap();
        assert_eq!(fs::read_to_string(probe.sample_env.npmrc()).unwrap(), "");
        assert_eq!(
            fs::read_to_string(probe.sample_env.global_npmrc()).unwrap(),
            ""
        );
        assert_eq!(
            fs::read_to_string(probe.work_dir.join(".npmrc")).unwrap(),
            crate::parity_proxy::public_registry_npmrc_content()
        );

        let command = probe.command_spec(Tool::Pnpm);
        assert_eq!(command.program, PathBuf::from("pnpm"));
        assert_eq!(command.current_dir, probe.work_dir);
        assert_eq!(
            command.env["npm_config_registry"],
            "https://registry.npmjs.org/"
        );
        assert!(Path::new(&command.env["HOME"]).starts_with(probe._temp_root.path()));
        assert!(Path::new(&command.env["npm_config_cache"]).starts_with(probe._temp_root.path()));
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
                counters: BTreeMap::new(),
            }),
            network: None,
            network_samples: Vec::new(),
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
            network: None,
            network_samples: Vec::new(),
        };
        assert!(!serde_json::to_string(&none)
            .unwrap()
            .contains("bpm_metrics"));
    }
}
