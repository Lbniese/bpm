//! Offline, deterministic integration test for `bpm install --frozen`.
//!
//! Drives the real `bpm` binary as a subprocess (the install orchestration lives
//! in `src/main.rs::run_install`). Tiny npm-style tarballs are built in-memory
//! and referenced via `file://` URLs, so the test needs no network and no local
//! registry server. It asserts the materialized `node_modules` layout, bin
//! linking, idempotent re-install, and the `--frozen` guard.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use bpm::integrity::{Integrity, Sha512Digest};
use bpm::lockfile::{Lockfile, PackageEntry, RootEntry};
use flate2::write::GzEncoder;
use flate2::Compression;
use tempfile::tempdir;

/// The path to the compiled `bpm` binary for this crate.
fn bpm_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bpm"))
}

/// Build a gzip+tar archive in npm layout (entries under `package/`).
fn build_tgz(files: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let mut buf = Vec::new();
    let enc = GzEncoder::new(&mut buf, Compression::none());
    let mut builder = tar::Builder::new(enc);
    for (path, data, mode) in files {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_size(data.len() as u64);
        header.set_mode(*mode);
        header.set_cksum();
        builder.append(&header, &data[..]).unwrap();
    }
    let enc = builder.into_inner().unwrap();
    enc.finish().unwrap();
    buf
}

/// Write a tarball into `dir/name`, returning its path and integrity.
fn seed_tarball(dir: &Path, name: &str, bytes: &[u8]) -> (PathBuf, Integrity) {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    let integrity = Integrity::sha512(Sha512Digest::hash_bytes(bytes));
    (path, integrity)
}

fn assert_resolves(p: &Path) {
    let meta = fs::symlink_metadata(p).expect("path missing");
    assert!(
        meta.file_type().is_symlink(),
        "not a symlink: {}",
        p.display()
    );
    assert!(Path::new(p).exists(), "dangling symlink: {}", p.display());
}

fn is_executable(p: &Path) -> bool {
    fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Set up a project with two packages (one top-level with a bin, one nested)
/// and return the project + store + tarball temp roots. The tarball dir MUST
/// outlive the spawned `bpm` process (the lockfile points at `file://` URLs into
/// it), so all three TempDirs are returned to the caller to hold.
fn setup_project() -> (tempfile::TempDir, tempfile::TempDir, tempfile::TempDir) {
    let project = tempdir().unwrap();
    let store = tempdir().unwrap();
    let tgz = tempdir().unwrap();

    let (greet_path, greet_int) = seed_tarball(
        tgz.path(),
        "greet.tgz",
        &build_tgz(&[
            (
                "package/package.json",
                b"{\"name\":\"greet\",\"version\":\"1.0.0\"}",
                0o644,
            ),
            (
                "package/cli.js",
                b"#!/usr/bin/env node\nconsole.log('hello');\n",
                0o755,
            ),
        ]),
    );
    let (dep_path, dep_int) = seed_tarball(
        tgz.path(),
        "dep.tgz",
        &build_tgz(&[("package/package.json", b"{\"name\":\"dep\"}", 0o644)]),
    );

    // Manifest that agrees with the lockfile root (frozen guard must pass).
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"greet":"^1.0.0"}}"#,
    )
    .unwrap();

    let mut lf = Lockfile::new("bpm-test");
    lf.root = RootEntry {
        name: Some("app".into()),
        version: Some("1.0.0".into()),
        dependencies: BTreeMap::from([("greet".into(), "^1.0.0".into())]),
    };
    lf.packages.push(PackageEntry {
        path: "node_modules/greet".into(),
        name: "greet".into(),
        version: "1.0.0".into(),
        resolved: format!("file://{}", greet_path.display()),
        integrity: Some(greet_int.to_npm_string()),
        bin: BTreeMap::from([("hello".into(), "./cli.js".into())]),
        ..Default::default()
    });
    lf.packages.push(PackageEntry {
        path: "node_modules/greet/node_modules/dep".into(),
        name: "dep".into(),
        version: "1.0.0".into(),
        resolved: format!("file://{}", dep_path.display()),
        integrity: Some(dep_int.to_npm_string()),
        ..Default::default()
    });
    lf.sort_packages();
    lf.write_to(&project.path().join("bpm.lock")).unwrap();

    (project, store, tgz)
}

