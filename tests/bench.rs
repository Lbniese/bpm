//! Deterministic, offline tests for benchmark harness plumbing.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use bpm::bench::lock_setup_command_specs;
use bpm::bench::{
    bpm_profile_filename, compare_results_against_baseline, compare_results_against_tool,
    fixture_smoke_command_spec, install_command_spec, install_command_spec_with_env,
    lock_setup_command_specs_with_env, pnpm_build_policy_version_command_spec,
    pnpm_version_matches, run_scenario_with_runner, run_suite_with_availability,
    run_suite_with_availability_with_runner, validate_cold_network_samples,
    version_probe_command_spec, write_bpm_profile_manifest, BenchmarkProtocol, BenchmarkResult,
    BpmProfileEntry, BpmProfileManifest, CommandOutcome, CommandRunner, CommandSpec,
    CompareOptions, CrossToolGateOptions, RunSuiteOptions, SampleEnvironment, ScenarioKind, Stats,
    SystemInfo, Tool, FIXTURES,
};

#[derive(Default)]
struct RecordingRunner {
    commands: Vec<CommandSpec>,
    exit_codes: VecDeque<i32>,
    project_npmrcs: Vec<String>,
}

impl RecordingRunner {
    fn with_exit_codes(exit_codes: impl IntoIterator<Item = i32>) -> Self {
        Self {
            commands: Vec::new(),
            exit_codes: exit_codes.into_iter().collect(),
            project_npmrcs: Vec::new(),
        }
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&mut self, command: &CommandSpec) -> anyhow::Result<CommandOutcome> {
        self.commands.push(command.clone());
        if let Ok(contents) = fs::read_to_string(command.current_dir.join(".npmrc")) {
            self.project_npmrcs.push(contents);
        }
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
fn pnpm_fixture_version_match_is_pure_and_deterministic() {
    assert!(pnpm_version_matches(Some("10.13.1"), Some("10.13.1")));
    assert!(pnpm_version_matches(Some(" 10.13.1\n"), Some("10.13.1")));
}

#[test]
fn pnpm_fixture_version_mismatch_and_missing_probe_fail_closed() {
    assert!(!pnpm_version_matches(Some("10.13.1"), Some("11.0.0")));
    assert!(!pnpm_version_matches(Some("10.13.1"), None));
    assert!(!pnpm_version_matches(None, Some("10.13.1")));
    assert!(!pnpm_version_matches(Some(""), Some("")));
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
        protocol: None,
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
fn benchmark_protocol_round_trips_and_is_explicit() {
    let protocol = BenchmarkProtocol {
        protocol_version: 1,
        cache_isolation_mode: "per-sample-home-v1".into(),
        lifecycle_policy: "scripts-off".into(),
        execution_mode: "round-robin-rotated-v1".into(),
        post_install_validation: "fixture-smoke-v1".into(),
        round_tool_order: vec![
            vec!["npm".into(), "pnpm".into(), "bpm".into()],
            vec!["pnpm".into(), "bpm".into(), "npm".into()],
        ],
        profile_parity: false,
    };
    let mut result = BenchmarkResult {
        scenario: "true_cold".into(),
        fixture: "minimal".into(),
        system: sample_system(),
        versions: BTreeMap::new(),
        cache_state: "cold".into(),
        number_of_runs: 2,
        protocol: Some(protocol.clone()),
        tools: vec![],
    };
    let json = serde_json::to_string(&result).unwrap();
    assert!(json.contains("per-sample-home-v1"));
    assert!(json.contains("round_tool_order"));
    let back: BenchmarkResult = serde_json::from_str(&json).unwrap();
    assert_eq!(back.protocol, Some(protocol));

    result.protocol = None;
    let old_json = serde_json::to_string(&result).unwrap();
    let old_back: BenchmarkResult = serde_json::from_str(&old_json).unwrap();
    assert!(old_back.protocol.is_none());
}

#[test]
fn benchmark_commands_share_private_sample_environment() {
    let sample_root = PathBuf::from("/tmp/bpm-041-sample");
    let sample_env = SampleEnvironment::new(sample_root.join("home"));
    let work_dir = sample_root.join("project");
    let store = sample_root.join("stores/pnpm");
    let commands = [
        lock_setup_command_specs_with_env(Tool::Npm, &work_dir, &store, &sample_env),
        lock_setup_command_specs_with_env(Tool::Pnpm, &work_dir, &store, &sample_env),
        lock_setup_command_specs_with_env(Tool::Bpm, &work_dir, &store, &sample_env),
        vec![install_command_spec_with_env(
            Tool::Npm,
            &work_dir,
            &store,
            ScenarioKind::TrueCold,
            None,
            true,
            &sample_env,
        )],
        vec![install_command_spec_with_env(
            Tool::Pnpm,
            &work_dir,
            &store,
            ScenarioKind::TrueCold,
            None,
            true,
            &sample_env,
        )],
        vec![install_command_spec_with_env(
            Tool::Bpm,
            &work_dir,
            &store,
            ScenarioKind::TrueCold,
            None,
            true,
            &sample_env,
        )],
    ];
    let real_home = std::env::var("HOME").unwrap_or_default();
    for command_list in &commands {
        for command in command_list {
            assert_eq!(
                command.env.get("HOME"),
                Some(&sample_root.join("home").to_string_lossy().into_owned())
            );
            for key in [
                "XDG_CACHE_HOME",
                "XDG_CONFIG_HOME",
                "XDG_DATA_HOME",
                "XDG_STATE_HOME",
                "PNPM_HOME",
                "npm_config_userconfig",
                "npm_config_globalconfig",
                "BPM_STORE",
            ] {
                assert!(command.env.contains_key(key), "missing {key}");
                assert!(
                    command.env[key].starts_with(sample_root.to_string_lossy().as_ref()),
                    "{key} escaped sample root: {}",
                    command.env[key]
                );
            }
            if !real_home.is_empty() {
                assert!(
                    command.current_dir.to_string_lossy() != real_home
                        && command.args.iter().all(|arg| !arg.contains(&real_home))
                        && command
                            .env
                            .values()
                            .all(|value| !value.contains(&real_home)),
                    "sample command inherited the operator home: {command:?}"
                );
            }
        }
    }
    let removals = &commands[0][0].env_remove;
    for key in [
        "NPM_CONFIG_USERCONFIG",
        "NPM_CONFIG_GLOBALCONFIG",
        "NPM_CONFIG_REGISTRY",
        "NPM_CONFIG_AUTH",
        "NODE_AUTH_TOKEN",
        "NPM_TOKEN",
    ] {
        assert!(removals
            .iter()
            .any(|name| name.to_string_lossy().eq_ignore_ascii_case(key)));
    }
    for key in [
        "PATH",
        "LD_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
    ] {
        assert!(
            !removals
                .iter()
                .any(|name| name.to_string_lossy().eq_ignore_ascii_case(key)),
            "process requirement was removed: {key}"
        );
    }

    let pnpm = &commands[4][0];
    let store_dir = pnpm
        .args
        .iter()
        .position(|arg| arg == "--store-dir")
        .unwrap();
    assert!(pnpm.args[store_dir + 1].starts_with(sample_root.to_string_lossy().as_ref()));
    for tool in [Tool::Npm, Tool::Pnpm, Tool::Bpm] {
        let install = install_command_spec_with_env(
            tool,
            &work_dir,
            &store,
            ScenarioKind::TrueCold,
            None,
            false,
            &sample_env,
        );
        assert_eq!(install.env.get("HOME"), commands[0][0].env.get("HOME"));
    }
}

#[test]
fn every_sample_command_sanitizes_corepack_and_reapplies_only_controlled_state() {
    let temp = tempfile::tempdir().unwrap();
    let sample_root = temp.path().join("cold-sample");
    let home = sample_root.join("home");
    let project = sample_root.join("project");
    let store = sample_root.join("store");
    let hostile_names = vec![
        OsString::from("cOrEpAcK_HoStIlE_PoLiCy"),
        OsString::from("COREPACK_NPM_TOKEN"),
        OsString::from("CoRePaCk_NpM_ReGiStRy"),
        OsString::from("corepack_enable_network"),
        OsString::from("PATH"),
        OsString::from("DyLd_LiBrArY_PaTh"),
        OsString::from("hTtPs_PrOxY"),
    ];
    let sample_env = SampleEnvironment::with_inherited_names(&home, hostile_names.clone());
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("package.json"),
        r#"{"scripts":{"smoke":"node"}}"#,
    )
    .unwrap();
    fs::write(
        project.join(".npmrc"),
        bpm::parity_proxy::public_registry_npmrc_content(),
    )
    .unwrap();
    sample_env.prepare(Tool::Pnpm, &store).unwrap();

    let mut commands = Vec::new();
    for tool in [Tool::Npm, Tool::Pnpm, Tool::Bpm] {
        commands.extend(lock_setup_command_specs_with_env(
            tool,
            &project,
            &store,
            &sample_env,
        ));
        commands.push(install_command_spec_with_env(
            tool,
            &project,
            &store,
            ScenarioKind::TrueCold,
            None,
            false,
            &sample_env,
        ));
    }
    commands.push(pnpm_build_policy_version_command_spec(
        &project,
        &store,
        &sample_env,
    ));
    commands.push(
        fixture_smoke_command_spec(
            fixture("large-frontend"),
            Tool::Pnpm,
            &project,
            &store,
            &sample_env,
        )
        .unwrap()
        .unwrap(),
    );

    let expected_corepack_home = sample_root.join("home/tool-cache/corepack-home");
    for command in &commands {
        for hostile_name in &hostile_names {
            let lower = hostile_name.to_string_lossy().to_ascii_lowercase();
            let should_remove = lower.starts_with("corepack_");
            assert_eq!(
                command.env_remove.contains(hostile_name),
                should_remove,
                "unexpected removal for {hostile_name:?} in {}",
                command.label
            );
        }
        for preserved_name in ["PATH", "DyLd_LiBrArY_PaTh", "hTtPs_PrOxY"] {
            assert!(
                !command
                    .env_remove
                    .iter()
                    .any(|name| name.to_string_lossy().eq_ignore_ascii_case(preserved_name)),
                "{} was removed from {}",
                preserved_name,
                command.label
            );
        }
        assert_eq!(
            Path::new(&command.env["COREPACK_HOME"]),
            expected_corepack_home
        );
        assert_eq!(
            command.env["COREPACK_NPM_REGISTRY"],
            "https://registry.npmjs.org/"
        );
        assert_eq!(command.env["COREPACK_ENV_FILE"], "0");
        assert!(
            !command.env.contains_key("npm_config_registry"),
            "Corepack registry must not replace the project registry in {}",
            command.label
        );
    }
    assert!(commands
        .iter()
        .any(|command| command.label == "pnpm_build_policy_version"));
    assert!(commands
        .iter()
        .any(|command| command.label == "fixture_smoke"));
}

#[test]
fn hostile_system_user_and_project_configs_cannot_override_a_sample() {
    let temp = tempfile::tempdir().unwrap();
    let sample_home = temp.path().join("sample-home");
    let project = temp.path().join("project");
    let store = temp.path().join("store");
    fs::create_dir_all(&sample_home).unwrap();
    fs::create_dir_all(&project).unwrap();
    let secret = "secret-value-must-never-enter-a-command-spec";
    let hostile = format!("registry=https://attacker.invalid/\\n_authToken={secret}\\n");
    let hostile_system = temp.path().join("hostile-system.npmrc");
    fs::write(&hostile_system, &hostile).unwrap();
    fs::write(sample_home.join(".npmrc"), &hostile).unwrap();
    fs::write(sample_home.join(".npmrc-global"), &hostile).unwrap();
    fs::write(project.join(".npmrc"), &hostile).unwrap();

    let sample_env = SampleEnvironment::new(&sample_home);
    sample_env.prepare(Tool::Npm, &store).unwrap();
    // This is the same controlled project write used after fixture copying;
    // it replaces any fixture/project config rather than merging it.
    fs::write(
        project.join(".npmrc"),
        bpm::parity_proxy::public_registry_npmrc_content(),
    )
    .unwrap();
    let command = install_command_spec_with_env(
        Tool::Npm,
        &project,
        &store,
        ScenarioKind::TrueCold,
        None,
        false,
        &sample_env,
    );

    assert_eq!(
        fs::read_to_string(&command.env["npm_config_userconfig"]).unwrap(),
        ""
    );
    assert_eq!(
        fs::read_to_string(&command.env["npm_config_globalconfig"]).unwrap(),
        ""
    );
    assert_eq!(
        fs::read_to_string(project.join(".npmrc")).unwrap(),
        bpm::parity_proxy::public_registry_npmrc_content()
    );
    assert_ne!(
        Path::new(&command.env["npm_config_globalconfig"]),
        hostile_system.as_path()
    );
    assert!(!format!("{command:?}").contains(secret));
    assert!(command.env_remove.iter().any(|name| name
        .to_string_lossy()
        .eq_ignore_ascii_case("NPM_CONFIG_GLOBALCONFIG")));
}

#[test]
fn hostile_probe_config_is_private_without_removing_path_or_proxies() {
    let temp = tempfile::tempdir().unwrap();
    let operator_home = temp.path().join("operator-home");
    let operator_project = temp.path().join("operator-project");
    let probe_root = temp.path().join("private-probe");
    fs::create_dir_all(&operator_home).unwrap();
    fs::create_dir_all(&operator_project).unwrap();
    fs::create_dir_all(probe_root.join("project")).unwrap();

    // These hostile values model the operator's files but are never read by
    // the test or by a probe command. Only the explicit inherited names below
    // are supplied to the pure command-spec constructor.
    fs::write(
        operator_home.join(".npmrc"),
        "registry=https://attacker.invalid/\n_authToken=hostile-token\n",
    )
    .unwrap();
    fs::write(
        operator_project.join(".npmrc"),
        "registry=https://attacker.invalid/\n_authToken=hostile-token\n",
    )
    .unwrap();
    fs::write(
        probe_root.join("project/.npmrc"),
        bpm::parity_proxy::public_registry_npmrc_content(),
    )
    .unwrap();

    let hostile_names = vec![
        OsString::from("nPm_CoNfIg_ReGiStRy"),
        OsString::from("NPM_CONFIG_USERCONFIG"),
        OsString::from("npm_config_globalconfig"),
        OsString::from("Node_Auth_Token"),
        OsString::from("NPM_TOKEN"),
        OsString::from("pnpm_store_dir"),
        OsString::from("YARN_NPM_AUTH_TOKEN"),
        OsString::from("BUN_INSTALL_CACHE_DIR"),
        OsString::from("BPM_REGISTRY"),
        OsString::from("COREPACK_HOME"),
        OsString::from("cOrEpAcK_eNv_FiLe"),
        OsString::from("CoRePaCk_NpM_ReGiStRy"),
        OsString::from("cOrEpAcK_NpM_ToKeN"),
        OsString::from("COREPACK_NPM_USERNAME"),
        OsString::from("CorePack_Npm_Password"),
        OsString::from("cOrEpAcK_EnAbLe_NeTwOrK"),
        OsString::from("COREPACK_DEFAULT_TO_LATEST"),
        OsString::from("COREPACK_ENABLE_PROJECT_SPEC"),
        OsString::from("COREPACK_ENABLE_STRICT"),
        OsString::from("COREPACK_INTEGRITY_KEYS"),
        OsString::from("nOdE_oPtIoNs"),
        OsString::from("NODE_PATH"),
        OsString::from("PATH"),
        OsString::from("DYLD_LIBRARY_PATH"),
        OsString::from("LD_LIBRARY_PATH"),
        OsString::from("HTTP_PROXY"),
        OsString::from("HTTPS_PROXY"),
        OsString::from("ALL_PROXY"),
        OsString::from("NO_PROXY"),
    ];

    for tool in [Tool::Npm, Tool::Pnpm] {
        let command = version_probe_command_spec(tool, &probe_root, &hostile_names);
        assert_eq!(command.program, PathBuf::from(tool.name()));
        assert_eq!(command.args, ["--version"]);
        assert_eq!(command.current_dir, probe_root.join("project"));
        assert_ne!(command.current_dir, operator_project);
        assert_eq!(
            command.env["HOME"],
            probe_root.join("home").to_string_lossy().as_ref()
        );
        for (key, expected) in [
            ("XDG_CACHE_HOME", probe_root.join("home/xdg-cache")),
            ("XDG_CONFIG_HOME", probe_root.join("home/xdg-config")),
            ("XDG_DATA_HOME", probe_root.join("home/xdg-data")),
            ("XDG_STATE_HOME", probe_root.join("home/xdg-state")),
            ("PNPM_HOME", probe_root.join("home/pnpm-home")),
            ("npm_config_userconfig", probe_root.join("home/.npmrc")),
            (
                "npm_config_globalconfig",
                probe_root.join("home/.npmrc-global"),
            ),
            (
                "COREPACK_HOME",
                probe_root.join("home/tool-cache/corepack-home"),
            ),
            ("BPM_STORE", probe_root.join("bpm-store")),
        ] {
            assert_eq!(Path::new(&command.env[key]), expected);
        }
        assert_eq!(
            command.env["npm_config_registry"],
            "https://registry.npmjs.org/"
        );
        assert_eq!(
            command.env["COREPACK_NPM_REGISTRY"],
            "https://registry.npmjs.org/"
        );
        assert_eq!(command.env["COREPACK_ENV_FILE"], "0");
        assert_eq!(
            Path::new(&command.env["npm_config_cache"]),
            probe_root.join("home/tool-cache").join(tool.name())
        );

        for hostile_name in &hostile_names {
            let lower = hostile_name.to_string_lossy().to_ascii_lowercase();
            let should_remove = lower.starts_with("npm_config_")
                || lower.starts_with("pnpm_")
                || lower.starts_with("yarn_")
                || lower.starts_with("bun_")
                || lower.starts_with("bpm_")
                || lower.starts_with("corepack_")
                || lower == "node_auth_token"
                || lower == "npm_token"
                || lower == "npm_auth_token"
                || lower == "node_options"
                || lower == "node_path";
            assert_eq!(
                command.env_remove.contains(hostile_name),
                should_remove,
                "unexpected probe environment removal for {hostile_name:?}"
            );
        }
        for auth_name in [
            "COREPACK_NPM_TOKEN",
            "COREPACK_NPM_USERNAME",
            "COREPACK_NPM_PASSWORD",
        ] {
            assert!(
                !command
                    .env
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case(auth_name)),
                "probe serialized an auth variable name: {auth_name}"
            );
        }
        let serialized_env = serde_json::to_string(&command.env).unwrap();
        assert!(!serialized_env.contains("hostile-token"));
        assert!(command
            .env
            .values()
            .all(|value| !value.contains("attacker.invalid")));
    }

    assert_eq!(
        fs::read_to_string(probe_root.join("project/.npmrc")).unwrap(),
        bpm::parity_proxy::public_registry_npmrc_content()
    );
}

#[test]
#[ignore = "opt-in host-tool proof: requires npm and pnpm on PATH; run with --ignored"]
fn real_npm_and_pnpm_remain_detectable_through_inherited_path() {
    assert!(
        Tool::Npm.detect(),
        "npm must remain discoverable through PATH"
    );
    assert!(
        Tool::Pnpm.detect(),
        "pnpm must remain discoverable through PATH"
    );
}

#[test]
fn every_benchmark_project_uses_public_or_per_sample_proxy_registry() {
    let mut normal_runner = RecordingRunner::default();
    run_suite_with_availability_with_runner(
        &[ScenarioKind::SecondProjectSameGraph],
        fixture("minimal"),
        &[Tool::Npm],
        &RunSuiteOptions {
            num_runs: 1,
            require_tools: false,
            ignore_scripts: false,
            profile_parity: false,
        },
        |_| true,
        &mut normal_runner,
    )
    .unwrap();
    assert!(!normal_runner.project_npmrcs.is_empty());
    assert!(normal_runner
        .project_npmrcs
        .iter()
        .all(|content| content == bpm::parity_proxy::public_registry_npmrc_content()));

    let mut parity_runner = RecordingRunner::default();
    let parity = run_suite_with_availability_with_runner(
        &[ScenarioKind::TrueCold],
        fixture("minimal"),
        &[Tool::Npm],
        &RunSuiteOptions {
            num_runs: 2,
            require_tools: false,
            ignore_scripts: false,
            profile_parity: true,
        },
        |_| true,
        &mut parity_runner,
    )
    .unwrap();
    assert_eq!(parity.results[0].tools[0].network_samples.len(), 2);
    assert_eq!(parity_runner.project_npmrcs.len(), 2);
    assert!(parity_runner
        .project_npmrcs
        .iter()
        .all(|content| content.starts_with("registry=http://127.0.0.1:")
            && !content.contains("attacker")));
}

#[test]
#[ignore = "opt-in provenance proof: requires host npm on PATH; run with --ignored"]
fn versions_record_npm_for_harness_lock_setup_without_scoring_npm() {
    assert!(std::process::Command::new("npm")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("npm must be installed for benchmark provenance tests")
        .success());
    let mut runner = RecordingRunner::default();
    let suite = run_suite_with_availability_with_runner(
        &[ScenarioKind::ResolvedCold],
        fixture("minimal"),
        &[Tool::Bpm],
        &RunSuiteOptions {
            num_runs: 1,
            require_tools: false,
            ignore_scripts: false,
            profile_parity: false,
        },
        |tool| tool == Tool::Bpm,
        &mut runner,
    )
    .unwrap();
    assert!(suite.results[0].versions.contains_key("npm"));
}

#[test]
fn npm_pnpm_yarn_and_bun_cache_paths_follow_the_selected_root() {
    let sample_home = PathBuf::from("/tmp/bpm-041-cache-sample/home");
    let cache_root = PathBuf::from("/tmp/bpm-041-cache-scenario/tool-cache");
    let sample_env = SampleEnvironment::with_cache_root(&sample_home, &cache_root);
    let work_dir = Path::new("/tmp/bpm-041-cache-sample/project");
    let store = Path::new("/tmp/bpm-041-cache-scenario/store");

    for tool in [Tool::Npm, Tool::Pnpm, Tool::Yarn, Tool::Bun] {
        let command = install_command_spec_with_env(
            tool,
            work_dir,
            store,
            ScenarioKind::TrueCold,
            None,
            false,
            &sample_env,
        );
        let (key, expected) = match tool {
            Tool::Npm | Tool::Pnpm => ("npm_config_cache", cache_root.join(tool.name())),
            Tool::Yarn => ("YARN_CACHE_FOLDER", cache_root.join("yarn")),
            Tool::Bun => ("BUN_INSTALL_CACHE_DIR", cache_root.join("bun")),
            Tool::Bpm => unreachable!(),
        };
        assert_eq!(Path::new(&command.env[key]), expected);
        assert_eq!(
            Path::new(&command.env["npm_config_userconfig"]),
            sample_home.join(".npmrc")
        );
        assert_eq!(
            Path::new(&command.env["npm_config_globalconfig"]),
            sample_home.join(".npmrc-global")
        );
    }
}

#[test]
fn bench_tools_include_bpm() {
    let names: Vec<&str> = Tool::all().iter().map(|c| c.name()).collect();
    assert!(names.contains(&"bpm"), "bpm tool missing: {names:?}");
    assert!(names.contains(&"npm"));
    assert!(names.contains(&"yarn"));
    assert!(names.contains(&"bun"));
}

#[test]
#[cfg(unix)]
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
            &work_dir
                .join(".sample-home/tool-cache/pnpm")
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
        false,
        None,
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

