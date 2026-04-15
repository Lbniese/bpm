//! End-to-end CLI tests for name-based `bpm fetch` resolution: a package
//! spec is resolved against a local (path-routed) registry mock, the tarball
//! is fetched through the immutable store, and the image is extracted.
//! Fully offline — no network.

mod common;

use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::{build_tgz, integrity_of, MiniServer, RouteBody};

fn bin() -> String {
    std::env::var("CARGO_BIN_EXE_bpm").expect("CARGO_BIN_EXE_bpm")
}

/// A local registry mock: serves a packument on `/lodash` and the tarball on
/// any `*.tgz` path. Records how many tarball bytes-responses were served so
/// the test can prove the store did not re-download on a cache hit.
struct RegistryMock {
    server: MiniServer,
    tarball_hits: Arc<AtomicUsize>,
}

impl RegistryMock {
    fn start(tgz: Vec<u8>, integrity: String) -> Self {
        let tgz = Arc::new(tgz);
        let base: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let base_c = base.clone();
        let tarball_hits = Arc::new(AtomicUsize::new(0));
        let tarball_hits_c = tarball_hits.clone();
        let integ = Arc::new(integrity);

        let server = MiniServer::start_routed(move |path| {
            let base = base_c.lock().unwrap().clone();
            if path == "/lodash/1.0.0" {
                let metadata = serde_json::json!({
                    "name": "lodash",
                    "version": "1.0.0",
                    "dist": {
                        "tarball": format!("{base}lodash/-/lodash-1.0.0.tgz"),
                        "integrity": &*integ,
                    }
                });
                Some(RouteBody(
                    serde_json::to_vec(&metadata).unwrap(),
                    "application/json",
                ))
            } else if path == "/lodash" {
                let packument = serde_json::json!({
                    "dist-tags": { "latest": "4.17.21" },
                    "versions": {
                        "4.17.21": {
                            "dist": {
                                "tarball": format!("{base}lodash/-/lodash-4.17.21.tgz"),
                                "integrity": &*integ,
                            }
                        },
                        "1.0.0": {
                            "dist": {
                                "tarball": format!("{base}lodash/-/lodash-1.0.0.tgz"),
                                "integrity": &*integ,
                            }
                        }
                    }
                });
                Some(RouteBody(
                    serde_json::to_vec(&packument).unwrap(),
                    "application/json",
                ))
            } else if path.ends_with(".tgz") {
                tarball_hits_c.fetch_add(1, Ordering::Relaxed);
                Some(RouteBody((*tgz).clone(), "application/gzip"))
            } else {
                None
            }
        });

        // Back-fill the base URL now that the server has a real address. The
        // packument is built lazily on each request, so it picks this up.
        *base.lock().unwrap() = server.url("");

        Self {
            server,
            tarball_hits,
        }
    }

    fn registry_url(&self) -> String {
        // Strip the trailing slash so resolve() composes `/lodash` cleanly.
        self.server.url("").trim_end_matches('/').to_string()
    }

    fn tarball_hits(&self) -> usize {
        self.tarball_hits.load(Ordering::Relaxed)
    }
}

fn fixture_tgz() -> Vec<u8> {
    build_tgz(|b| {
        common::add_dir(b, "package", 0o755);
        common::add_file(
            b,
            "package/package.json",
            0o644,
            br#"{"name":"lodash","version":"4.17.21"}"#,
        );
        common::add_file(b, "package/index.js", 0o644, b"module.exports = 1;");
    })
}

