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

/// A second project that shares the same graph (identical `bpm.lock`) reuses
/// the already-built graph volume. Its derived lifecycle output is already
/// persisted in the volume, so the reused-volume install must NOT re-run any
/// lifecycle script. This is the warm-path fix the M7 closeout calls out: the
/// plan-cache path already skipped lifecycle, but a volume reuse with a plan
/// miss (this project has no `.bpm-state` yet) used to re-run every script.
#[test]
fn second_project_reuses_volume_and_skips_lifecycle() {
    let (proj_a, store, _tgz, _host_image_rel) = setup_project();

    // Project A builds the volume and runs the postinstall (marker appears).
    let out_a = run_install(proj_a.path(), store.path());
    let stderr_a = String::from_utf8_lossy(&out_a.stderr);
    assert!(out_a.status.success(), "install A failed: {stderr_a}");
    let stderr_a_has_lifecycle = stderr_a.contains("lifecycle:");
    let marker_a = proj_a.path().join("node_modules/host/.bpm-marker");
    assert!(marker_a.exists(), "install A should produce the marker");
    let built_mtime = fs::symlink_metadata(&marker_a).unwrap().modified().unwrap();

    // Settle so a re-write would be observable via mtime.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    // Project B: same store, identical graph (same `bpm.lock` + manifest).
    let proj_b = tempfile::tempdir().unwrap();
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

    let metrics_b = proj_b.path().join("metrics_b.json");
    let out_b = Command::new(bpm_bin())
        .arg("install")
        .arg("--frozen")
        .arg("--store")
        .arg(store.path())
        .arg("--json-metrics")
        .arg(&metrics_b)
        .current_dir(proj_b.path())
        .output()
        .expect("failed to run bpm");
    let stdout_b = String::from_utf8_lossy(&out_b.stdout);
    let stderr_b = String::from_utf8_lossy(&out_b.stderr);
    assert!(out_b.status.success(), "install B failed: {stderr_b}");
    // The volume is reused (no rebuild).
    assert!(
        stdout_b.contains("graph volume reused"),
        "expected volume reuse, stdout: {stdout_b}",
    );
    // B reaches A's derived content through the shared volume.
    assert!(proj_b.path().join("node_modules/host/.bpm-marker").exists());

    // Headline assertion: lifecycle did NOT re-run. The summary line that A
    // produced is absent from B's stderr...
    if stderr_a_has_lifecycle {
        assert!(
            !stderr_b.contains("lifecycle:"),
            "reused-volume install must not report lifecycle execution: {stderr_b}",
        );
    }
    // ...and the skip is observable in B's metrics.
    let metrics_text = fs::read_to_string(&metrics_b).unwrap();
    assert!(
        metrics_text.contains("\"lifecycle_skipped_cached_volume\""),
        "expected lifecycle_skipped_cached_volume marker in metrics: {metrics_text}",
    );

    // The derived content in the shared volume is untouched: B's view of the
    // marker still carries A's original mtime (it was never rewritten).
    let reuse_mtime = fs::symlink_metadata(proj_b.path().join("node_modules/host/.bpm-marker"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        built_mtime, reuse_mtime,
        "reused volume must not re-run lifecycle / rewrite derived content",
    );
}

/// Headline derived-store test: two *different* graphs that share a
/// lifecycle-bearing package's dependency closure reuse that package's derived
/// image, so its scripts never re-run on the second graph.
///
/// Graph G1 = {host, host-dep, unrelated-a}; G2 = {host, host-dep,
/// unrelated-b}. `host` carries a postinstall; `unrelated-*` is not reachable
/// from `host`, so `host`'s closure (and thus its derived key) is identical
/// across G1 and G2 even though the graphs differ. With `BPM_DERIVED_STORE=1`,
/// project A builds and publishes `host`'s derived image; project B (different
/// graph, same store) attaches it and skips the postinstall entirely -- proven
/// by a run-counter the script appends on every execution.
#[test]
fn derived_store_reuses_image_across_graphs_with_same_closure() {
    let store = tempfile::tempdir().unwrap();
    let tgz = tempfile::tempdir().unwrap();

    // `host` appends to a run-counter on every postinstall, so re-execution is
    // observable as a line count > 1. The guard `test -d ../host-dep` proves the
    // dependency still resolves during the derived build (npm semantics).
    let host_bytes = build_tgz(|b| {
        add_dir_pkg(b);
        add_file(
            b,
            "package/package.json",
            0o644,
            br#"{"name":"host","version":"1.0.0","scripts":{"postinstall":"test -d ../host-dep && echo run >> .bpm-runs"},"dependencies":{"host-dep":"^1.0.0"}}"#,
        );
    });
    let host_dep_bytes = build_tgz(|b| {
        add_dir_pkg(b);
        add_file(
            b,
            "package/package.json",
            0o644,
            br#"{"name":"host-dep","version":"1.0.0"}"#,
        );
    });
    let unrel_a_bytes = build_tgz(|b| {
        add_dir_pkg(b);
        add_file(
            b,
            "package/package.json",
            0o644,
            br#"{"name":"unrelated-a","version":"1.0.0"}"#,
        );
    });
    let unrel_b_bytes = build_tgz(|b| {
        add_dir_pkg(b);
        add_file(
            b,
            "package/package.json",
            0o644,
            br#"{"name":"unrelated-b","version":"1.0.0"}"#,
        );
    });

    let (host_path, host_int) = seed_tarball(tgz.path(), "host.tgz", &host_bytes);
    let (hd_path, hd_int) = seed_tarball(tgz.path(), "host-dep.tgz", &host_dep_bytes);
    let (ua_path, ua_int) = seed_tarball(tgz.path(), "unrelated-a.tgz", &unrel_a_bytes);
    let (ub_path, ub_int) = seed_tarball(tgz.path(), "unrelated-b.tgz", &unrel_b_bytes);

    let host_resolved = format!("file://{}", host_path.display());
    let hd_resolved = format!("file://{}", hd_path.display());
    let host_int_str = host_int.to_npm_string();
    let hd_int_str = hd_int.to_npm_string();

    let write_project = |dir: &Path, unrel_name: &str, unrel_path: &Path, unrel_int_str: String| {
        fs::write(
            dir.join("package.json"),
            format!(
                r#"{{"name":"app","version":"1.0.0","dependencies":{{"host":"^1.0.0","{unrel_name}":"^1.0.0"}}}}"#
            ),
        )
        .unwrap();
        let mut lf = Lockfile::new("bpm-test");
        lf.root = RootEntry {
            name: Some("app".into()),
            version: Some("1.0.0".into()),
            dependencies: BTreeMap::from([
                ("host".into(), "^1.0.0".into()),
                (unrel_name.into(), "^1.0.0".into()),
            ]),
        };
        lf.packages.push(PackageEntry {
            path: "node_modules/host".into(),
            name: "host".into(),
            version: "1.0.0".into(),
            resolved: host_resolved.clone(),
            integrity: Some(host_int_str.clone()),
            dependencies: BTreeMap::from([("host-dep".into(), "^1.0.0".into())]),
            ..Default::default()
        });
        lf.packages.push(PackageEntry {
            path: "node_modules/host-dep".into(),
            name: "host-dep".into(),
            version: "1.0.0".into(),
            resolved: hd_resolved.clone(),
            integrity: Some(hd_int_str.clone()),
            ..Default::default()
        });
        lf.packages.push(PackageEntry {
            path: format!("node_modules/{unrel_name}"),
            name: unrel_name.into(),
            version: "1.0.0".into(),
            resolved: format!("file://{}", unrel_path.display()),
            integrity: Some(unrel_int_str),
            ..Default::default()
        });
        lf.sort_packages();
        lf.write_to(&dir.join("bpm.lock")).unwrap();
    };

    let proj_a = tempfile::tempdir().unwrap();
    write_project(
        proj_a.path(),
        "unrelated-a",
        &ua_path,
        ua_int.to_npm_string(),
    );
    let proj_b = tempfile::tempdir().unwrap();
    write_project(
        proj_b.path(),
        "unrelated-b",
        &ub_path,
        ub_int.to_npm_string(),
    );

    let metrics_a = proj_a.path().join("metrics_a.json");
    let out_a = Command::new(bpm_bin())
        .arg("install")
        .arg("--frozen")
        .arg("--store")
        .arg(store.path())
        .arg("--json-metrics")
        .arg(&metrics_a)
        .env("BPM_DERIVED_STORE", "1")
        .current_dir(proj_a.path())
        .output()
        .expect("failed to run bpm");
    let stderr_a = String::from_utf8_lossy(&out_a.stderr);
    assert!(out_a.status.success(), "install A failed: {stderr_a}");
    let runs_a = proj_a.path().join("node_modules/host/.bpm-runs");
    assert!(
        runs_a.exists(),
        "host postinstall did not run in A: {stderr_a}",
    );
    let metrics_a_text = fs::read_to_string(&metrics_a).unwrap();
    assert!(
        metrics_a_text.contains("derived_store_built"),
        "A should build + publish host's derived image: {metrics_a_text}",
    );

    // Settle so an accidental re-run would advance the run-counter unambiguously.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let metrics_b = proj_b.path().join("metrics_b.json");
    let out_b = Command::new(bpm_bin())
        .arg("install")
        .arg("--frozen")
        .arg("--store")
        .arg(store.path())
        .arg("--json-metrics")
        .arg(&metrics_b)
        .env("BPM_DERIVED_STORE", "1")
        .current_dir(proj_b.path())
        .output()
        .expect("failed to run bpm");
    let stdout_b = String::from_utf8_lossy(&out_b.stdout);
    let stderr_b = String::from_utf8_lossy(&out_b.stderr);
    assert!(out_b.status.success(), "install B failed: {stderr_b}");
    // G2 != G1, so the volume is NOT reused -- it is a fresh build.
    assert!(
        !stdout_b.contains("graph volume reused"),
        "G2 must build its own volume (it is a different graph): {stdout_b}",
    );
    let metrics_b_text = fs::read_to_string(&metrics_b).unwrap();
    assert!(
        metrics_b_text.contains("derived_store_hit"),
        "B must reuse host's derived image across the different graph: {metrics_b_text}",
    );

    // Headline proof: host's postinstall did NOT re-run in B. The run-counter
    // would hold two lines had the script executed; the attached derived image
    // still carries the single line from A's build.
    let runs_b = proj_b.path().join("node_modules/host/.bpm-runs");
    assert!(
        runs_b.exists(),
        "B did not attach host's derived image: {stderr_b}",
    );
    let b_runs_count = fs::read_to_string(&runs_b).unwrap().lines().count();
    assert_eq!(
        b_runs_count, 1,
        "host postinstall re-ran in B (derived store did not hit): {stderr_b}\nmetrics: {metrics_b_text}",
    );
}