    run_scenario_with_runner(
        ScenarioKind::TrueCold,
        fixture,
        Tool::Bpm,
        1,
        false,
        None,
        &mut runner,
    )
    .unwrap();

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

    let error = run_scenario_with_runner(
        ScenarioKind::TrueCold,
        fixture,
        Tool::Npm,
        1,
        false,
        None,
        &mut runner,
    )
    .unwrap_err();

    let message = format!("{error:#}");
    assert!(message.contains("timed benchmark failed"));
    assert!(message.contains("tool=npm"));
    assert!(message.contains("scenario=true_cold"));
    assert!(message.contains("exit_code=17"));
}

#[test]
fn run_suite_rejects_duplicate_tools_before_accumulator_removal() {
    let mut runner = RecordingRunner::default();
    let error = run_suite_with_availability_with_runner(
        &[ScenarioKind::TrueCold],
        fixture("minimal"),
        &[Tool::Npm, Tool::Npm],
        &RunSuiteOptions {
            num_runs: 1,
            require_tools: false,
            ignore_scripts: false,
            profile_parity: false,
        },
        |_| true,
        &mut runner,
    )
    .unwrap_err();

    let message = format!("{error:#}");
    assert!(message.contains("duplicate tool 'npm'"), "{message}");
    assert!(runner.commands.is_empty());
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
            ignore_scripts: false,
            profile_parity: false,
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
            ignore_scripts: false,
            profile_parity: false,
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
            ignore_scripts: false,
            profile_parity: false,
        },
        |tool| matches!(tool, Tool::Bpm),
    )
    .unwrap();

    assert!(suite.results.is_empty());
}