fn setup_package_lock_project() -> (tempfile::TempDir, tempfile::TempDir, tempfile::TempDir) {
    let project = tempdir().unwrap();
    let store = tempdir().unwrap();
    let tgz = tempdir().unwrap();

    let (greet_path, greet_int) = seed_tarball(
        tgz.path(),
        "greet.tgz",
        &build_tgz(&[
            (
                "package/package.json",
                b"{\"name\":\"greet\",\"version\":\"1.0.0\"}",
                0o644,
            ),
            (
                "package/cli.js",
                b"#!/usr/bin/env node\nconsole.log('hello');\n",
                0o755,
            ),
        ]),
    );
    let (dep_path, dep_int) = seed_tarball(
        tgz.path(),
        "dep.tgz",
        &build_tgz(&[("package/package.json", b"{\"name\":\"dep\"}", 0o644)]),
    );

    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"greet":"^1.0.0"}}"#,
    )
    .unwrap();
    fs::write(
        project.path().join("package-lock.json"),
        format!(
            r#"{{
  "name": "app",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "packages": {{
    "": {{ "name": "app", "version": "1.0.0", "dependencies": {{ "greet": "^1.0.0" }} }},
    "node_modules/greet": {{
      "version": "1.0.0",
      "resolved": "file://{}",
      "integrity": "{}",
      "bin": {{ "hello": "./cli.js" }},
      "dependencies": {{ "dep": "^1.0.0" }}
    }},
    "node_modules/greet/node_modules/dep": {{
      "version": "1.0.0",
      "resolved": "file://{}",
      "integrity": "{}"
    }}
  }}
}}"#,
            greet_path.display(),
            greet_int.to_npm_string(),
            dep_path.display(),
            dep_int.to_npm_string()
        ),
    )
    .unwrap();

    (project, store, tgz)
}

fn run_install(project: &Path, store: &Path) -> std::process::Output {
    Command::new(bpm_bin())
        .arg("install")
        .arg("--frozen")
        .arg("--store")
        .arg(store)
        .current_dir(project)
        .output()
        .expect("failed to run bpm")
}

fn run_plain_install(project: &Path, store: &Path) -> std::process::Output {
    Command::new(bpm_bin())
        .arg("install")
        .arg("--store")
        .arg(store)
        .current_dir(project)
        .output()
        .expect("failed to run bpm")
}

fn run_ci(project: &Path, store: &Path) -> std::process::Output {
    Command::new(bpm_bin())
        .arg("ci")
        .arg("--store")
        .arg(store)
        .current_dir(project)
        .output()
        .expect("failed to run bpm ci")
}

#[test]
fn frozen_install_materializes_node_modules_and_bins() {
    let (project, store, _tgz) = setup_project();
    let out = run_install(project.path(), store.path());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "install failed: {stderr}");
    assert!(
        stdout.contains("installed 2 package(s)"),
        "stdout: {stdout}"
    );
    // The project attaches to a reusable graph volume via shallow relays.
    assert!(stdout.contains("graph volume built"), "stdout: {stdout}");

    let nm = project.path().join("node_modules");
    // Top-level relay: a symlink into the shared graph volume.
    assert_resolves(&nm.join("greet"));
    // Nested packages live inside the volume as hardlinked real files/dirs
    // (not symlinks), reached transitively through the top-level relay. Bins
    // intentionally remain relative symlinks so Node preserves package-relative
    // resolution when launching a CLI.
    assert!(nm.join("greet/node_modules/dep").exists());
    assert!(nm.join("greet/package.json").exists());
    assert!(nm.join("greet/node_modules/dep/package.json").exists());

    // The volume entry itself (the relay target) must be a REAL directory, not
    // a symlink: hardlink materialization keeps a package's realpath inside the
    // volume so self-referential requires resolve.
    let volume_entry = fs::read_link(nm.join("greet")).unwrap();
    let volume_meta = fs::symlink_metadata(&volume_entry).unwrap();
    assert!(
        volume_meta.is_dir(),
        "volume entry should be a real directory, not a symlink: {}",
        volume_entry.display()
    );
    assert!(!volume_meta.file_type().is_symlink());

    let bin = nm.join(".bin").join("hello");
    assert!(bin.exists(), "bin must be reachable through the relay");
    assert!(is_executable(&bin), "bin must keep its executable bit");
    assert_eq!(
        fs::read_link(&bin).unwrap(),
        PathBuf::from("../greet/cli.js"),
        "volume bins must preserve package-relative resolution",
    );
}

