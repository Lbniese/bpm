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
fn consume_failure_preserves_manifest_and_absent_lock() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join("store");
    let lib = make_pkg(tmp.path(), "broken-link");
    let app = tmp.path().join("app");
    fs::create_dir_all(&app).unwrap();
    let original_manifest = br#"{"name":"app","version":"1.0.0","custom":true}"#;
    fs::write(app.join("package.json"), original_manifest).unwrap();
    assert_eq!(run(&["link"], &lib, &store).2, Some(0));
    fs::write(lib.join("package.json"), "{ malformed").unwrap();

    let (stdout, _stderr, code) = run(&["link", "broken-link"], &app, &store);
    assert_ne!(code, Some(0));
    assert_eq!(
        fs::read(app.join("package.json")).unwrap(),
        original_manifest
    );
    assert!(!app.join("bpm.lock").exists());
    assert!(!app.join("node_modules/broken-link").exists());
    assert!(!stdout.contains("linked"));
    assert!(!stdout.contains("already linked"));
}

#[test]
fn consume_failure_preserves_existing_manifest_and_lock() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join("store");
    let lib = make_pkg(tmp.path(), "existing-link");
    let app = tmp.path().join("app");
    fs::create_dir_all(&app).unwrap();
    fs::write(
        app.join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();
    assert_eq!(run(&["link"], &lib, &store).2, Some(0));
    assert_eq!(run(&["link", "existing-link"], &app, &store).2, Some(0));
    let manifest_before = fs::read(app.join("package.json")).unwrap();
    let lock_before = fs::read(app.join("bpm.lock")).unwrap();
    fs::write(lib.join("package.json"), "not json").unwrap();

    let (stdout, _, code) = run(&["link", "existing-link"], &app, &store);
    assert_ne!(code, Some(0));
    assert_eq!(fs::read(app.join("package.json")).unwrap(), manifest_before);
    assert_eq!(fs::read(app.join("bpm.lock")).unwrap(), lock_before);
    assert!(!stdout.contains("linked"));
    assert!(!stdout.contains("already linked"));
}