#[test]
fn suite_round_robin_order_is_rotated_and_samples_stay_logical() {
    let mut runner = RecordingRunner::default();
    let suite = run_suite_with_availability_with_runner(
        &[ScenarioKind::TrueCold],
        fixture("minimal"),
        &[Tool::Npm, Tool::Pnpm, Tool::Bpm],
        &RunSuiteOptions {
            num_runs: 4,
            require_tools: false,
            ignore_scripts: false,
            profile_parity: false,
        },
        |_| true,
        &mut runner,
    )
    .unwrap();

    let timed_tools: Vec<&str> = runner
        .commands
        .iter()
        .filter(|command| command.label == "install")
        .map(|command| match command.program.to_string_lossy().as_ref() {
            "npm" => "npm",
            "pnpm" => "pnpm",
            _ => "bpm",
        })
        .collect();
    assert_eq!(
        timed_tools,
        ["npm", "pnpm", "bpm", "pnpm", "bpm", "npm", "bpm", "npm", "pnpm", "npm", "pnpm", "bpm",]
    );
    let protocol = suite.results[0].protocol.as_ref().unwrap();
    assert_eq!(
        protocol.round_tool_order,
        vec![
            vec!["npm", "pnpm", "bpm"],
            vec!["pnpm", "bpm", "npm"],
            vec!["bpm", "npm", "pnpm"],
            vec!["npm", "pnpm", "bpm"],
        ]
    );
    for tool in &suite.results[0].tools {
        assert_eq!(tool.wall_clock_ms.values.len(), 4);
        assert_eq!(tool.exit_codes, [0, 0, 0, 0]);
    }
}