fn run_fetch(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(bin()).args(args).output().expect("run bpm");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn fetch_resolves_name_to_latest() {
    let tgz = fixture_tgz();
    let integrity = integrity_of(&tgz);
    let reg = RegistryMock::start(tgz, integrity);
    let store = tempfile::tempdir().unwrap();

    let (ok, stdout, stderr) = run_fetch(&[
        "fetch",
        "lodash",
        "--registry",
        &reg.registry_url(),
        "--store",
        store.path().to_str().unwrap(),
    ]);
    assert!(ok, "stderr: {stderr}\nstdout: {stdout}");
    // Resolution line is printed to stderr before the artifact line.
    assert!(stderr.contains("resolved lodash@4.17.21"), "{stderr}");
    assert!(stdout.contains("artifact"), "{stdout}");
    assert!(stdout.contains("image"), "{stdout}");
    assert_eq!(reg.tarball_hits(), 1, "tarball downloaded exactly once");

    // Image extracted: package.json at the image root.
    let image_root = store.path().join("images/sha512");
    let found = walk_find(&image_root, "package.json");
    assert!(found, "image package.json not found under {image_root:?}");
}

#[test]
fn fetch_resolves_exact_version() {
    let tgz = fixture_tgz();
    let integrity = integrity_of(&tgz);
    let reg = RegistryMock::start(tgz, integrity);
    let store = tempfile::tempdir().unwrap();

    let (ok, _stdout, stderr) = run_fetch(&[
        "fetch",
        "lodash@1.0.0",
        "--registry",
        &reg.registry_url(),
        "--store",
        store.path().to_str().unwrap(),
    ]);
    assert!(ok, "{stderr}");
    assert!(stderr.contains("resolved lodash@1.0.0"), "{stderr}");
    assert_eq!(reg.tarball_hits(), 1);
}

#[test]
fn second_fetch_serves_tarball_from_cache() {
    let tgz = fixture_tgz();
    let integrity = integrity_of(&tgz);
    let reg = RegistryMock::start(tgz, integrity);
    let store = tempfile::tempdir().unwrap();
    let store_path = store.path().to_str().unwrap();
    let registry = reg.registry_url();

    let (ok1, out1, err1) = run_fetch(&[
        "fetch",
        "lodash",
        "--registry",
        &registry,
        "--store",
        store_path,
    ]);
    assert!(ok1, "{err1}");
    assert_eq!(reg.tarball_hits(), 1);
    assert!(out1.contains("stored"), "first run should store: {out1}");

    // Second fetch resolves again (1 metadata hit) but must NOT re-download.
    let (ok2, out2, err2) = run_fetch(&[
        "fetch",
        "lodash",
        "--registry",
        &registry,
        "--store",
        store_path,
    ]);
    assert!(ok2, "{err2}");
    assert_eq!(
        reg.tarball_hits(),
        1,
        "tarball must not re-download on cache hit"
    );
    assert!(
        out2.contains("cached"),
        "second run should be cached: {out2}"
    );
}

#[test]
fn fetch_unknown_package_errors_clearly() {
    // Empty registry: every path 404s.
    let server = MiniServer::start_routed(|_path| None);
    let registry = server.url("").trim_end_matches('/').to_string();
    let store = tempfile::tempdir().unwrap();

    let (ok, _stdout, stderr) = run_fetch(&[
        "fetch",
        "does-not-exist-xyz",
        "--registry",
        &registry,
        "--store",
        store.path().to_str().unwrap(),
    ]);
    assert!(!ok, "should fail on unknown package");
    assert!(
        stderr.contains("does-not-exist-xyz"),
        "error names the package: {stderr}"
    );
}

#[test]
fn fetch_url_still_works_unchanged() {
    // A bare exact URL must skip resolution and fetch directly (regression guard).
    let tgz = fixture_tgz();
    let integrity = integrity_of(&tgz);
    let server = MiniServer::start(tgz);
    let url = server.url_for();
    let store = tempfile::tempdir().unwrap();

    let (ok, stdout, stderr) = run_fetch(&[
        "fetch",
        &url,
        "--integrity",
        &integrity,
        "--store",
        store.path().to_str().unwrap(),
    ]);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("artifact"), "{stdout}");
    assert!(
        !stderr.contains("resolved"),
        "URL path must not resolve: {stderr}"
    );
}

/// Recursively search `root` for a file named `name`, returning whether found.
fn walk_find(root: &std::path::Path, name: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if walk_find(&p, name) {
                return true;
            }
        } else if p.file_name().map(|n| n == name).unwrap_or(false) {
            return true;
        }
    }
    false
}