#[test]
fn installs_directly_from_package_lock_without_writing_bpm_lock() {
    let (project, store, _tgz) = setup_package_lock_project();
    let lock_path = project.path().join("package-lock.json");
    let before = fs::read(&lock_path).unwrap();

    let out = run_plain_install(project.path(), store.path());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "install failed: {stderr}");
    assert!(
        stdout.contains("installed 2 package(s)"),
        "stdout: {stdout}"
    );
    assert!(project
        .path()
        .join("node_modules/greet/package.json")
        .exists());
    assert!(project
        .path()
        .join("node_modules/greet/node_modules/dep/package.json")
        .exists());
    assert!(project.path().join("node_modules/.bin/hello").exists());
    assert!(project.path().join(".bpm-state").exists());
    assert!(!project.path().join("bpm.lock").exists());
    assert_eq!(fs::read(&lock_path).unwrap(), before);

    let second = run_plain_install(project.path(), store.path());
    assert!(second.status.success());
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("nothing to install"),
        "stdout: {}",
        String::from_utf8_lossy(&second.stdout)
    );
}

#[test]
fn ci_uses_package_lock_and_reports_package_lock_drift() {
    let (project, store, _tgz) = setup_package_lock_project();

    let ci = run_ci(project.path(), store.path());
    assert!(
        ci.status.success(),
        "ci failed: {}",
        String::from_utf8_lossy(&ci.stderr)
    );
    assert!(!project.path().join("bpm.lock").exists());

    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","dependencies":{"greet":"^1.0.0","extra":"1.0.0"}}"#,
    )
    .unwrap();
    let drift = run_ci(project.path(), store.path());
    assert!(!drift.status.success());
    let stderr = String::from_utf8_lossy(&drift.stderr);
    assert!(stderr.contains("package-lock.json"), "{stderr}");
    assert!(stderr.contains("extra"), "{stderr}");
}

#[test]
fn package_lock_v1_v2_and_blocking_diagnostics_are_rejected() {
    for version in [1, 2] {
        let project = tempdir().unwrap();
        let store = tempdir().unwrap();
        fs::write(project.path().join("package.json"), r#"{"name":"app"}"#).unwrap();
        fs::write(
            project.path().join("package-lock.json"),
            format!(r#"{{"lockfileVersion":{version},"packages":{{}}}}"#),
        )
        .unwrap();

        let out = run_plain_install(project.path(), store.path());
        assert!(!out.status.success(), "v{version} should fail");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(&format!("unsupported lockfileVersion {version}")),
            "{stderr}"
        );
    }

    let project = tempdir().unwrap();
    let store = tempdir().unwrap();
    fs::write(project.path().join("package.json"), r#"{"name":"app"}"#).unwrap();
    fs::write(
        project.path().join("package-lock.json"),
        r#"{"lockfileVersion":3,"packages":{"":{"name":"app"},"packages/local":{"version":"1.0.0","link":true},"node_modules/missing":{"version":"1.0.0"}}}"#,
    )
    .unwrap();

    let out = run_plain_install(project.path(), store.path());
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("LINK_PACKAGE_UNSUPPORTED"), "{stderr}");
    assert!(stderr.contains("MISSING_RESOLVED"), "{stderr}");
    assert!(!project.path().join("node_modules").exists());
}

#[test]
fn selected_lock_precedence_matches_project_lock_contract() {
    let (project, store, _tgz) = setup_project();
    fs::write(
        project.path().join("package-lock.json"),
        r#"{"lockfileVersion":2,"packages":{}}"#,
    )
    .unwrap();
    let sibling = run_plain_install(project.path(), store.path());
    assert!(
        sibling.status.success(),
        "sibling bpm.lock should win: {}",
        String::from_utf8_lossy(&sibling.stderr)
    );

    let child = project.path().join("child");
    fs::create_dir(&child).unwrap();
    fs::write(
        child.join("package.json"),
        r#"{"name":"child","dependencies":{"greet":"^1.0.0"}}"#,
    )
    .unwrap();
    fs::write(
        child.join("package-lock.json"),
        fs::read_to_string(project.path().join("package-lock.json"))
            .unwrap()
            .replace("\"lockfileVersion\":2", "\"lockfileVersion\":3"),
    )
    .unwrap();

    let nested = run_plain_install(&child, store.path());
    assert!(
        !nested.status.success(),
        "nested package-lock should be selected before ancestor bpm.lock"
    );
    assert!(
        String::from_utf8_lossy(&nested.stderr).contains("NoPackages")
            || String::from_utf8_lossy(&nested.stderr).contains("no \"packages\" table")
            || String::from_utf8_lossy(&nested.stderr).contains("package-lock.json"),
        "{}",
        String::from_utf8_lossy(&nested.stderr)
    );
}

#[test]
fn repeat_install_is_a_no_op_on_the_store() {
    let (project, store, _tgz) = setup_project();

    let first = run_install(project.path(), store.path());
    assert!(first.status.success());

    // Snapshot the greet symlink so we can prove the second run didn't rewrite it.
    let greet_link = project.path().join("node_modules/greet");
    let before = fs::read_link(&greet_link).unwrap();

    let second = run_install(project.path(), store.path());
    let stdout = String::from_utf8_lossy(&second.stdout);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(second.status.success(), "second install failed: {stderr}");
    // Milestone 3: with a valid cached plan, the second run skips resolution
    // and plan construction entirely — a plan-cache hit, not a store cache hit.
    assert!(
        stdout.contains("nothing to install"),
        "expected plan-cache hit, stdout: {stdout}"
    );
    assert!(
        stdout.contains("already materialized"),
        "expected plan-cache hit, stdout: {stdout}"
    );

    let after = fs::read_link(&greet_link).unwrap();
    assert_eq!(before, after, "idempotent rerun rewrote the symlink");
}

#[test]
fn frozen_refuses_when_manifest_and_lock_disagree() {
    let (project, store, _tgz) = setup_project();

    // Declare a dependency the lockfile does not have.
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","dependencies":{"greet":"^1.0.0","extra":"^9.0.0"}}"#,
    )
    .unwrap();

    let out = run_install(project.path(), store.path());
    assert!(!out.status.success(), "frozen mismatch should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("frozen install refused"),
        "expected frozen refusal, stderr: {stderr}"
    );
    assert!(
        stderr.contains("extra"),
        "should name the extra dep: {stderr}"
    );
}

