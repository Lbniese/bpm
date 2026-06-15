//! End-to-end tests for `bpm why`.
//!
//! These tests build a minimal project on disk with a lockfile and verify
//! the output of `bpm why <package>`.

use std::fs;
use std::path::Path;
use std::process::Command;

/// Path to the built `bpm` binary.
fn bpm_binary() -> &'static str {
    env!("CARGO_BIN_EXE_bpm")
}

/// Write a minimal `bpm.lock` at `dir` from a JSON string.
fn write_bpm_lock(dir: &Path, json: &str) {
    fs::write(dir.join("bpm.lock"), json).unwrap();
}

/// Write a minimal `package.json` at `dir`.
fn write_package_json(dir: &Path, deps: &[(&str, &str)]) {
    let dep_map: Vec<String> = deps
        .iter()
        .map(|(name, version)| format!("\"{}\": \"{}\"", name, version))
        .collect();
    let json = format!(
        r#"{{
        "name": "test-project",
        "version": "1.0.0",
        "dependencies": {{ {} }}
    }}"#,
        dep_map.join(", ")
    );
    fs::write(dir.join("package.json"), json).unwrap();
}

/// Run `bpm why` with the given args and return (stdout, stderr, exit_code).
fn run_why(workdir: &Path, target: &str) -> (String, String, Option<i32>) {
    let output = Command::new(bpm_binary())
        .arg("why")
        .arg(target)
        .current_dir(workdir)
        .output()
        .expect("failed to run bpm why");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit_code = output.status.code();
    (stdout, stderr, exit_code)
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Lockfile with a single root dependency.
fn lockfile_single_root_dep() -> String {
    r#"{
        "lockfileVersion": 2,
        "generator": "bpm-test",
        "root": {
            "dependencies": { "lodash": "^4.17.0" }
        },
        "packages": [{
            "path": "node_modules/lodash",
            "name": "lodash",
            "version": "4.17.21",
            "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
            "integrity": "sha512-abc"
        }]
    }"#
    .to_string()
}

/// Lockfile with a root dep `express` that depends on `accepts`,
/// plus `accepts` as a transitive package.
fn lockfile_transitive() -> String {
    r#"{
        "lockfileVersion": 2,
        "generator": "bpm-test",
        "root": {
            "dependencies": { "express": "^4.18.0" }
        },
        "packages": [
            {
                "path": "node_modules/accepts",
                "name": "accepts",
                "version": "1.3.8",
                "resolved": "https://registry.npmjs.org/accepts/-/accepts-1.3.8.tgz",
                "integrity": "sha512-def"
            },
            {
                "path": "node_modules/express",
                "name": "express",
                "version": "4.18.2",
                "resolved": "https://registry.npmjs.org/express/-/express-4.18.2.tgz",
                "integrity": "sha512-abc",
                "dependencies": { "accepts": "^1.3.8" }
            }
        ]
    }"#
    .to_string()
}

/// Lockfile with a target that has multiple parents.
fn lockfile_multi_parent() -> String {
    r#"{
        "lockfileVersion": 2,
        "generator": "bpm-test",
        "root": {
            "dependencies": { "express": "^4.18.0", "connect": "^3.7.0" }
        },
        "packages": [
            {
                "path": "node_modules/accepts",
                "name": "accepts",
                "version": "1.3.8",
                "resolved": "https://registry.npmjs.org/accepts/-/accepts-1.3.8.tgz",
                "integrity": "sha512-def"
            },
            {
                "path": "node_modules/connect",
                "name": "connect",
                "version": "3.7.0",
                "resolved": "https://registry.npmjs.org/connect/-/connect-3.7.0.tgz",
                "integrity": "sha512-ghi",
                "dependencies": { "accepts": "^1.3.7" }
            },
            {
                "path": "node_modules/express",
                "name": "express",
                "version": "4.18.2",
                "resolved": "https://registry.npmjs.org/express/-/express-4.18.2.tgz",
                "integrity": "sha512-abc",
                "dependencies": { "accepts": "^1.3.8" }
            }
        ]
    }"#
    .to_string()
}

/// Lockfile with the same package appearing at multiple version slots.
fn lockfile_multi_version() -> String {
    r#"{
        "lockfileVersion": 2,
        "generator": "bpm-test",
        "root": {
            "dependencies": { "app": "^1.0.0" }
        },
        "packages": [
            {
                "path": "node_modules/app",
                "name": "app",
                "version": "1.0.0",
                "resolved": "https://registry.npmjs.org/app/-/app-1.0.0.tgz",
                "integrity": "sha512-abc",
                "dependencies": { "dep": "^1.0.0", "dep2": "^2.0.0" }
            },
            {
                "path": "node_modules/dep",
                "name": "dep",
                "version": "1.0.0",
                "resolved": "https://registry.npmjs.org/dep/-/dep-1.0.0.tgz",
                "integrity": "sha512-def"
            },
            {
                "path": "node_modules/dep2",
                "name": "dep2",
                "version": "1.0.0",
                "resolved": "https://registry.npmjs.org/dep2/-/dep2-1.0.0.tgz",
                "integrity": "sha512-jkl"
            },
            {
                "path": "node_modules/other/dep",
                "name": "dep",
                "version": "2.0.0",
                "resolved": "https://registry.npmjs.org/dep/-/dep-2.0.0.tgz",
                "integrity": "sha512-ghi",
                "dependencies": { "dep2": "^1.0.0" }
            }
        ]
    }"#
    .to_string()
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn errors_when_no_lockfile() {
    let tmp = tempfile::tempdir().unwrap();
    let (_stdout, stderr, code) = run_why(tmp.path(), "lodash");
    assert_ne!(code, Some(0), "expected non-zero exit code");
    assert!(
        stderr.contains("no lockfile found"),
        "expected 'no lockfile found' error, got: {stderr}"
    );
}

