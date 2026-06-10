//! Deterministic, offline tests for benchmark harness plumbing.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use bpm::bench::{
    bpm_profile_filename, compare_results_against_baseline, install_command_spec,
    lock_setup_command_specs, run_scenario_with_runner, run_suite_with_availability,
    write_bpm_profile_manifest, BenchmarkResult, BpmProfileEntry, BpmProfileManifest,
    CommandOutcome, CommandRunner, CommandSpec, CompareOptions, RunSuiteOptions, ScenarioKind,
    Stats, SystemInfo, Tool, FIXTURES,
};

#[derive(Default)]
struct RecordingRunner {
    commands: Vec<CommandSpec>,
    exit_codes: VecDeque<i32>,
}

impl RecordingRunner {
    fn with_exit_codes(exit_codes: impl IntoIterator<Item = i32>) -> Self {
        Self {
            commands: Vec::new(),
            exit_codes: exit_codes.into_iter().collect(),
        }
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&mut self, command: &CommandSpec) -> anyhow::Result<CommandOutcome> {
        self.commands.push(command.clone());
        Ok(CommandOutcome {
            exit_code: self.exit_codes.pop_front().unwrap_or(0),
        })
    }
}

#[test]
fn fixtures_have_meaningful_packages() {
    assert!(FIXTURES.len() >= 2);
    for f in FIXTURES {
        assert!(!f.name.is_empty());
        assert!(!f.packages.is_empty());
        for p in f.packages {
            assert!(
                p.contains('@'),
                "fixture {} has unpinned package {p}",
                f.name
            );
        }
    }
}

#[test]
fn scenario_kinds_are_stable_and_unique() {
    let all = ScenarioKind::all();
    let names: Vec<&str> = all.iter().map(|s| s.name()).collect();
    assert_eq!(names.len(), 8);
    let set: BTreeSet<&str> = names.iter().copied().collect();
    assert_eq!(set.len(), 8, "scenario names must be unique");
    assert!(names.contains(&"repeat_install"));
    assert!(names.contains(&"monorepo_incremental"));
}

#[test]
fn stats_p95_is_deterministic_for_same_input() {
    let v1: Vec<f64> = (0..200).map(|i| i as f64).collect();
    let v2: Vec<f64> = v1.iter().rev().cloned().collect();
    let a = Stats::compute(v1);
    let b = Stats::compute(v2);
    assert!((a.median - b.median).abs() < f64::EPSILON);
    assert!((a.p95 - b.p95).abs() < f64::EPSILON);
    assert!((a.stddev - b.stddev).abs() < f64::EPSILON);
}

#[test]
fn benchmark_result_serializes_with_versions() {
    let mut versions = BTreeMap::new();
    versions.insert("node".to_string(), "v26.0.0".to_string());
    versions.insert("npm".to_string(), "11.12.1".to_string());
    let result = BenchmarkResult {
        scenario: "resolved_cold".into(),
        fixture: "minimal".into(),
        system: sample_system(),
        versions,
        cache_state: "cold".into(),
        number_of_runs: 3,
        tools: vec![],
    };
    let json = serde_json::to_string(&result).unwrap();
    assert!(
        json.contains("\"versions\""),
        "versions field missing: {json}"
    );
    assert!(
        json.contains("\"npm\":\"11.12.1\""),
        "pinned npm version missing: {json}"
    );
    let back: BenchmarkResult = serde_json::from_str(&json).unwrap();
    assert_eq!(
        back.versions.get("npm").map(|s| s.as_str()),
        Some("11.12.1")
    );
    assert_eq!(back.scenario, "resolved_cold");
}

#[test]
fn tools_include_bpm_after_milestone_2() {
    let names: Vec<&str> = Tool::all().iter().map(|c| c.name()).collect();
    assert!(names.contains(&"bpm"), "bpm tool missing: {names:?}");
    assert!(names.contains(&"npm"));
    assert!(names.contains(&"yarn"));
    assert!(names.contains(&"bun"));
}

