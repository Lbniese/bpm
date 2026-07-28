//! End-to-end tests for `bpm link` / `bpm unlink` (global two-step developer
//! linking). Unix-only: the feature is symlink-based.
//!
//! Flow under test:
//!   1. `bpm link` (in lib dir)      -> registers `$STORE/links/<name>` -> lib
//!   2. `bpm link <name>` (in app)   -> adds `file:` dep, installs, materializes
//!      `node_modules/<name>` -> lib
//!   3. `bpm unlink <name>` (in app) -> removes the dep and reinstalls
//!   4. `bpm unlink --global` (in lib) -> unregisters

#![cfg(unix)]

use std::fs;
use std::process::Command;

use tempfile::TempDir;

fn bpm_bin() -> &'static str {
    env!("CARGO_BIN_EXE_bpm")
}

/// Run `bpm` with `args` in `cwd`, using `store` as the store root.
fn run(
    args: &[&str],
    cwd: &std::path::Path,
    store: &std::path::Path,
) -> (String, String, Option<i32>) {
    let mut full: Vec<&str> = args.to_vec();
    full.push("--store");
    full.push(store.to_str().unwrap());
    let out = Command::new(bpm_bin())
        .args(&full)
        .current_dir(cwd)
        .output()
        .expect("run bpm");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
}

/// Create a package dir with a package.json and an index.js.
fn make_pkg(parent: &std::path::Path, name: &str) -> std::path::PathBuf {
    let dir = parent.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("package.json"),
        format!(r#"{{"name":"{name}","version":"1.0.0"}}"#),
    )
    .unwrap();
    fs::write(
        dir.join("index.js"),
        format!("module.exports = '{name}';\n"),
    )
    .unwrap();
    dir
}

#[test]
fn register_creates_global_symlink() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join("store");
    let lib = make_pkg(tmp.path(), "mylib");

    let (stdout, stderr, code) = run(&["link"], &lib, &store);
    assert_eq!(
        code,
        Some(0),
        "register failed\nstdout:{stdout}\nstderr:{stderr}"
    );

    let link = store.join("links").join("mylib");
    assert!(link.is_symlink(), "{} should be a symlink", link.display());
    let target = fs::canonicalize(&link).unwrap();
    assert_eq!(target, fs::canonicalize(&lib).unwrap());
}

#[test]
fn consume_materializes_node_modules_link() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join("store");
    let lib = make_pkg(tmp.path(), "mylib");
    let app = tmp.path().join("app");
    fs::create_dir_all(&app).unwrap();
    fs::write(
        app.join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();

    // Register the library.
    let (_, _, code) = run(&["link"], &lib, &store);
    assert_eq!(code, Some(0), "register should succeed");

    // Consume it into the app project.
    let (stdout, stderr, code) = run(&["link", "mylib"], &app, &store);
    assert_eq!(
        code,
        Some(0),
        "consume failed\nstdout:{stdout}\nstderr:{stderr}"
    );

    // node_modules/mylib must be a symlink whose content is the lib's.
    let nm = app.join("node_modules").join("mylib");
    assert!(nm.is_symlink(), "{} should be a symlink", nm.display());
    let index = fs::read_to_string(nm.join("index.js")).unwrap();
    assert!(index.contains("mylib"), "linked content mismatch: {index}");

    // package.json records the file: dependency pointing at the registry.
    let pkg = fs::read_to_string(app.join("package.json")).unwrap();
    assert!(
        pkg.contains("\"mylib\"") && pkg.contains("file:"),
        "package.json missing link dep: {pkg}"
    );

    // bpm.lock records a link entry.
    let lock = fs::read_to_string(app.join("bpm.lock")).unwrap();
    assert!(
        lock.contains("\"mylib\"") && lock.contains("\"link\": true"),
        "bpm.lock missing link entry: {lock}"
    );
}

#[test]
fn unconsume_removes_link() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join("store");
    let lib = make_pkg(tmp.path(), "mylib");
    let app = tmp.path().join("app");
    fs::create_dir_all(&app).unwrap();
    fs::write(
        app.join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();

    run(&["link"], &lib, &store);
    let (_, stderr, code) = run(&["link", "mylib"], &app, &store);
    assert_eq!(code, Some(0), "consume should succeed; stderr:{stderr}");

    let (stdout, stderr, code) = run(&["unlink", "mylib"], &app, &store);
    assert_eq!(
        code,
        Some(0),
        "unconsume failed\nstdout:{stdout}\nstderr:{stderr}"
    );

    let nm = app.join("node_modules").join("mylib");
    assert!(
        !nm.exists(),
        "node_modules/mylib should be gone after unlink"
    );
    let pkg = fs::read_to_string(app.join("package.json")).unwrap();
    assert!(
        !pkg.contains("\"mylib\""),
        "package.json still references mylib: {pkg}"
    );
}

#[test]
fn unregister_removes_global_link() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join("store");
    let lib = make_pkg(tmp.path(), "mylib");

    run(&["link"], &lib, &store);
    let link = store.join("links").join("mylib");
    assert!(link.is_symlink());

    let (_, _, code) = run(&["unlink", "--global"], &lib, &store);
    assert_eq!(code, Some(0), "unregister should succeed");
    assert!(!link.exists(), "global link should be removed");
}

#[test]
fn consume_errors_when_not_registered() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join("store");
    let app = tmp.path().join("app");
    fs::create_dir_all(&app).unwrap();
    fs::write(
        app.join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();

    let (_stdout, stderr, code) = run(&["link", "nope"], &app, &store);
    assert_ne!(code, Some(0), "expected non-zero exit");
    assert!(
        stderr.contains("no global link named 'nope'"),
        "expected not-registered error, got: {stderr}"
    );
}