#[test]
fn cold_caches_are_fresh_but_warm_caches_are_shared_per_tool() {
    let mut cold_runner = RecordingRunner::default();
    run_suite_with_availability_with_runner(
        &[ScenarioKind::TrueCold],
        fixture("minimal"),
        &[Tool::Npm],
        &RunSuiteOptions {
            num_runs: 2,
            require_tools: false,
            ignore_scripts: false,
            profile_parity: false,
        },
        |_| true,
        &mut cold_runner,
    )
    .unwrap();
    let cold_installs: Vec<&CommandSpec> = cold_runner
        .commands
        .iter()
        .filter(|command| command.label == "install")
        .collect();
    assert_eq!(cold_installs.len(), 2);
    let cold_caches: Vec<PathBuf> = cold_installs
        .iter()
        .map(|command| PathBuf::from(command.env["npm_config_cache"].clone()))
        .collect();
    assert_ne!(cold_caches[0], cold_caches[1]);
    assert!(cold_caches
        .iter()
        .all(|path| path.ends_with(Path::new("tool-cache").join("npm"))));
    let cold_corepack_homes: BTreeSet<PathBuf> = cold_installs
        .iter()
        .map(|command| PathBuf::from(command.env["COREPACK_HOME"].clone()))
        .collect();
    assert_eq!(cold_corepack_homes.len(), 2);
    assert!(cold_corepack_homes
        .iter()
        .all(|path| { path.ends_with(Path::new("tool-cache").join("corepack-home")) }));
    assert!(cold_installs.iter().all(|command| {
        Path::new(&command.env["npm_config_cache"]).starts_with(Path::new(&command.env["HOME"]))
    }));

    let mut warm_runner = RecordingRunner::default();
    run_suite_with_availability_with_runner(
        &[ScenarioKind::WarmStore],
        fixture("minimal"),
        &[Tool::Npm],
        &RunSuiteOptions {
            num_runs: 2,
            require_tools: false,
            ignore_scripts: false,
            profile_parity: false,
        },
        |_| true,
        &mut warm_runner,
    )
    .unwrap();
    let warm_installs: Vec<&CommandSpec> = warm_runner
        .commands
        .iter()
        .filter(|command| command.label == "install")
        .collect();
    assert_eq!(warm_installs.len(), 4, "seed and timed install per round");
    let warm_caches: BTreeSet<PathBuf> = warm_installs
        .iter()
        .map(|command| PathBuf::from(command.env["npm_config_cache"].clone()))
        .collect();
    assert_eq!(warm_caches.len(), 1);
    let warm_cache = warm_caches.iter().next().unwrap();
    assert!(warm_cache.ends_with(Path::new("cache").join("npm")));
    let warm_corepack_homes: BTreeSet<PathBuf> = warm_installs
        .iter()
        .map(|command| PathBuf::from(command.env["COREPACK_HOME"].clone()))
        .collect();
    assert_eq!(warm_corepack_homes.len(), 1);
    let warm_corepack_home = warm_corepack_homes.iter().next().unwrap();
    assert!(warm_corepack_home.ends_with(Path::new("cache").join("corepack-home")));
    let warm_homes: BTreeSet<PathBuf> = warm_installs
        .iter()
        .map(|command| PathBuf::from(command.env["HOME"].clone()))
        .collect();
    assert_eq!(warm_homes.len(), 2, "home remains private per sample");
    assert!(warm_installs.iter().all(|command| {
        !Path::new(&command.env["npm_config_cache"]).starts_with(Path::new(&command.env["HOME"]))
    }));
}

