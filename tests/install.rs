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
    assert!(stdout.contains("1 bin(s) linked"), "stdout: {stdout}");

    let nm = project.path().join("node_modules");
    assert_resolves(&nm.join("greet"));
    assert_resolves(&nm.join("greet/node_modules/dep"));
    assert!(nm.join("greet/package.json").exists());
    assert!(nm.join("greet/node_modules/dep/package.json").exists());

    let bin = nm.join(".bin").join("hello");
    assert_resolves(&bin);
    assert!(is_executable(&bin), "bin must keep its executable bit");
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
    // Second run against the populated store: everything served from cache.
    assert!(stdout.contains("2 cached"), "stdout: {stdout}");
    assert!(stdout.contains("0 fetched"), "stdout: {stdout}");

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
fn install_runs_offline_without_network() {
    // Pure marker: the whole flow above used file:// sources only; there is no
    // exercise of any HTTP path, so installs are reproducible offline.
    let (project, store, _tgz) = setup_project();
    let out = run_install(project.path(), store.path());
    assert!(out.status.success());
}
