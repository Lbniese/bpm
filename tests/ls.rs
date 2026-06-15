//! End-to-end tests for `bpm ls` (dependency-tree listing).
//!
//! Each test writes a `bpm.lock` to a temp dir and spawns the built `bpm`
//! binary, then asserts on the rendered tree / JSON / exit behavior.

use std::fs;
use std::path::Path;
use std::process::Command;

/// Path to the built `bpm` binary.
fn bpm_binary() -> &'static str {
    env!("CARGO_BIN_EXE_bpm")
}

/// Write a `bpm.lock` at `dir` from a JSON string.
fn write_lock(dir: &Path, json: &str) {
    fs::write(dir.join("bpm.lock"), json).unwrap();
}

/// Run `bpm ls [args]` in `workdir`, returning (stdout, stderr, exit_code).
fn run_ls(workdir: &Path, args: &[&str]) -> (String, String, Option<i32>) {
    let mut cmd = Command::new(bpm_binary());
    cmd.arg("ls").args(args).current_dir(workdir);
    let output = cmd.output().expect("failed to run bpm ls");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code(),
    )
}

fn count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

// ── Fixtures ────────────────────────────────────────────────────────────────

/// Single root dependency.
fn lock_single() -> String {
    r#"{
        "lockfileVersion": 2,
        "generator": "bpm-test",
        "root": { "name": "test-project", "version": "1.0.0", "dependencies": { "lodash": "^4.17.0" } },
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

/// `express` depends transitively on `accepts`.
fn lock_transitive() -> String {
    r#"{
        "lockfileVersion": 2,
        "generator": "bpm-test",
        "root": { "name": "test-project", "version": "1.0.0", "dependencies": { "express": "^4.18.0" } },
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

/// Two root deps; only `express` reaches `accepts` (for filter pruning).
fn lock_two_roots() -> String {
    r#"{
        "lockfileVersion": 2,
        "generator": "bpm-test",
        "root": { "name": "test-project", "version": "1.0.0", "dependencies": { "express": "^4.18.0", "lodash": "^4.17.0" } },
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
            },
            {
                "path": "node_modules/lodash",
                "name": "lodash",
                "version": "4.17.21",
                "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
                "integrity": "sha512-ghi"
            }
        ]
    }"#
    .to_string()
}

/// Diamond: both `connect` and `express` depend on `accepts`, which itself
/// depends on `mime-types`. Used to exercise dedup vs. `--all`.
fn lock_diamond() -> String {
    r#"{
        "lockfileVersion": 2,
        "generator": "bpm-test",
        "root": { "name": "test-project", "version": "1.0.0", "dependencies": { "express": "^4.18.0", "connect": "^3.7.0" } },
        "packages": [
            {
                "path": "node_modules/accepts",
                "name": "accepts",
                "version": "1.3.8",
                "resolved": "https://registry.npmjs.org/accepts/-/accepts-1.3.8.tgz",
                "integrity": "sha512-def",
                "dependencies": { "mime-types": "^1.0.0" }
            },
            {
                "path": "node_modules/connect",
                "name": "connect",
                "version": "3.7.0",
                "resolved": "https://registry.npmjs.org/connect/-/connect-3.7.0.tgz",
                "integrity": "sha512-jkl",
                "dependencies": { "accepts": "^1.3.7" }
            },
            {
                "path": "node_modules/express",
                "name": "express",
                "version": "4.18.2",
                "resolved": "https://registry.npmjs.org/express/-/express-4.18.2.tgz",
                "integrity": "sha512-abc",
                "dependencies": { "accepts": "^1.3.8" }
            },
            {
                "path": "node_modules/mime-types",
                "name": "mime-types",
                "version": "2.1.35",
                "resolved": "https://registry.npmjs.org/mime-types/-/mime-types-2.1.35.tgz",
                "integrity": "sha512-mno"
            }
        ]
    }"#
    .to_string()
}

/// A cycle: `a` depends on `b`, `b` depends on `a`.
fn lock_cycle() -> String {
    r#"{
        "lockfileVersion": 2,
        "generator": "bpm-test",
        "root": { "name": "test-project", "version": "1.0.0", "dependencies": { "a": "^1.0.0" } },
        "packages": [
            {
                "path": "node_modules/a",
                "name": "a",
                "version": "1.0.0",
                "resolved": "https://registry.npmjs.org/a/-/a-1.0.0.tgz",
                "integrity": "sha512-aaa",
                "dependencies": { "b": "^1.0.0" }
            },
            {
                "path": "node_modules/b",
                "name": "b",
                "version": "1.0.0",
                "resolved": "https://registry.npmjs.org/b/-/b-1.0.0.tgz",
                "integrity": "sha512-bbb",
                "dependencies": { "a": "^1.0.0" }
            }
        ]
    }"#
    .to_string()
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn errors_when_no_lockfile() {
    let tmp = tempfile::tempdir().unwrap();
    let (_stdout, stderr, code) = run_ls(tmp.path(), &[]);
    assert_ne!(code, Some(0), "expected non-zero exit code");
    assert!(
        stderr.contains("no lockfile found"),
        "expected 'no lockfile found', got: {stderr}"
    );
}

#[test]
fn renders_root_and_single_dependency() {
    let tmp = tempfile::tempdir().unwrap();
    write_lock(tmp.path(), &lock_single());

    let (stdout, stderr, code) = run_ls(tmp.path(), &[]);
    assert_eq!(code, Some(0), "expected zero exit code; stderr: {stderr}");
    assert!(
        stdout.contains("test-project@1.0.0"),
        "expected root label, got: {stdout}"
    );
    assert!(
        stdout.contains("└── lodash@4.17.21"),
        "expected leaf line, got: {stdout}"
    );
}