#[test]
fn fixture_smoke_follows_timed_install_and_is_not_a_timed_exit_code() {
    let mut runner = RecordingRunner::default();
    let result = run_scenario_with_runner(
        ScenarioKind::TrueCold,
        fixture("large-frontend"),
        Tool::Npm,
        1,
        true,
        None,
        &mut runner,
    )
    .unwrap();

    assert_eq!(
        runner
            .commands
            .iter()
            .map(|command| command.label)
            .collect::<Vec<_>>(),
        ["install", "fixture_smoke"]
    );
    assert_eq!(result.exit_codes, [0]);
}

#[test]
fn cold_network_leak_tripwire_rejects_collapse_and_accepts_stable_samples() {
    let shape = |request_count| bpm::parity_proxy::NetworkShape {
        request_count,
        ..Default::default()
    };
    let error =
        validate_cold_network_samples("pnpm", &[shape(116), shape(0), shape(0)], 3).unwrap_err();
    assert!(format!("{error:#}").contains("tool=pnpm"));
    validate_cold_network_samples("pnpm", &[shape(116), shape(116), shape(115)], 3).unwrap();
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
    assert!(format!("{error:#}").contains("matching machine/system"));

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
fn strict_comparison_accepts_differing_bpm_version_only() {
    // A regression gate intentionally compares two BPM builds on the same
    // host; a BPM-only version difference must succeed in strict mode.
    let baseline = vec![result_with_tools(
        "minimal",
        "repeat_install",
        vec![("bpm", 10.0, vec![0])],
    )];
    let mut current = result_with_tools("minimal", "repeat_install", vec![("bpm", 10.0, vec![0])]);
    // Only the BPM version differs; host and node/npm/pnpm stay identical.
    current
        .versions
        .insert("bpm".to_string(), "bpm 0.0.1".to_string());
    current
        .system
        .runtime_versions
        .insert("bpm".to_string(), "bpm 0.0.1".to_string());

    let rows = compare_results_against_baseline(&baseline, &[current], &CompareOptions::default())
        .expect("BPM-only version difference is comparable in strict mode");
    assert_eq!(rows.len(), 1);
}

#[test]
fn strict_comparison_rejects_differing_external_runtime() {
    // Node/npm/pnpm or kernel differences remain strict errors even when BPM
    // is also allowed to differ.
    let baseline = vec![result_with_tools(
        "minimal",
        "repeat_install",
        vec![("bpm", 10.0, vec![0])],
    )];
    let mut current = result_with_tools("minimal", "repeat_install", vec![("bpm", 10.0, vec![0])]);
    current
        .versions
        .insert("node".to_string(), "v99.0.0".to_string());

    let error = compare_results_against_baseline(&baseline, &[current], &CompareOptions::default())
        .expect_err("external runtime mismatch must fail strictly");
    assert!(format!("{error:#}").contains("matching machine/system"));
}

#[test]
fn strict_comparison_rejects_ratio_above_envelope() {
    let baseline = vec![result_with_tools(
        "minimal",
        "repeat_install",
        vec![("bpm", 10.0, vec![0])],
    )];
    let current = vec![result_with_tools(
        "minimal",
        "repeat_install",
        vec![("bpm", 100.0, vec![0])], // ratio 10x exceeds envelope 2.0
    )];

    let error = compare_results_against_baseline(
        &baseline,
        &current,
        &CompareOptions {
            regression_envelope: 2.0,
            informational: false,
        },
    )
    .expect_err("ratio above envelope must fail strictly");
    assert!(format!("{error:#}").contains("regression exceeds envelope"));
}

#[test]
fn informational_comparison_returns_row_despite_ratio_excess() {
    let baseline = vec![result_with_tools(
        "minimal",
        "repeat_install",
        vec![("bpm", 10.0, vec![0])],
    )];
    let current = vec![result_with_tools(
        "minimal",
        "repeat_install",
        vec![("bpm", 100.0, vec![0])], // ratio 10x exceeds envelope 2.0
    )];

    let rows = compare_results_against_baseline(
        &baseline,
        &current,
        &CompareOptions {
            regression_envelope: 2.0,
            informational: true,
        },
    )
    .expect("informational mode reports without gating");
    assert_eq!(rows.len(), 1);
    assert!(rows[0].ratio > 2.0);
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
    let error = compare_results_against_tool(&results, &gate_options()).unwrap_err();
    assert!(format!("{error:#}").contains("protocol-less"));
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
fn cross_tool_gate_passes_at_inclusive_boundary() {
    let result = paired_result(vec![100.0; 7], vec![100.0; 7], true, false);
    let rows = compare_results_against_tool(&[result], &gate_options()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].paired_median_ratio, 1.0);
    assert_eq!(rows[0].paired_p95_ratio, 1.0);
}

#[test]
fn cross_tool_gate_rejects_paired_median_failure() {
    let result = paired_result(vec![110.0; 7], vec![100.0; 7], true, false);
    let error = compare_results_against_tool(&[result], &gate_options()).unwrap_err();
    assert!(format!("{error:#}").contains("paired_median_ratio"));
}

#[test]
fn cross_tool_gate_rejects_paired_p95_failure() {
    let result = paired_result(
        vec![50.0, 50.0, 50.0, 50.0, 50.0, 50.0, 200.0],
        vec![100.0; 7],
        true,
        false,
    );
    let error = compare_results_against_tool(&[result], &gate_options()).unwrap_err();
    assert!(format!("{error:#}").contains("paired_p95_ratio"));
}

#[test]
fn cross_tool_gate_rejects_protocol_mismatch_and_protocolless_results() {
    let first = paired_result(vec![90.0; 7], vec![100.0; 7], true, false);
    let mut second = paired_result(vec![90.0; 7], vec![100.0; 7], true, false);
    second.protocol.as_mut().unwrap().round_tool_order = (0..7)
        .map(|round| {
            if round % 2 == 0 {
                vec!["bpm".into(), "pnpm".into()]
            } else {
                vec!["pnpm".into(), "bpm".into()]
            }
        })
        .collect();
    let error =
        compare_results_against_tool(&[first, second.clone()], &gate_options()).unwrap_err();
    assert!(format!("{error:#}").contains("protocol mismatch"));
    second.protocol.as_mut().unwrap().execution_mode = "legacy".into();
    let error = compare_results_against_tool(&[second], &gate_options()).unwrap_err();
    assert!(format!("{error:#}").contains("protocol"));

    let old = paired_result(vec![90.0; 7], vec![100.0; 7], false, false);
    let error = compare_results_against_tool(&[old], &gate_options()).unwrap_err();
    assert!(format!("{error:#}").contains("protocol-less"));
}

#[test]
fn protocol_v1_requires_exact_unique_tool_membership_and_unique_results() {
    let mut duplicate_order = paired_result(vec![90.0; 7], vec![100.0; 7], true, false);
    duplicate_order.protocol.as_mut().unwrap().round_tool_order[0] =
        vec!["pnpm".into(), "bpm".into(), "bpm".into()];
    let error = compare_results_against_tool(&[duplicate_order], &gate_options()).unwrap_err();
    assert!(format!("{error:#}").contains("duplicate tool entries"));

    let mut missing_order = paired_result(vec![90.0; 7], vec![100.0; 7], true, false);
    missing_order.protocol.as_mut().unwrap().round_tool_order =
        (0..7).map(|_| vec!["pnpm".into()]).collect();
    let error = compare_results_against_tool(&[missing_order], &gate_options()).unwrap_err();
    assert!(format!("{error:#}").contains("exactly match result tools"));

    let mut extra_order = paired_result(vec![90.0; 7], vec![100.0; 7], true, false);
    extra_order.protocol.as_mut().unwrap().round_tool_order = (0..7)
        .map(|round| {
            if round % 2 == 0 {
                vec!["pnpm".into(), "bpm".into(), "npm".into()]
            } else {
                vec!["bpm".into(), "npm".into(), "pnpm".into()]
            }
        })
        .collect();
    let error = compare_results_against_tool(&[extra_order], &gate_options()).unwrap_err();
    assert!(format!("{error:#}").contains("exactly match result tools"));

    let mut duplicate_results = paired_result(vec![90.0; 7], vec![100.0; 7], true, false);
    duplicate_results
        .tools
        .push(duplicate_results.tools[1].clone());
    let error = compare_results_against_tool(&[duplicate_results], &gate_options()).unwrap_err();
    assert!(format!("{error:#}").contains("duplicate ToolResults"));
}

#[test]
fn cross_tool_gate_rejects_missing_tool_and_too_few_samples() {
    let mut missing = paired_result(vec![90.0; 7], vec![100.0; 7], true, false);
    missing.tools.retain(|tool| tool.tool == "bpm");
    let error = compare_results_against_tool(&[missing], &gate_options()).unwrap_err();
    assert!(format!("{error:#}").contains("missing tool pnpm"));

    let mut options = gate_options();
    options.runs = 6;
    let result = paired_result(vec![90.0; 7], vec![100.0; 7], true, false);
    let error = compare_results_against_tool(&[result], &options).unwrap_err();
    assert!(format!("{error:#}").contains("runs >= 7"));
}

#[test]
fn cross_tool_gate_rejects_scripts_on_proxy_and_invalid_limits() {
    let scripts_on = paired_result(vec![90.0; 7], vec![100.0; 7], true, false);
    let mut options = gate_options();
    options.ignore_scripts = false;
    let error = compare_results_against_tool(&[scripts_on], &options).unwrap_err();
    assert!(format!("{error:#}").contains("ignore-scripts"));

    let proxy = paired_result(vec![90.0; 7], vec![100.0; 7], true, true);
    let error = compare_results_against_tool(&[proxy], &gate_options()).unwrap_err();
    assert!(format!("{error:#}").contains("parity"));

    let result = paired_result(vec![90.0; 7], vec![100.0; 7], true, false);
    let mut invalid = gate_options();
    invalid.max_median_ratio = 0.0;
    assert!(compare_results_against_tool(std::slice::from_ref(&result), &invalid).is_err());
    invalid.max_median_ratio = f64::NAN;
    assert!(compare_results_against_tool(&[result], &invalid).is_err());
}

#[test]
fn profile_filenames_and_manifest_are_deterministic() {
    let temp = tempfile::tempdir().unwrap();
    let mut versions = BTreeMap::new();
    versions.insert(
        "bpm".to_string(),
        format!("bpm {}", env!("CARGO_PKG_VERSION")),
    );
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
        false,
    );
    let profile = install_command_spec(
        Tool::Bpm,
        work_dir,
        store_dir,
        ScenarioKind::RepeatInstall,
        Some(Path::new("/tmp/profile.json")),
        false,
    );

    assert!(!score.args.contains(&"--json-metrics".to_string()));
    assert!(profile.args.contains(&"--json-metrics".to_string()));
}