#[test]
fn frozen_refuses_when_dependency_specification_drifts() {
    let (project, store, _tgz) = setup_project();

    // Same dependency name, but the declared range changes. The name set is
    // unchanged, so a name-only comparison would silently accept the stale
    // lockfile. Frozen mode must reject any change to the canonical
    // `name -> specification` map.
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","dependencies":{"greet":"^2.0.0"}}"#,
    )
    .unwrap();

    let out = run_install(project.path(), store.path());
    assert!(
        !out.status.success(),
        "same-name spec drift should fail (bpm.lock)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("frozen install refused"),
        "expected frozen refusal, stderr: {stderr}"
    );
    assert!(
        stderr.contains("greet"),
        "should name the drifted dep: {stderr}"
    );
    assert!(
        stderr.contains("^2.0.0"),
        "should show the new manifest spec: {stderr}"
    );
    assert!(
        stderr.contains("^1.0.0"),
        "should show the locked spec: {stderr}"
    );
}

#[test]
fn ci_refuses_when_dependency_specification_drifts() {
    let (project, store, _tgz) = setup_package_lock_project();

    // Same dependency name, but the declared range changes. `bpm ci` must
    // reject this for the npm v3 package-lock.json authority using the same
    // shared frozen guard as bpm.lock.
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","dependencies":{"greet":"^2.0.0"}}"#,
    )
    .unwrap();

    let out = run_ci(project.path(), store.path());
    assert!(
        !out.status.success(),
        "same-name spec drift should fail (package-lock.json)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("frozen install refused"),
        "expected frozen refusal, stderr: {stderr}"
    );
    assert!(
        stderr.contains("package-lock.json"),
        "should identify the selected lock authority: {stderr}"
    );
    assert!(
        stderr.contains("greet"),
        "should name the drifted dep: {stderr}"
    );
    assert!(
        stderr.contains("^2.0.0"),
        "should show the new manifest spec: {stderr}"
    );
    assert!(
        stderr.contains("^1.0.0"),
        "should show the locked spec: {stderr}"
    );
}

#[test]
fn install_runs_offline_without_network() {
    // Pure marker: the whole flow above used file:// sources only; there is no
    // exercise of any HTTP path, so installs are reproducible offline.
    let (project, store, _tgz) = setup_project();
    let out = run_install(project.path(), store.path());
    assert!(out.status.success());
}