#[test]
fn npm_and_pnpm_lock_setup_use_native_commands() {
    let work_dir = Path::new("/tmp/work");
    let store_dir = Path::new("/tmp/store");

    let npm = lock_setup_command_specs(Tool::Npm, work_dir, store_dir);
    assert_eq!(npm.len(), 1);
    assert_eq!(npm[0].program, PathBuf::from("npm"));
    assert_eq!(npm[0].args, ["install", "--package-lock-only"]);

    let pnpm = lock_setup_command_specs(Tool::Pnpm, work_dir, store_dir);
    assert_eq!(pnpm.len(), 1);
    assert_eq!(pnpm[0].program, PathBuf::from("pnpm"));
    assert_eq!(
        pnpm[0].args,
        [
            "install",
            "--lockfile-only",
            "--store-dir",
            "/tmp/store/pnpm-cache/store",
        ]
    );
    assert_eq!(
        pnpm[0].env.get("npm_config_cache"),
        Some(
            &store_dir
                .join("pnpm-cache/npm-cache")
                .to_string_lossy()
                .into_owned()
        )
    );
}

#[test]
fn bpm_lock_setup_imports_before_timed_install() {
    let fixture = fixture("minimal");
    let mut runner = RecordingRunner::with_exit_codes([0, 0, 0]);

    let result = run_scenario_with_runner(
        ScenarioKind::ResolvedCold,
        fixture,
        Tool::Bpm,
        1,
        &mut runner,
    )
    .unwrap();

    assert_eq!(result.exit_codes, [0]);
    assert_eq!(runner.commands.len(), 3);
    assert_eq!(runner.commands[0].program, PathBuf::from("npm"));
    assert_eq!(runner.commands[0].args, ["install", "--package-lock-only"]);
    assert!(runner.commands[1].args.windows(2).any(|window| window
        == [
            "--out",
            runner.commands[1]
                .current_dir
                .join("bpm.lock")
                .to_string_lossy()
                .as_ref()
        ]));
    assert_eq!(runner.commands[2].label, "install");
    assert_eq!(runner.commands[2].args[0], "install");
    assert!(runner.commands[2].args.contains(&"--frozen".to_string()));
    assert!(!runner.commands[2].args.iter().any(|arg| arg == "import"));
}

#[test]
fn true_cold_has_no_lock_setup_commands() {
    let fixture = fixture("minimal");
    let mut runner = RecordingRunner::with_exit_codes([0]);

    run_scenario_with_runner(ScenarioKind::TrueCold, fixture, Tool::Bpm, 1, &mut runner).unwrap();

    assert_eq!(runner.commands.len(), 1);
    assert_eq!(runner.commands[0].label, "install");
    assert!(!runner.commands[0]
        .args
        .iter()
        .any(|arg| arg.contains("lock") || arg == "import"));
}

#[test]
fn nonzero_timed_exit_invalidates_the_scenario() {
    let fixture = fixture("minimal");
    let mut runner = RecordingRunner::with_exit_codes([17]);

    let error =
        run_scenario_with_runner(ScenarioKind::TrueCold, fixture, Tool::Npm, 1, &mut runner)
            .unwrap_err();

    let message = format!("{error:#}");
    assert!(message.contains("timed benchmark failed"));
    assert!(message.contains("tool=npm"));
    assert!(message.contains("scenario=true_cold"));
    assert!(message.contains("exit_code=17"));
}

#[test]
fn run_suite_rejects_zero_runs_at_public_boundary() {
    let error = run_suite_with_availability(
        &[],
        fixture("minimal"),
        &[Tool::Bpm],
        &RunSuiteOptions {
            num_runs: 0,
            require_tools: false,
        },
        |_| true,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("at least 1"));
}

#[test]
fn run_suite_strict_missing_tools_reports_all_missing_names() {
    let error = run_suite_with_availability(
        &[],
        fixture("minimal"),
        &[Tool::Npm, Tool::Pnpm, Tool::Bpm],
        &RunSuiteOptions {
            num_runs: 1,
            require_tools: true,
        },
        |_| false,
    )
    .unwrap_err();

    let message = format!("{error:#}");
    assert!(message.contains("npm"));
    assert!(message.contains("pnpm"));
    assert!(message.contains("bpm"));
}

#[test]
fn run_suite_permissive_missing_tools_still_allows_empty_work_when_one_tool_is_available() {
    let suite = run_suite_with_availability(
        &[],
        fixture("minimal"),
        &[Tool::Bpm, Tool::Pnpm],
        &RunSuiteOptions {
            num_runs: 1,
            require_tools: false,
        },
        |tool| matches!(tool, Tool::Bpm),
    )
    .unwrap();

    assert!(suite.results.is_empty());
}