#[test]
fn install_command_spec_appends_ignore_scripts_when_enabled() {
    let work_dir = Path::new("/tmp/work");
    let store = Path::new("/tmp/store");
    // every manager accepts the same flag, so enabling the sweep
    // appends `--ignore-scripts` uniformly across npm/pnpm/bpm.
    for tool in [Tool::Npm, Tool::Pnpm, Tool::Bpm] {
        let spec = install_command_spec(tool, work_dir, store, ScenarioKind::TrueCold, None, true);
        assert!(
            spec.args.contains(&"--ignore-scripts".to_string()),
            "{} install must carry --ignore-scripts when the sweep is enabled: {:?}",
            tool.name(),
            spec.args,
        );
    }
}

#[test]
fn install_command_spec_omits_ignore_scripts_by_default() {
    let work_dir = Path::new("/tmp/work");
    let store = Path::new("/tmp/store");
    // The default path (sweep off) must stay byte-identical to the headline
    // baseline: no `--ignore-scripts` anywhere.
    for tool in [Tool::Npm, Tool::Pnpm, Tool::Bpm] {
        let spec = install_command_spec(tool, work_dir, store, ScenarioKind::TrueCold, None, false);
        assert!(
            !spec.args.contains(&"--ignore-scripts".to_string()),
            "{} install must not carry --ignore-scripts by default: {:?}",
            tool.name(),
            spec.args,
        );
    }
}