#[test]
fn frozen_install_skips_optional_package_incompatible_with_host_platform() {
    // Platform filtering must skip an optional dependency whose `os` constraint
    // does not match this host, while still installing everything else. This
    // exercises build_install_work -> check_package_platform end to end and
    // proves the graph volume omits the skipped placement (its artifact id is
    // None, so it is never materialized).
    let project = tempdir().unwrap();
    let store = tempdir().unwrap();
    let tgz = tempdir().unwrap();

    let (greet_path, greet_int) = seed_tarball(
        tgz.path(),
        "greet.tgz",
        &build_tgz(&[(
            "package/package.json",
            b"{\"name\":\"greet\",\"version\":\"1.0.0\"}",
            0o644,
        )]),
    );
    // A real tarball so the entry is well-formed even though it is skipped.
    let (native_path, native_int) = seed_tarball(
        tgz.path(),
        "native.tgz",
        &build_tgz(&[(
            "package/package.json",
            b"{\"name\":\"native\",\"version\":\"1.0.0\",\"os\":[\"win32\"]}",
            0o644,
        )]),
    );

    // The optional dependency targets a platform no unix host satisfies.
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"greet":"^1.0.0"},"optionalDependencies":{"native":"^1.0.0"}}"#,
    )
    .unwrap();

    let mut lf = Lockfile::new("bpm-test");
    lf.root = RootEntry {
        name: Some("app".into()),
        version: Some("1.0.0".into()),
        dependencies: BTreeMap::from([
            ("greet".into(), "^1.0.0".into()),
            ("native".into(), "^1.0.0".into()),
        ]),
    };
    // The frozen guard requires the recorded optional map to match the manifest.
    lf.resolution.root.optional_dependencies = BTreeMap::from([("native".into(), "^1.0.0".into())]);
    lf.packages.push(PackageEntry {
        path: "node_modules/greet".into(),
        name: "greet".into(),
        version: "1.0.0".into(),
        resolved: format!("file://{}", greet_path.display()),
        integrity: Some(greet_int.to_npm_string()),
        ..Default::default()
    });
    lf.packages.push(PackageEntry {
        path: "node_modules/native".into(),
        name: "native".into(),
        version: "1.0.0".into(),
        resolved: format!("file://{}", native_path.display()),
        integrity: Some(native_int.to_npm_string()),
        optional: true,
        os: vec!["win32".to_string()],
        ..Default::default()
    });
    lf.sort_packages();
    lf.write_to(&project.path().join("bpm.lock")).unwrap();

    let out = run_install(project.path(), store.path());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "install failed: {stderr}\n--stdout--\n{stdout}"
    );

    // The compatible production dependency is still installed.
    assert!(
        project
            .path()
            .join("node_modules/greet/package.json")
            .exists(),
        "greet should be installed: {stdout} {stderr}"
    );
    // The platform-incompatible optional dependency is skipped, never linked.
    assert!(
        !project.path().join("node_modules/native").exists(),
        "native should be skipped on this platform: {stdout} {stderr}"
    );
    // The skip is surfaced as a stable platform diagnostic.
    assert!(
        stderr.contains("platform:"),
        "expected a platform skip diagnostic: {stderr}"
    );
}

#[test]
fn plan_cache_hit_is_recorded_in_metrics() {
    let (project, store, _tgz) = setup_project();
    let m1 = project.path().join("m1.json");
    let m2 = project.path().join("m2.json");

    run_install(project.path(), store.path());
    // Second run must record a plan cache hit, not a miss.
    Command::new(bpm_bin())
        .arg("install")
        .arg("--frozen")
        .arg("--store")
        .arg(store.path())
        .arg("--json-metrics")
        .arg(&m2)
        .current_dir(project.path())
        .output()
        .expect("failed to run bpm");
    let m2_text = fs::read_to_string(&m2).unwrap();
    assert!(m2_text.contains("\"plan_cache_hit\""), "metrics: {m2_text}");
    assert!(
        !m2_text.contains("\"plan_cache_miss\""),
        "second run should not miss: {m2_text}"
    );
    let _ = m1;
}

#[test]
fn plan_cache_invalidates_when_a_symlink_disappears() {
    let (project, store, _tgz) = setup_project();

    let first = run_install(project.path(), store.path());
    assert!(first.status.success());
    // A second run hits the cache.
    let second = run_install(project.path(), store.path());
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(stdout.contains("nothing to install"), "stdout: {stdout}");

    // Drift: someone deleted a materialized package symlink. The cached plan's
    // project-state validation must reject it and force a full re-install.
    let target = project.path().join("node_modules/greet");
    fs::remove_file(&target).unwrap();

    let third = run_install(project.path(), store.path());
    let stdout = String::from_utf8_lossy(&third.stdout);
    let stderr = String::from_utf8_lossy(&third.stderr);
    assert!(third.status.success(), "reinstall failed: {stderr}");
    assert!(
        stdout.contains("installed 2 package(s)"),
        "expected a full re-install after drift, stdout: {stdout}"
    );
    assert!(target.exists(), "package symlink should be restored");
}