#[test]
fn errors_when_package_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    write_bpm_lock(tmp.path(), &lockfile_single_root_dep());
    write_package_json(tmp.path(), &[("lodash", "^4.17.0")]);

    let (_stdout, stderr, code) = run_why(tmp.path(), "nonexistent");
    assert_ne!(code, Some(0), "expected non-zero exit code");
    assert!(
        stderr.contains("not found in lockfile"),
        "expected 'not found in lockfile' error, got: {stderr}"
    );
}

#[test]
fn shows_root_dependency() {
    let tmp = tempfile::tempdir().unwrap();
    write_bpm_lock(tmp.path(), &lockfile_single_root_dep());
    write_package_json(tmp.path(), &[("lodash", "^4.17.0")]);

    let (stdout, stderr, code) = run_why(tmp.path(), "lodash");
    assert_eq!(code, Some(0), "expected zero exit code; stderr: {stderr}");
    assert!(stdout.contains("lodash@4.17.21"), "expected version line");
    assert!(
        stdout.contains("root: lodash@^4.17.0"),
        "expected root line"
    );
}

#[test]
fn shows_transitive_dependency() {
    let tmp = tempfile::tempdir().unwrap();
    write_bpm_lock(tmp.path(), &lockfile_transitive());
    write_package_json(tmp.path(), &[("express", "^4.18.0")]);

    // `accepts` is a transitive dependency of express.
    let (stdout, stderr, code) = run_why(tmp.path(), "accepts");
    assert_eq!(code, Some(0), "expected zero exit code; stderr: {stderr}");
    assert!(stdout.contains("accepts@1.3.8"), "expected version line");
    assert!(
        stdout.contains("express@4.18.2 requires accepts@^1.3.8"),
        "expected parent line, got: {stdout}"
    );
}

#[test]
fn shows_multiple_parents() {
    let tmp = tempfile::tempdir().unwrap();
    write_bpm_lock(tmp.path(), &lockfile_multi_parent());
    write_package_json(tmp.path(), &[("express", "^4.18.0"), ("connect", "^3.7.0")]);

    // `accepts` is depended on by both express and connect.
    let (stdout, stderr, code) = run_why(tmp.path(), "accepts");
    assert_eq!(code, Some(0), "expected zero exit code; stderr: {stderr}");
    assert!(stdout.contains("accepts@1.3.8"), "expected version line");
    assert!(
        stdout.contains("express@4.18.2 requires accepts@^1.3.8"),
        "expected express parent, got: {stdout}"
    );
    assert!(
        stdout.contains("connect@3.7.0 requires accepts@^1.3.7"),
        "expected connect parent, got: {stdout}"
    );
}

#[test]
fn handles_missing_package_json_gracefully() {
    // `bpm why` only needs a lockfile — package.json is optional.
    let tmp = tempfile::tempdir().unwrap();
    write_bpm_lock(tmp.path(), &lockfile_single_root_dep());
    // No package.json written.

    let (stdout, stderr, code) = run_why(tmp.path(), "lodash");
    assert_eq!(code, Some(0), "expected zero exit code; stderr: {stderr}");
    assert!(stdout.contains("lodash@4.17.21"), "expected version line");
}

#[test]
fn shows_orphaned_package_with_no_parents() {
    // A package in the lockfile with no dependencies pointing to it.
    let json = r#"{
        "lockfileVersion": 2,
        "generator": "bpm-test",
        "root": {},
        "packages": [{
            "path": "node_modules/orphan",
            "name": "orphan",
            "version": "0.0.1",
            "resolved": "",
            "integrity": "sha512-abc"
        }]
    }"#;
    let tmp = tempfile::tempdir().unwrap();
    write_bpm_lock(tmp.path(), json);

    let (stdout, stderr, code) = run_why(tmp.path(), "orphan");
    assert_eq!(code, Some(0), "expected zero exit code; stderr: {stderr}");
    assert!(stdout.contains("orphan@0.0.1"), "expected version line");
    assert!(
        stdout.contains("<no parents"),
        "expected orphan message, got: {stdout}"
    );
}

#[test]
fn shows_multiple_versions_of_same_package() {
    let tmp = tempfile::tempdir().unwrap();
    write_bpm_lock(tmp.path(), &lockfile_multi_version());
    write_package_json(tmp.path(), &[("app", "^1.0.0")]);

    // `dep2` appears at node_modules/dep2. It is depended on by both `app` and `other/dep`.
    let (stdout, stderr, code) = run_why(tmp.path(), "dep2");
    assert_eq!(code, Some(0), "expected zero exit code; stderr: {stderr}");
    assert!(
        stdout.contains("dep2@1.0.0"),
        "expected version line for dep2@1.0.0, got: {stdout}"
    );
}