#[test]
fn ignore_scripts_threads_through_to_true_cold_timed_install() {
    let fixture = fixture("minimal");
    let mut runner = RecordingRunner::with_exit_codes([0]);

    run_scenario_with_runner(
        ScenarioKind::TrueCold,
        fixture,
        Tool::Bpm,
        1,
        true,
        None,
        &mut runner,
    )
    .unwrap();

    // True-cold emits a single command (the timed install); it must carry the flag.
    assert_eq!(runner.commands.len(), 1);
    assert!(runner.commands[0]
        .args
        .contains(&"--ignore-scripts".to_string()));
}

#[test]
fn parity_npmrc_content_redirects_registry_to_proxy() {
    let addr: std::net::SocketAddr = "127.0.0.1:18080".parse().unwrap();
    assert_eq!(
        bpm::parity_proxy::parity_npmrc_content(addr),
        "registry=http://127.0.0.1:18080\n"
    );
}

#[test]
fn profile_parity_off_yields_no_network_shape() {
    // Without --profile-parity the harness passes parity_addr=None, so the
    // returned ToolResults must carry no NetworkShape (byte-identical to the
    // baseline schema).
    let fixture = fixture("minimal");
    let mut runner = RecordingRunner::with_exit_codes([0]);
    let result = run_scenario_with_runner(
        ScenarioKind::TrueCold,
        fixture,
        Tool::Bpm,
        1,
        false,
        None,
        &mut runner,
    )
    .unwrap();
    assert!(result.network.is_none());
}