/// Plan 011: a fresh frozen install persists nonempty, sorted project-view
/// ownership (the relay set) into `.bpm-state`, so a later graph change can
/// reconcile stale entries by exact identity rather than guesswork.
#[test]
fn fresh_install_persists_nonempty_sorted_project_view_ownership() {
    let (project, store, _tgz) = setup_project();
    let first = run_install(project.path(), store.path());
    assert!(first.status.success());

    let state = fs::read_to_string(project.path().join(".bpm-state")).expect(".bpm-state");
    let json: serde_json::Value = serde_json::from_str(&state).expect("parse .bpm-state");
    let owned: Vec<String> = json["owned_entries"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e["path"].as_str().map(String::from))
                .collect()
        })
        .expect("owned_entries array");
    assert!(
        !owned.is_empty(),
        "ownership must be nonempty after install"
    );
    assert_eq!(
        owned,
        {
            let mut s = owned.clone();
            s.sort();
            s
        },
        "ownership must be persisted sorted"
    );
    assert!(
        owned.iter().any(|p| p == "node_modules/greet"),
        "ownership must include the shallow greet entry; got: {owned:?}"
    );
}

#[test]
fn next_build_uses_a_project_local_dependency_view() {
    let project = tempdir().unwrap();
    let store = tempdir().unwrap();
    let tgz = tempdir().unwrap();
    fs::create_dir_all(project.path().join("packages/app")).unwrap();
    fs::write(
        project.path().join("packages/app/package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"next-app","version":"1.0.0","dependencies":{"next":"1.0.0"},"devDependencies":{"typescript":"1.0.0","@types/react":"1.0.0","@types/node":"1.0.0","eslint":"1.0.0"}}"#,
    )
    .unwrap();

    let next_script = br##"#!/usr/bin/env node
const fs = require('fs');
const path = require('path');
const Module = require('module');
function resolveFrom(baseDir, moduleId) {
  const realBase = fs.realpathSync(baseDir);
  const from = path.join(realBase, 'noop.js');
  return Module._resolveFilename(moduleId, {
    id: from,
    filename: from,
    paths: Module._nodeModulePaths(realBase),
  });
}
const required = [
  ['typescript', 'typescript/lib/typescript.js'],
  ['@types/react', '@types/react/index.d.ts'],
  ['@types/node', '@types/node/index.d.ts'],
  ['eslint', 'eslint/package.json'],
];
try {
  for (const [pkg, file] of required) {
    const packageJson = fs.realpathSync(resolveFrom(process.cwd(), `${pkg}/package.json`));
    const relative = path.relative(pkg, file);
    if (!fs.existsSync(path.join(path.dirname(packageJson), relative))) throw new Error(file);
  }
  console.log('next build ok');
} catch (error) {
  console.log('Installing devDependencies (npm):');
  console.error(error.message);
  process.exit(42);
}
"##;
    let packages = vec![
        (
            "next",
            br#"{"name":"next","version":"1.0.0"}"# as &[u8],
            vec![("bin/next", next_script.as_slice(), 0o755)],
        ),
        (
            "typescript",
            br#"{"name":"typescript","version":"1.0.0"}"# as &[u8],
            vec![(
                "lib/typescript.js",
                b"module.exports = { version: '1.0.0' };".as_slice(),
                0o644,
            )],
        ),
        (
            "@types/react",
            br#"{"name":"@types/react","version":"1.0.0"}"# as &[u8],
            vec![("index.d.ts", b"export {};".as_slice(), 0o644)],
        ),
        (
            "@types/node",
            br#"{"name":"@types/node","version":"1.0.0"}"# as &[u8],
            vec![("index.d.ts", b"export {};".as_slice(), 0o644)],
        ),
        (
            "eslint",
            br#"{"name":"eslint","version":"1.0.0"}"# as &[u8],
            Vec::new(),
        ),
    ];
    let mut lockfile = Lockfile::new("bpm-test");
    lockfile.root = RootEntry {
        name: Some("next-app".into()),
        version: Some("1.0.0".into()),
        dependencies: BTreeMap::from([
            ("next".into(), "1.0.0".into()),
            ("typescript".into(), "1.0.0".into()),
            ("@types/react".into(), "1.0.0".into()),
            ("@types/node".into(), "1.0.0".into()),
            ("eslint".into(), "1.0.0".into()),
        ]),
    };
    for (name, manifest, files) in packages {
        let mut entries: Vec<(String, &[u8], u32)> =
            vec![("package/package.json".into(), manifest, 0o644)];
        entries.extend(
            files
                .into_iter()
                .map(|(path, bytes, mode)| (format!("package/{path}"), bytes, mode)),
        );
        let archive_entries = entries
            .iter()
            .map(|(path, bytes, mode)| (path.as_str(), *bytes, *mode))
            .collect::<Vec<_>>();
        let archive = build_tgz(&archive_entries);
        let archive_name = name.replace('/', "_");
        let (path, integrity) = seed_tarball(tgz.path(), &format!("{archive_name}.tgz"), &archive);
        let mut bin = BTreeMap::new();
        if name == "next" {
            bin.insert("next".into(), "bin/next".into());
        }
        lockfile.packages.push(PackageEntry {
            path: format!("node_modules/{name}"),
            name: name.into(),
            version: "1.0.0".into(),
            resolved: format!("file://{}", path.display()),
            integrity: Some(integrity.to_npm_string()),
            bin,
            ..Default::default()
        });
    }
    // Exercise the workspace materialization branch as well: it must use the
    // same project-local backend for registry packages when Next is present.
    lockfile.packages.push(PackageEntry {
        path: "node_modules/app".into(),
        name: "app".into(),
        link: true,
        workspace_target: Some("packages/app".into()),
        ..Default::default()
    });
    lockfile.sort_packages();
    lockfile.write_to(&project.path().join("bpm.lock")).unwrap();

    let install = run_plain_install(project.path(), store.path());
    assert!(
        install.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );
    assert!(
        !fs::symlink_metadata(project.path().join("node_modules/next"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "Next must receive a project-local dependency view"
    );

    let build = Command::new(bpm_bin())
        .args(["exec", "next", "build"])
        .current_dir(project.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&build.stdout);
    let stderr = String::from_utf8_lossy(&build.stderr);
    assert!(
        build.status.success(),
        "next build failed: {stderr}\n{stdout}"
    );
    assert!(stdout.contains("next build ok"), "stdout: {stdout}");
    assert!(
        !stdout.contains("Installing devDependencies (npm)"),
        "Next attempted its fallback installer: {stdout}"
    );
}

#[test]
fn second_project_with_same_graph_reuses_the_volume() {
    // Milestone 4 success criterion: a second project that shares the same
    // graph (identical bpm.lock) performs minimal filesystem work — it reuses
    // the already-built graph volume in the shared store rather than rebuilding.

    // Project A builds the volume the first time.
    let (proj_a, store, tgz) = setup_project();
    let out_a = run_install(proj_a.path(), store.path());
    let stdout_a = String::from_utf8_lossy(&out_a.stdout);
    assert!(out_a.status.success());
    assert!(
        stdout_a.contains("graph volume built"),
        "stdout: {stdout_a}"
    );

    // Project B: same store, identical bpm.lock (same graph id) + package.json.
    let proj_b = tempdir().unwrap();
    fs::write(
        proj_b.path().join("package.json"),
        fs::read_to_string(proj_a.path().join("package.json")).unwrap(),
    )
    .unwrap();
    fs::write(
        proj_b.path().join("bpm.lock"),
        fs::read_to_string(proj_a.path().join("bpm.lock")).unwrap(),
    )
    .unwrap();
    let _ = tgz; // tarballs live via file:// URLs in the shared lockfile

    let out_b = run_install(proj_b.path(), store.path());
    let stdout_b = String::from_utf8_lossy(&out_b.stdout);
    let stderr_b = String::from_utf8_lossy(&out_b.stderr);
    assert!(out_b.status.success(), "second install failed: {stderr_b}");
    // The volume is reused (no rebuild); only the project relays were created.
    assert!(
        stdout_b.contains("graph volume reused"),
        "expected volume reuse, stdout: {stdout_b}"
    );
    // And the project view works through the reused volume.
    assert!(proj_b
        .path()
        .join("node_modules/greet/package.json")
        .exists());
    assert!(proj_b
        .path()
        .join("node_modules/greet/node_modules/dep/package.json")
        .exists());
    assert!(proj_b.path().join("node_modules/.bin/hello").exists());
    // Project B's node_modules is a relay layer INTO the shared volume.
    let relay = fs::read_link(proj_b.path().join("node_modules/greet")).unwrap();
    assert!(
        relay.to_string_lossy().contains("graphs/blake3"),
        "relay should point into the graph volume: {}",
        relay.display()
    );
}

// ── GC ownership integration (plan 017) ──────────────────────────────────

/// Run `bpm gc --older-than <grace> --store <store>` and return its output.
fn run_gc(store: &Path, grace: &str) -> std::process::Output {
    Command::new(bpm_bin())
        .arg("gc")
        .arg("--older-than")
        .arg(grace)
        .arg("--store")
        .arg(store)
        .output()
        .expect("failed to run bpm gc")
}

/// Set the mtime of every immutable store object (graphs, artifacts, images,
/// derived) under `store` to `age` in the past so a small grace window makes
/// them age-eligible for GC.
fn age_store_objects(store: &Path, age: std::time::Duration) {
    let old = std::time::SystemTime::now() - age;
    for namespace in ["graphs", "artifacts", "images", "derived"] {
        let base = store.join(namespace);
        let Ok(entries) = fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            age_tree_recursive(&entry.path(), old);
        }
    }
}

fn age_tree_recursive(path: &Path, old: std::time::SystemTime) {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.is_dir() {
            if let Ok(children) = fs::read_dir(path) {
                for child in children.flatten() {
                    age_tree_recursive(&child.path(), old);
                }
            }
        }
        if let Ok(file) = fs::File::open(path) {
            let _ = file.set_modified(old);
        }
    }
}

/// Count published graph volumes under `<store>/graphs/blake3/**`.
fn count_graph_volumes(store: &Path) -> usize {
    let base = store.join("graphs/blake3");
    let Ok(prefixes) = fs::read_dir(&base) else {
        return 0;
    };
    let mut count = 0;
    for prefix in prefixes.flatten() {
        if let Ok(volumes) = fs::read_dir(prefix.path()) {
            count += volumes.flatten().filter(|e| e.path().is_dir()).count();
        }
    }
    count
}

/// Collect every published artifact tarball path under the store.
fn collect_artifact_files(store: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let base = store.join("artifacts/sha512");
    let Ok(prefixes) = fs::read_dir(&base) else {
        return out;
    };
    for prefix in prefixes.flatten() {
        if let Ok(files) = fs::read_dir(prefix.path()) {
            for file in files.flatten() {
                out.push(file.path());
            }
        }
    }
    out
}

#[test]
fn gc_retains_active_project_graph_and_dependencies() {
    let (project, store, _tgz) = setup_project();
    let out = run_install(project.path(), store.path());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "install failed: {stderr}");

    // The graph volume and its artifact/image were published.
    assert_eq!(count_graph_volumes(store.path()), 1);
    let artifacts_before = collect_artifact_files(store.path());
    assert!(!artifacts_before.is_empty(), "store should hold artifacts");

    // Age every store object well past a one-second grace window, then GC.
    age_store_objects(store.path(), std::time::Duration::from_secs(60));
    let out = run_gc(store.path(), "1s");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "gc failed: {stderr}\n{stdout}");

    // The active project's graph volume and artifacts survive GC.
    assert_eq!(
        count_graph_volumes(store.path()),
        1,
        "active graph must survive GC; stdout: {stdout}"
    );
    for artifact in &artifacts_before {
        assert!(
            artifact.exists(),
            "active artifact must survive GC: {}",
            artifact.display()
        );
    }
    // The project's node_modules is still usable through the retained volume.
    assert!(project
        .path()
        .join("node_modules/greet/package.json")
        .exists());

    // Re-running install is a plan-cache hit and still succeeds (the retained
    // graph remains valid).
    let out = run_install(project.path(), store.path());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "re-install failed: {stderr}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("nothing to install"),
        "expected plan-cache hit; stdout: {stdout}"
    );
}

#[test]
fn gc_rebuilds_protection_after_store_db_deleted() {
    let (project, store, _tgz) = setup_project();
    let out = run_install(project.path(), store.path());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "install failed: {stderr}");
    assert_eq!(count_graph_volumes(store.path()), 1);

    let db = store.path().join("store.db");
    assert!(db.exists(), "store.db should exist after install");
    fs::remove_file(&db).unwrap();

    // Age objects and GC with no store.db: durable graph inventory + project
    // registration must reconstruct protection so the active graph is retained.
    age_store_objects(store.path(), std::time::Duration::from_secs(60));
    let out = run_gc(store.path(), "1s");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "gc failed: {stderr}\n{stdout}");
    assert_eq!(
        count_graph_volumes(store.path()),
        1,
        "graph must be retained after store.db rebuild; stdout: {stdout}"
    );
}