#[test]
fn consume_preserves_npm_v3_lock_authority() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join("store");
    let lib = make_pkg(tmp.path(), "npm-link");
    let app = tmp.path().join("app");
    fs::create_dir_all(&app).unwrap();
    fs::write(
        app.join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();
    fs::write(
        app.join("package-lock.json"),
        r#"{"name":"app","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{"":{"name":"app","version":"1.0.0"}}}"#,
    )
    .unwrap();
    assert_eq!(run(&["link"], &lib, &store).2, Some(0));

    let (_, stderr, code) = run(&["link", "npm-link"], &app, &store);
    assert_eq!(code, Some(0), "consume failed: {stderr}");
    assert!(!app.join("bpm.lock").exists());
    let lock = fs::read_to_string(app.join("package-lock.json")).unwrap();
    assert!(lock.contains("node_modules/npm-link"));
    assert!(lock.contains(r#""link": true"#));

    // The exported npm-v3 link remains directly installable after re-import,
    // rather than working only for the in-memory consume graph.
    let entry = app.join("node_modules/npm-link");
    fs::remove_file(&entry).unwrap();
    let (_, stderr, code) = run(&["install"], &app, &store);
    assert_eq!(code, Some(0), "install from npm lock failed: {stderr}");
    assert!(entry.is_symlink());
}

#[test]
fn unchanged_consume_repairs_missing_materialized_link() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join("store");
    let lib = make_pkg(tmp.path(), "repair-link");
    let app = tmp.path().join("app");
    fs::create_dir_all(&app).unwrap();
    fs::write(
        app.join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();
    assert_eq!(run(&["link"], &lib, &store).2, Some(0));
    assert_eq!(run(&["link", "repair-link"], &app, &store).2, Some(0));
    let entry = app.join("node_modules/repair-link");
    fs::remove_file(&entry).unwrap();

    let (stdout, stderr, code) = run(&["link", "repair-link"], &app, &store);
    assert_eq!(code, Some(0), "repair failed: {stderr}");
    assert!(stdout.contains("already linked"));
    assert!(entry.is_symlink());
}

#[test]
fn repeated_consume_follows_reregistered_target() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join("store");
    let first_parent = tmp.path().join("first");
    let second_parent = tmp.path().join("second");
    fs::create_dir_all(&first_parent).unwrap();
    fs::create_dir_all(&second_parent).unwrap();
    let first = make_pkg(&first_parent, "repoint-link");
    let second = make_pkg(&second_parent, "repoint-link");
    fs::write(second.join("index.js"), "module.exports = 'second';\n").unwrap();
    let app = tmp.path().join("app");
    fs::create_dir_all(&app).unwrap();
    fs::write(
        app.join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();

    assert_eq!(run(&["link"], &first, &store).2, Some(0));
    assert_eq!(run(&["link", "repoint-link"], &app, &store).2, Some(0));
    assert_eq!(run(&["link"], &second, &store).2, Some(0));
    let (_, stderr, code) = run(&["link", "repoint-link"], &app, &store);
    assert_eq!(code, Some(0), "reconsume failed: {stderr}");
    assert!(
        fs::read_to_string(app.join("node_modules/repoint-link/index.js"))
            .unwrap()
            .contains("second")
    );
}

#[test]
fn scoped_link_full_lifecycle_is_isolated() {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join("store");
    let package_parent = tmp.path().join("packages");
    let first = make_pkg(&package_parent, "@scope/first");
    let second = make_pkg(&package_parent, "@scope/second");
    let app = tmp.path().join("app");
    fs::create_dir_all(&app).unwrap();
    fs::write(
        app.join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();

    let (_, stderr, code) = run(&["link"], &first, &store);
    assert_eq!(code, Some(0), "first register failed: {stderr}");
    let (_, stderr, code) = run(&["link"], &second, &store);
    assert_eq!(code, Some(0), "second register failed: {stderr}");
    let global_scope = store.join("links/@scope");
    let first_registration = global_scope.join("first");
    let second_registration = global_scope.join("second");
    assert!(global_scope.is_dir() && !global_scope.is_symlink());
    assert!(first_registration.is_symlink());
    assert!(second_registration.is_symlink());

    let (_, stderr, code) = run(&["link", "@scope/first"], &app, &store);
    assert_eq!(code, Some(0), "first consume failed: {stderr}");
    let (_, stderr, code) = run(&["link", "@scope/second"], &app, &store);
    assert_eq!(code, Some(0), "second consume failed: {stderr}");
    let first_entry = app.join("node_modules/@scope/first");
    let second_entry = app.join("node_modules/@scope/second");
    assert!(first_entry.is_symlink());
    assert!(second_entry.is_symlink());
    assert_eq!(
        fs::canonicalize(&first_entry).unwrap(),
        fs::canonicalize(&first).unwrap()
    );
    let manifest = fs::read_to_string(app.join("package.json")).unwrap();
    assert!(manifest.contains(r#""@scope/first""#));
    assert!(manifest.contains(&first_registration.display().to_string()));
    let lock = fs::read_to_string(app.join("bpm.lock")).unwrap();
    assert!(lock.contains("node_modules/@scope/first"));
    assert!(lock.contains(r#""link": true"#));

    let (_, stderr, code) = run(&["unlink", "@scope/first"], &app, &store);
    assert_eq!(code, Some(0), "first unconsume failed: {stderr}");
    assert!(!first_entry.exists());
    assert!(
        second_entry.is_symlink(),
        "sibling consumer link was removed"
    );

    let (_, stderr, code) = run(&["unlink", "--global"], &first, &store);
    assert_eq!(code, Some(0), "first unregister failed: {stderr}");
    assert!(!first_registration.exists());
    assert!(second_registration.is_symlink());
    assert!(global_scope.is_dir(), "nonempty global scope was removed");

    assert_eq!(run(&["unlink", "@scope/second"], &app, &store).2, Some(0));
    assert_eq!(run(&["unlink", "--global"], &second, &store).2, Some(0));
    assert!(!global_scope.exists(), "empty global scope was not cleaned");
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
