//! End-to-end tests for `bpm init`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Path to the compiled `bpm` binary.
fn bpm_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bpm"))
}

fn run(args: &[&str], cwd: &Path) -> std::process::Output {
    Command::new(bpm_bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run bpm")
}

/// Create a clean-named project directory so the derived package name is
/// deterministic (tempfile dirs begin with `.`, which is sanitized away).
fn clean_project(name: &str) -> PathBuf {
    let outer = tempfile::tempdir().expect("tempdir");
    let project = outer.path().join(name);
    fs::create_dir_all(&project).expect("create project dir");
    // Leak the outer tempdir so the project survives this call; the OS reclaims
    // /tmp eventually. (Tests are short-lived processes.)
    std::mem::forget(outer);
    project
}

#[test]
fn init_yes_writes_package_json_from_directory_name() {
    let project = clean_project("demo-project");
    let out = run(&["init", "-y"], &project);
    assert!(
        out.status.success(),
        "init failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let pkg = project.join("package.json");
    assert!(pkg.exists(), "package.json should be written");
    let content = fs::read_to_string(&pkg).expect("read package.json");
    assert!(
        content.ends_with('\n'),
        "package.json should end with a newline"
    );

    let value: serde_json::Value = serde_json::from_str(&content).expect("valid json");
    assert_eq!(value["name"], "demo-project");
    assert_eq!(value["version"], "1.0.0");
    assert_eq!(value["main"], "index.js");
    assert_eq!(value["license"], "MIT");
    assert!(value["scripts"]["test"]
        .as_str()
        .unwrap()
        .contains("no test specified"));
    assert!(value.get("description").is_none());
    assert!(value.get("author").is_none());
    assert!(value.get("repository").is_none());
}

#[test]
fn init_yes_applies_field_overrides() {
    let project = clean_project("demo-project");
    let out = run(
        &[
            "init",
            "-y",
            "--name",
            "@scope/lib",
            "--version",
            "0.1.0",
            "--license",
            "Apache-2.0",
            "--description",
            "scoped demo",
            "--entry",
            "lib/index.js",
        ],
        &project,
    );
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(project.join("package.json")).unwrap()).unwrap();
    assert_eq!(value["name"], "@scope/lib");
    assert_eq!(value["version"], "0.1.0");
    assert_eq!(value["license"], "Apache-2.0");
    assert_eq!(value["description"], "scoped demo");
    assert_eq!(value["main"], "lib/index.js");
}

#[test]
fn init_refuses_to_overwrite_without_force() {
    let project = clean_project("demo-project");
    fs::write(project.join("package.json"), r#"{"name":"existing"}"#).unwrap();

    let out = run(&["init", "-y"], &project);
    assert!(!out.status.success(), "should refuse to overwrite");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("already exists"), "stderr: {stderr}");
    assert!(stderr.contains("--force"), "stderr: {stderr}");

    // Original file must be untouched.
    assert_eq!(
        fs::read_to_string(project.join("package.json")).unwrap(),
        r#"{"name":"existing"}"#,
    );
}

#[test]
fn init_force_overwrites_existing() {
    let project = clean_project("demo-project");
    fs::write(project.join("package.json"), r#"{"name":"existing"}"#).unwrap();

    let out = run(&["init", "-y", "--force", "--name", "fresh"], &project);
    assert!(
        out.status.success(),
        "init --force failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(project.join("package.json")).unwrap()).unwrap();
    assert_eq!(value["name"], "fresh");
}

#[test]
fn init_rejects_invalid_name_and_writes_nothing() {
    let project = clean_project("demo-project");
    let out = run(&["init", "-y", "--name", "Invalid Name!"], &project);
    assert!(!out.status.success(), "should reject invalid name");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("invalid package name"), "stderr: {stderr}");
    assert!(
        !project.join("package.json").exists(),
        "no file should be written on validation failure",
    );
}

#[test]
fn init_derives_name_from_uppercase_and_spaces_in_directory() {
    let project = clean_project("My Cool App");
    let out = run(&["init", "-y"], &project);
    assert!(out.status.success());
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(project.join("package.json")).unwrap()).unwrap();
    assert_eq!(value["name"], "my-cool-app");
}