fn gate_options() -> CrossToolGateOptions {
    CrossToolGateOptions {
        target: Tool::Pnpm,
        max_median_ratio: 1.0,
        max_p95_ratio: 1.0,
        require_tools: true,
        runs: 7,
        ignore_scripts: true,
        profile_parity: false,
    }
}

fn paired_result(
    bpm_values: Vec<f64>,
    target_values: Vec<f64>,
    with_protocol: bool,
    profile_parity: bool,
) -> BenchmarkResult {
    assert_eq!(bpm_values.len(), target_values.len());
    let number_of_runs = bpm_values.len();
    let protocol = with_protocol.then(|| BenchmarkProtocol {
        protocol_version: 1,
        cache_isolation_mode: "per-sample-home-v1".into(),
        lifecycle_policy: "scripts-off".into(),
        execution_mode: "round-robin-rotated-v1".into(),
        post_install_validation: "fixture-smoke-v1".into(),
        round_tool_order: (0..number_of_runs)
            .map(|round| {
                if round % 2 == 0 {
                    vec!["pnpm".into(), "bpm".into()]
                } else {
                    vec!["bpm".into(), "pnpm".into()]
                }
            })
            .collect(),
        profile_parity,
    });
    BenchmarkResult {
        scenario: "true_cold".into(),
        fixture: "large-frontend".into(),
        system: sample_system(),
        versions: BTreeMap::from([
            ("node".into(), "v26.0.0".into()),
            ("pnpm".into(), "10.13.1".into()),
            ("bpm".into(), "bpm 0.3.0".into()),
        ]),
        cache_state: "cold".into(),
        number_of_runs,
        protocol,
        tools: vec![
            bpm::bench::ToolResults {
                tool: "bpm".into(),
                wall_clock_ms: Stats::compute(bpm_values),
                exit_codes: vec![0; number_of_runs],
                bpm_metrics: None,
                network: None,
                network_samples: Vec::new(),
            },
            bpm::bench::ToolResults {
                tool: "pnpm".into(),
                wall_clock_ms: Stats::compute(target_values),
                exit_codes: vec![0; number_of_runs],
                bpm_metrics: None,
                network: None,
                network_samples: Vec::new(),
            },
        ],
    }
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
        (
            "bpm".to_string(),
            format!("bpm {}", env!("CARGO_PKG_VERSION")),
        ),
    ]);
    BenchmarkResult {
        scenario: scenario.to_string(),
        fixture: fixture.to_string(),
        system: sample_system(),
        versions,
        cache_state: "warm".to_string(),
        number_of_runs: 1,
        protocol: None,
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
                bpm_metrics: None,
                network: None,
                network_samples: Vec::new(),
            })
            .collect(),
    }
}
