//! End-to-end tests for `bpm doctor`.
//!
//! These build a minimal project on disk, run the doctor inspection, and assert
//! on the structured [`DoctorReport`] (including deterministic JSON output that
//! must be byte-identical across runs for the same inputs).

use std::fs;
use std::path::Path;

use bpm::doctor::{run, DoctorReport};

use tempfile::tempdir;

fn write(dir: &Path, name: &str, contents: &str) {
    fs::write(dir.join(name), contents).unwrap();
}

fn codes(report: &DoctorReport) -> Vec<String> {
    report
        .diagnostics
        .iter()
        .map(|d| d.code.to_string())
        .collect()
}

#[test]
fn reports_manifest_not_found() {
    let tmp = tempdir().unwrap();
    let report = run(tmp.path());
    assert!(report.has_error());
    assert!(codes(&report).contains(&"MANIFEST_NOT_FOUND".to_string()));
    assert!(!report.manifest_found);
    assert!(report.project_root.is_none());
}

#[test]
fn reports_clean_manifest_with_declared_dependencies() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "package.json",
        r#"{"name":"app","version":"1.0.0","dependencies":{"react":"^18.0.0"},
        "scripts":{"build":"tsc"}}"#,
    );

    let report = run(root);
    assert!(!report.has_error());
    assert_eq!(report.manifest.name.as_deref(), Some("app"));
    assert_eq!(report.manifest.declared_dependencies, 1);
    assert!(codes(&report).contains(&"DECLARED_DEPENDENCIES".to_string()));
    assert!(codes(&report).contains(&"LIFECYCLE_SCRIPTS".to_string()));
    assert!(!codes(&report).contains(&"MANIFEST_NAME_MISSING".to_string()));
}

#[test]
fn flags_missing_name_and_version() {
    let tmp = tempdir().unwrap();
    write(
        tmp.path(),
        "package.json",
        r#"{"dependencies":{"x":"^1.0.0"}}"#,
    );
    let report = run(tmp.path());
    let c = codes(&report);
    assert!(c.contains(&"MANIFEST_NAME_MISSING".to_string()));
    assert!(c.contains(&"MANIFEST_VERSION_MISSING".to_string()));
    // Missing name/version are warnings here: not actionable blockers for a
    // workspace root, so exit stays clean.
    assert!(!report.has_error());
}

#[test]
fn flags_invalid_name_as_error() {
    let tmp = tempdir().unwrap();
    write(
        tmp.path(),
        "package.json",
        r#"{"name":"Bad Name","version":"1.0.0"}"#,
    );
    let report = run(tmp.path());
    let c = codes(&report);
    assert!(c.contains(&"MANIFEST_NAME_INVALID".to_string()));
    assert!(report.has_error());
}

#[test]
fn detects_native_addon_via_binding_gyp() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "package.json",
        r#"{"name":"native","version":"1.0.0"}"#,
    );
    write(root, "binding.gyp", r#"{}"#);
    let report = run(root);
    assert!(codes(&report).contains(&"NATIVE_ADDON".to_string()));
}

#[test]
fn detects_native_addon_via_known_builder() {
    let tmp = tempdir().unwrap();
    write(
        tmp.path(),
        "package.json",
        r#"{"name":"native","version":"1.0.0","devDependencies":{"node-gyp":"^10.0.0"}}"#,
    );
    let report = run(tmp.path());
    assert!(codes(&report).contains(&"NATIVE_ADDON".to_string()));
}

#[test]
fn reports_workspaces_overrides_and_engines() {
    let tmp = tempdir().unwrap();
    write(
        tmp.path(),
        "package.json",
        r#"{"name":"root","version":"1.0.0","workspaces":["packages/*"],
        "overrides":{"lodash":"^4.0.0"},"engines":{"node":">=20"}}"#,
    );
    let report = run(tmp.path());
    let c = codes(&report);
    assert!(c.contains(&"WORKSPACES_UNSUPPORTED".to_string()));
    assert!(c.contains(&"OVERRIDES_DECLARED".to_string()));
    assert!(c.contains(&"ENGINES_NODE".to_string()));
    assert_eq!(report.manifest.workspaces, 1);
    assert_eq!(report.manifest.overrides, 1);
    assert_eq!(report.manifest.engines_node.as_deref(), Some(">=20"));
}

#[test]
fn reports_peer_override_and_optional_dependency_diagnostics_together() {
    let tmp = tempdir().unwrap();
    write(
        tmp.path(),
        "package.json",
        r#"{
            "name":"diagnostic-fixture",
            "version":"1.0.0",
            "peerDependencies":{"react":"^18.0.0"},
            "optionalDependencies":{"fsevents":"^2.3.3"},
            "overrides":{"semver":"7.7.2"}
        }"#,
    );

    let report = run(tmp.path());
    let actual = codes(&report);

    assert_eq!(report.manifest.declared_dependencies, 2);
    assert_eq!(report.manifest.overrides, 1);
    assert_eq!(
        actual,
        vec!["DECLARED_DEPENDENCIES", "OVERRIDES_DECLARED"],
        "peer and optional dependencies must contribute to the dependency diagnostic while overrides are noted as honored"
    );
}

#[test]
fn malformed_manifest_is_error_diagnostic() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write(root, "package.json", "{ \"name\": ");
    let report = run(root);
    assert!(report.manifest_found);
    assert!(report.has_error());
    assert!(codes(&report).contains(&"MANIFEST_PARSE".to_string()));
}

#[test]
fn json_output_is_byte_stable_across_runs() {
    // Identical inputs must produce identical bytes, independent of map iteration order.
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "package.json",
        r#"{"name":"app","version":"1.0.0","dependencies":{"z":"^1","a":"^2","m":"^3"},
        "devDependencies":{"d":"^1"},"scripts":{"build":"tsc","test":"jest"}}"#,
    );

    let first = run(root).render_json();
    let second = run(root).render_json();
    assert_eq!(first, second, "JSON output is not byte-stable");

    // Diagnostics must be sorted by stable code, not insertion order.
    let report = run(root);
    let code_seq: Vec<&str> = report.diagnostics.iter().map(|d| d.code).collect();
    let mut sorted = code_seq.clone();
    sorted.sort();
    assert_eq!(code_seq, sorted, "diagnostics not in stable order");
}

#[test]
fn text_output_contains_version_and_roots() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".git")).unwrap();
    write(root, "package.json", r#"{"name":"app","version":"1.0.0"}"#);
    let report = run(root);
    let text = report.render_text();
    assert!(text.contains("bpm 0.0.1"));
    assert!(text.contains("repository root:"));
    assert!(text.contains("project root:"));
}

#[test]
fn diagnostics_use_nonempty_severity_labels() {
    let tmp = tempdir().unwrap();
    let report = run(tmp.path());
    for d in &report.diagnostics {
        assert!(!d.severity.as_str().is_empty());
    }
}