#[test]
fn semantic_baseline_lookup_ignores_array_order() {
    let baseline = vec![result_with_tools(
        "minimal",
        "repeat_install",
        vec![("bpm", 10.0, vec![0]), ("npm", 20.0, vec![0])],
    )];
    let current = vec![result_with_tools(
        "minimal",
        "repeat_install",
        vec![("npm", 18.0, vec![0]), ("bpm", 9.0, vec![0])],
    )];

    let rows =
        compare_results_against_baseline(&baseline, &current, &CompareOptions::default()).unwrap();

    assert_eq!(rows.len(), 2);
    let bpm = rows.iter().find(|row| row.tool == "bpm").unwrap();
    assert!((bpm.ratio - 0.9).abs() < f64::EPSILON);
}

#[test]
fn baseline_comparison_fails_on_missing_key() {
    let baseline = vec![result_with_tools(
        "minimal",
        "repeat_install",
        vec![("npm", 20.0, vec![0])],
    )];
    let current = vec![result_with_tools(
        "minimal",
        "repeat_install",
        vec![("bpm", 9.0, vec![0])],
    )];

    let error = compare_results_against_baseline(&baseline, &current, &CompareOptions::default())
        .unwrap_err();
    assert!(format!("{error:#}").contains("baseline missing comparison key"));
}

#[test]
fn baseline_comparison_fails_on_duplicate_key() {
    let duplicate = result_with_tools("minimal", "repeat_install", vec![("bpm", 10.0, vec![0])]);
    let error = compare_results_against_baseline(
        &[duplicate.clone(), duplicate],
        &[result_with_tools(
            "minimal",
            "repeat_install",
            vec![("bpm", 9.0, vec![0])],
        )],
        &CompareOptions::default(),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("duplicate comparison key"));
}

#[test]
fn baseline_comparison_fails_on_nonzero_exit_codes() {
    let baseline = vec![result_with_tools(
        "minimal",
        "repeat_install",
        vec![("bpm", 10.0, vec![1])],
    )];
    let current = vec![result_with_tools(
        "minimal",
        "repeat_install",
        vec![("bpm", 9.0, vec![0])],
    )];

    let error = compare_results_against_baseline(&baseline, &current, &CompareOptions::default())
        .unwrap_err();
    assert!(format!("{error:#}").contains("nonzero exit code"));
}

#[test]
fn baseline_comparison_rejects_environment_mismatch_unless_informational() {
    let baseline = vec![result_with_tools(
        "minimal",
        "repeat_install",
        vec![("bpm", 10.0, vec![0])],
    )];
    let mut current = result_with_tools("minimal", "repeat_install", vec![("bpm", 9.0, vec![0])]);
    current.system.kernel = "25.0.0".to_string();

    let error =
        compare_results_against_baseline(&baseline, &[current.clone()], &CompareOptions::default())
            .unwrap_err();
    assert!(format!("{error:#}").contains("matching machine/system and versions"));

    let rows = compare_results_against_baseline(
        &baseline,
        &[current],
        &CompareOptions {
            regression_envelope: 2.0,
            informational: true,
        },
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn benchmark_result_backwards_deserializes_existing_reference_schema() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("benchmarks")
        .join("baselines")
        .join("reference.json");
    let json = std::fs::read_to_string(path).expect("read checked-in reference baseline");
    let results: Vec<BenchmarkResult> = serde_json::from_str(&json).unwrap();
    assert!(!results.is_empty());
}

#[test]
fn reference_baseline_has_strict_expected_keys_and_versions() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("benchmarks")
        .join("baselines")
        .join("reference.json");
    let json = std::fs::read_to_string(path).expect("read checked-in reference baseline");
    let results: Vec<BenchmarkResult> = serde_json::from_str(&json).unwrap();

    let expected: BTreeSet<(&str, &str)> = BTreeSet::from([
        ("large-frontend", "repeat_install"),
        ("large-frontend", "resolved_cold"),
        ("large-frontend", "true_cold"),
        ("many-small-files", "repeat_install"),
        ("many-small-files", "resolved_cold"),
        ("many-small-files", "true_cold"),
        ("minimal", "repeat_install"),
        ("monorepo", "repeat_install"),
        ("monorepo", "resolved_cold"),
        ("native-addon", "repeat_install"),
        ("native-addon", "resolved_cold"),
        ("native-addon", "true_cold"),
    ]);
    let actual: BTreeSet<(&str, &str)> = results
        .iter()
        .map(|result| (result.fixture.as_str(), result.scenario.as_str()))
        .collect();
    assert_eq!(actual, expected);

    let expected_tool_names: BTreeSet<&str> = BTreeSet::from(["bpm", "npm", "pnpm"]);
    let first_versions = results.first().unwrap().versions.clone();
    for result in &results {
        assert_eq!(
            result.number_of_runs, 7,
            "unexpected run count for {result:?}"
        );
        assert_eq!(result.versions, first_versions);
        for key in ["node", "npm", "pnpm", "bpm"] {
            assert!(result.versions.contains_key(key));
            assert!(result.system.runtime_versions.contains_key(key));
        }
        let tool_names: BTreeSet<&str> =
            result.tools.iter().map(|tool| tool.tool.as_str()).collect();
        assert_eq!(tool_names, expected_tool_names);
        for tool in &result.tools {
            assert_eq!(tool.exit_codes.len(), result.number_of_runs);
            assert!(tool.exit_codes.iter().all(|code| *code == 0));
        }
    }
}

