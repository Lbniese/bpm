//! Integration test for in-place lifecycle execution against the graph volume.
//!
//! A package with a `postinstall` script that both reaches a dependency
//! (proving the volume's complete `node_modules` tree resolves during the
//! script, like npm) and writes a file into its own directory (proving derived
//! content persists) is installed offline via `file://` tarballs. The marker
//! file must appear in the project's `node_modules` view (reached through the
//! volume) while the immutable store image stays pristine (isolation).

#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use bpm::integrity::{Integrity, Sha512Digest};
use bpm::lockfile::{Lockfile, PackageEntry, RootEntry};

use common::{add_file, build_tgz, integrity_of};

fn bpm_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bpm"))
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

/// Build a project: `host` (with a postinstall) depends on `dep`. The
/// postinstall writes a marker file only when `dep` is reachable as a sibling
/// (`../dep`), so a present marker proves dependency resolution through the
/// volume tree during script execution.
fn setup_project() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    tempfile::TempDir,
    PathBuf,
) {
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let tgz = tempfile::tempdir().unwrap();

    let host_bytes = build_tgz(|b| {
        add_dir_pkg(b);
        add_file(
            b,
            "package/package.json",
            0o644,
            br#"{"name":"host","version":"1.0.0","scripts":{"postinstall":"test -d ../dep && echo derived > .bpm-marker"}}"#,
        );
    });
    let dep_bytes = build_tgz(|b| {
        add_dir_pkg(b);
        add_file(
            b,
            "package/package.json",
            0o644,
            br#"{"name":"dep","version":"1.0.0","main":"index.js"}"#,
        );
        add_file(b, "package/index.js", 0o644, b"module.exports = 1;\n");
    });

    let (host_path, host_int) = seed_tarball(tgz.path(), "host.tgz", &host_bytes);
    let (dep_path, dep_int) = seed_tarball(tgz.path(), "dep.tgz", &dep_bytes);

    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"host":"^1.0.0"}}"#,
    )
    .unwrap();

    let mut lf = Lockfile::new("bpm-test");
    lf.root = RootEntry {
        name: Some("app".into()),
        version: Some("1.0.0".into()),
        dependencies: BTreeMap::from([("host".into(), "^1.0.0".into())]),
    };
    lf.packages.push(PackageEntry {
        path: "node_modules/host".into(),
        name: "host".into(),
        version: "1.0.0".into(),
        resolved: format!("file://{}", host_path.display()),
        integrity: Some(host_int.to_npm_string()),
        dependencies: BTreeMap::from([("dep".into(), "^1.0.0".into())]),
        ..Default::default()
    });
    lf.packages.push(PackageEntry {
        path: "node_modules/dep".into(),
        name: "dep".into(),
        version: "1.0.0".into(),
        resolved: format!("file://{}", dep_path.display()),
        integrity: Some(dep_int.to_npm_string()),
        ..Default::default()
    });
    lf.sort_packages();
    lf.write_to(&project.path().join("bpm.lock")).unwrap();

    let host_hex = Sha512Digest::hash_bytes(&host_bytes).to_hex();
    (
        project,
        store,
        tgz,
        PathBuf::from(format!("images/sha512/{}/{}", &host_hex[..2], host_hex)),
    )
}

fn add_dir_pkg(b: &mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>) {
    let mut h = tar::Header::new_gnu();
    h.set_path("package").unwrap();
    h.set_entry_type(tar::EntryType::Directory);
    h.set_size(0);
    h.set_mode(0o755);
    h.set_cksum();
    b.append(&h, &[][..]).unwrap();
}

fn seed_tarball(dir: &Path, name: &str, bytes: &[u8]) -> (PathBuf, Integrity) {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    (path, Integrity::parse(&integrity_of(bytes)).unwrap())
}

#[test]
fn postinstall_runs_in_volume_resolves_deps_and_persists_derived_content() {
    let (project, store, _tgz, host_image_rel) = setup_project();
    let out = run_install(project.path(), store.path());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "install failed: {stderr}");

    // The postinstall only writes the marker when `../dep` resolves, so a
    // present marker proves dependency resolution through the volume tree AND
    // that derived content reached the project view.
    let marker = project.path().join("node_modules/host/.bpm-marker");
    assert!(
        marker.exists(),
        "postinstall marker missing — deps did not resolve or derived content did not persist: {stderr}",
    );
    assert_eq!(fs::read_to_string(&marker).unwrap().trim(), "derived",);

    // The marker reached the project because the volume holds the derived copy;
    // the immutable store image must remain pristine (isolation).
    let store_host_image = store.path().join(&host_image_rel).join(".bpm-marker");
    assert!(
        !store_host_image.exists(),
        "postinstall mutated the immutable store image — isolation failed",
    );
    assert_eq!(
        fs::read_to_string(store.path().join(&host_image_rel).join("package.json"))
            .unwrap()
            .trim_end_matches('\n'),
        r#"{"name":"host","version":"1.0.0","scripts":{"postinstall":"test -d ../dep && echo derived > .bpm-marker"}}"#,
    );
}

#[test]
fn second_install_hits_plan_cache_without_rerunning_lifecycle() {
    let (project, store, _tgz, _host_image_rel) = setup_project();

    let first = run_install(project.path(), store.path());
    assert!(first.status.success());
    let marker = project.path().join("node_modules/host/.bpm-marker");
    assert!(marker.exists(), "first install should produce the marker");
    let first_mtime = fs::symlink_metadata(&marker).unwrap().modified().unwrap();

    // Settle so a re-write would observable advance the mtime.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let second = run_install(project.path(), store.path());
    let stdout = String::from_utf8_lossy(&second.stdout);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(second.status.success(), "second install failed: {stderr}");
    assert!(
        stdout.contains("nothing to install"),
        "expected plan-cache hit, stdout: {stdout}",
    );

    // A plan-cache hit skips lifecycle entirely; the prior derived content is
    // reused untouched (mtime unchanged).
    let second_mtime = fs::symlink_metadata(&marker).unwrap().modified().unwrap();
    assert_eq!(
        first_mtime, second_mtime,
        "cache hit should not re-run lifecycle / rewrite derived content",
    );
}
