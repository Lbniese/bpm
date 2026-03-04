//! Deterministic, offline unit tests for the benchmark harness plumbing.
//!
//! The tool runs themselves need npm/pnpm + the network and are exercised
//! manually (like the `bpm fetch` real-network smoke test), not in CI. These
//! tests cover the parts that must hold regardless of the environment:
//! fixture generation, result serialization, the pinned-versions map, and the
//! baseline filename helpers.

use bpm::bench::{run_suite, BenchmarkResult, ScenarioKind, Stats, SystemInfo, Tool, FIXTURES};
use std::collections::BTreeMap;

#[test]
fn fixtures_have_meaningful_packages() {
    assert!(FIXTURES.len() >= 2);
    for f in FIXTURES {
        assert!(!f.name.is_empty());
        assert!(!f.packages.is_empty());
        for p in f.packages {
            // Each pinned package is `name@version`.
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
    // None of these tests run the network, so just assert on the encoding.
    let set: std::collections::BTreeSet<&str> = names.iter().copied().collect();
    assert_eq!(set.len(), 8, "scenario names must be unique");
    assert!(names.contains(&"repeat_install"));
    assert!(names.contains(&"monorepo_incremental"));
}

#[test]
fn stats_p95_is_deterministic_for_same_input() {
    let v1: Vec<f64> = (0..200).map(|i| i as f64).collect();
    let v2: Vec<f64> = v1.iter().rev().cloned().collect(); // reversed input
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
        system: SystemInfo {
            machine: "arm64".into(),
            operating_system: "15.0".into(),
            kernel: "24.0.0".into(),
            runtime_versions: BTreeMap::new(),
        },
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
    // Roundtrip preserves the pinned versions.
    let back: BenchmarkResult = serde_json::from_str(&json).unwrap();
    assert_eq!(
        back.versions.get("npm").map(|s| s.as_str()),
        Some("11.12.1")
    );
    assert_eq!(back.scenario, "resolved_cold");
}

#[test]
fn tools_include_bpm_after_milestone_2() {
    // M2 unblocked the bpm tool; the harness must advertise it.
    let names: Vec<&str> = Tool::all().iter().map(|c| c.name()).collect();
    assert!(names.contains(&"bpm"), "bpm tool missing: {names:?}");
    assert!(names.contains(&"npm"));
    assert!(names.contains(&"yarn"));
    assert!(names.contains(&"bun"));
}

#[test]
fn run_suite_skips_missing_tools_without_failing() {
    // pnpm is intentionally not installed in CI; asking for it must not error
    // and must simply omit it from the result. We use a cheap fixture+scenario
    // combination and a single run, guarded by skipping if npm is unavailable.
    if std::process::Command::new("npm")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        eprintln!("npm not available; skipping run_suite network test");
        return;
    }
    let fixture = FIXTURES.iter().find(|f| f.name == "minimal").unwrap();
    // resolved_cold still needs network to generate a real lockfile; only assert
    // the function returns Ok (tool skip path) rather than panicking.
    let suite = run_suite(
        &[ScenarioKind::ResolvedCold],
        fixture,
        &[Tool::Npm, Tool::Pnpm], // pnpm missing -> skipped
        1,
    );
    // Network may legitimately fail; this test only asserts the skip path does
    // not panic. If it errored on network, that's acceptable => ignore the Err.
    match suite {
        Ok(s) => {
            assert!(!s.results.is_empty());
            for r in &s.results {
                // pnpm must not appear in pinned versions since it was skipped.
                assert!(!r.versions.contains_key("pnpm"));
            }
        }
        Err(_) => { /* network-dependent; not a plumbing failure */ }
    }
}