#[test]
fn profile_filenames_and_manifest_are_deterministic() {
    let temp = tempfile::tempdir().unwrap();
    let mut versions = BTreeMap::new();
    versions.insert("bpm".to_string(), "bpm 0.1.10".to_string());
    versions.insert("node".to_string(), "v26.0.0".to_string());
    let manifest = BpmProfileManifest {
        fixture: "minimal".to_string(),
        diagnostic_only: true,
        note: "diagnostic".to_string(),
        system: sample_system(),
        versions,
        profiles: vec![BpmProfileEntry {
            fixture: "minimal".to_string(),
            scenario: ScenarioKind::RepeatInstall.name().to_string(),
            tool: "bpm".to_string(),
            metrics_file: bpm_profile_filename("minimal", ScenarioKind::RepeatInstall),
        }],
    };

    let path = write_bpm_profile_manifest(temp.path(), &manifest).unwrap();
    assert_eq!(
        bpm_profile_filename("minimal", ScenarioKind::RepeatInstall),
        "minimal--repeat_install--bpm-profile.json"
    );
    assert_eq!(path.file_name().unwrap(), "manifest.json");

    let roundtrip: BpmProfileManifest =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(roundtrip, manifest);
}

#[test]
fn bpm_profile_command_adds_json_metrics_without_changing_scorecard_command() {
    let work_dir = Path::new("/tmp/work");
    let store_dir = Path::new("/tmp/store");
    let score = install_command_spec(
        Tool::Bpm,
        work_dir,
        store_dir,
        ScenarioKind::RepeatInstall,
        None,
    );
    let profile = install_command_spec(
        Tool::Bpm,
        work_dir,
        store_dir,
        ScenarioKind::RepeatInstall,
        Some(Path::new("/tmp/profile.json")),
    );

    assert!(!score.args.contains(&"--json-metrics".to_string()));
    assert!(profile.args.contains(&"--json-metrics".to_string()));
}

fn fixture(name: &str) -> &'static bpm::bench::Fixture {
    FIXTURES
        .iter()
        .find(|fixture| fixture.name == name)
        .unwrap()
}

fn sample_system() -> SystemInfo {
    SystemInfo {
        machine: "arm64".into(),
        operating_system: "15.0".into(),
        kernel: "24.0.0".into(),
        runtime_versions: BTreeMap::new(),
    }
}

fn result_with_tools(
    fixture: &str,
    scenario: &str,
    tools: Vec<(&str, f64, Vec<i32>)>,
) -> BenchmarkResult {
    let versions = BTreeMap::from([
        ("node".to_string(), "v26.0.0".to_string()),
        ("npm".to_string(), "11.12.1".to_string()),
        ("pnpm".to_string(), "10.13.1".to_string()),
        ("bpm".to_string(), "bpm 0.1.10".to_string()),
    ]);
    BenchmarkResult {
        scenario: scenario.to_string(),
        fixture: fixture.to_string(),
        system: sample_system(),
        versions,
        cache_state: "warm".to_string(),
        number_of_runs: 1,
        tools: tools
            .into_iter()
            .map(|(tool, median, exit_codes)| bpm::bench::ToolResults {
                tool: tool.to_string(),
                wall_clock_ms: Stats {
                    values: vec![median],
                    median,
                    p95: median,
                    stddev: 0.0,
                },
                exit_codes,
            })
            .collect(),
    }
}