#[test]
fn list_alias_works() {
    let tmp = tempfile::tempdir().unwrap();
    write_lock(tmp.path(), &lock_single());

    let (stdout, stderr, code) = run_ls_replace_first(tmp.path(), "list", &[]);
    assert_eq!(code, Some(0), "expected zero exit code; stderr: {stderr}");
    assert!(stdout.contains("lodash@4.17.21"), "alias produced no tree");
}

#[test]
fn renders_nested_transitive_tree() {
    let tmp = tempfile::tempdir().unwrap();
    write_lock(tmp.path(), &lock_transitive());

    let (stdout, stderr, code) = run_ls(tmp.path(), &[]);
    assert_eq!(code, Some(0), "expected zero exit code; stderr: {stderr}");
    assert!(
        stdout.contains("└── express@4.18.2"),
        "expected express under root, got: {stdout}"
    );
    assert!(
        stdout.contains("    └── accepts@1.3.8"),
        "expected indented accepts under express, got: {stdout}"
    );
}

#[test]
fn deduplicates_shared_packages_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    write_lock(tmp.path(), &lock_diamond());

    let (stdout, stderr, code) = run_ls(tmp.path(), &[]);
    assert_eq!(code, Some(0), "expected zero exit code; stderr: {stderr}");
    // `mime-types` is reached via both `connect` and `express`; by default it
    // is expanded only once (under whichever parent is visited first).
    assert_eq!(
        count(&stdout, "mime-types@2.1.35"),
        1,
        "expected mime-types expanded once, got: {stdout}"
    );
}

#[test]
fn all_flag_expands_every_occurrence() {
    let tmp = tempfile::tempdir().unwrap();
    write_lock(tmp.path(), &lock_diamond());

    let (stdout, stderr, code) = run_ls(tmp.path(), &["--all"]);
    assert_eq!(code, Some(0), "expected zero exit code; stderr: {stderr}");
    assert_eq!(
        count(&stdout, "mime-types@2.1.35"),
        2,
        "expected mime-types expanded twice under --all, got: {stdout}"
    );
}

#[test]
fn depth_zero_hides_transitives() {
    let tmp = tempfile::tempdir().unwrap();
    write_lock(tmp.path(), &lock_transitive());

    let (stdout, stderr, code) = run_ls(tmp.path(), &["--depth", "0"]);
    assert_eq!(code, Some(0), "expected zero exit code; stderr: {stderr}");
    assert!(
        stdout.contains("express@4.18.2"),
        "expected direct dep at depth 0, got: {stdout}"
    );
    assert!(
        !stdout.contains("accepts@1.3.8"),
        "expected transitive hidden at depth 0, got: {stdout}"
    );
}

#[test]
fn filter_prunes_to_matching_paths() {
    let tmp = tempfile::tempdir().unwrap();
    write_lock(tmp.path(), &lock_two_roots());

    let (stdout, stderr, code) = run_ls(tmp.path(), &["accepts"]);
    assert_eq!(code, Some(0), "expected zero exit code; stderr: {stderr}");
    assert!(
        stdout.contains("accepts@1.3.8"),
        "expected the matched package, got: {stdout}"
    );
    assert!(
        stdout.contains("express@4.18.2"),
        "expected the path to the match, got: {stdout}"
    );
    assert!(
        !stdout.contains("lodash@4.17.21"),
        "expected unrelated branch pruned, got: {stdout}"
    );
}

#[test]
fn filter_with_no_match_errors() {
    let tmp = tempfile::tempdir().unwrap();
    write_lock(tmp.path(), &lock_single());

    let (_stdout, stderr, code) = run_ls(tmp.path(), &["nonexistent"]);
    assert_ne!(code, Some(0), "expected non-zero exit code");
    assert!(
        stderr.contains("no package matching"),
        "expected no-match error, got: {stderr}"
    );
}

#[test]
fn cycle_does_not_loop_forever() {
    let tmp = tempfile::tempdir().unwrap();
    write_lock(tmp.path(), &lock_cycle());

    let (stdout, stderr, code) = run_ls(tmp.path(), &[]);
    assert_eq!(code, Some(0), "expected zero exit code; stderr: {stderr}");
    assert!(
        stdout.contains("a@1.0.0"),
        "expected cycle member a, got: {stdout}"
    );
    assert!(
        stdout.contains("b@1.0.0"),
        "expected cycle member b, got: {stdout}"
    );
}

#[test]
fn json_output_is_structured() {
    let tmp = tempfile::tempdir().unwrap();
    write_lock(tmp.path(), &lock_transitive());

    let (stdout, stderr, code) = run_ls(tmp.path(), &["--json"]);
    assert_eq!(code, Some(0), "expected zero exit code; stderr: {stderr}");

    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("invalid JSON: {e}; stdout: {stdout}"));
    assert_eq!(value["name"], "test-project");
    assert_eq!(value["version"], "1.0.0");
    assert_eq!(value["dependencies"]["express"]["version"], "4.18.2");
    assert_eq!(
        value["dependencies"]["express"]["dependencies"]["accepts"]["version"],
        "1.3.8"
    );
}

/// Variant of `run_ls` that lets the caller swap the subcommand (e.g. the
/// `list` alias) while keeping the rest of the args.
fn run_ls_replace_first(
    workdir: &Path,
    subcommand: &str,
    args: &[&str],
) -> (String, String, Option<i32>) {
    let mut cmd = Command::new(bpm_binary());
    cmd.arg(subcommand).args(args).current_dir(workdir);
    let output = cmd.output().expect("failed to run bpm ls");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code(),
    )
}
