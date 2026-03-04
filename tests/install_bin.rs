//! End-to-end CLI tests for `bpm install <pkg>` — fetch a single package and
//! link its declared executables into a global bin directory. Fully offline:
//! a local mock registry serves the packument + tarball.

mod common;

use std::path::Path;
use std::process::Command;

use common::{build_tgz, integrity_of, MiniServer, RouteBody};

fn bin() -> String {
    std::env::var("CARGO_BIN_EXE_bpm").expect("CARGO_BIN_EXE_bpm")
}

/// A mock registry serving `demo-cli`'s packument (latest 1.0.0) and tarball.
/// The tarball mimics an npm package: files at the archive root, with a
/// `package.json` declaring two bins and a shebang script for each.
#[cfg(unix)]
struct RegistryMock {
    _server: MiniServer,
    registry_url: String,
}

#[cfg(unix)]
impl RegistryMock {
    fn start() -> Self {
        let tgz = fixture_tgz();
        let integrity = integrity_of(&tgz);
        let base: std::sync::Arc<std::sync::Mutex<String>> =
            std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let base_c = base.clone();
        let tgz = std::sync::Arc::new(tgz);

        let server = MiniServer::start_routed(move |path| {
            let base = base_c.lock().unwrap().clone();
            if path == "/demo-cli" {
                let packument = serde_json::json!({
                    "dist-tags": { "latest": "1.0.0" },
                    "versions": {
                        "1.0.0": {
                            "dist": {
                                "tarball": format!("{base}demo-cli/-/demo-cli-1.0.0.tgz"),
                                "integrity": &*integrity_of(&tgz),
                            }
                        }
                    }
                });
                Some(RouteBody(
                    serde_json::to_vec(&packument).unwrap(),
                    "application/json",
                ))
            } else if path.ends_with(".tgz") {
                Some(RouteBody((*tgz).clone(), "application/gzip"))
            } else {
                None
            }
        });

        *base.lock().unwrap() = server.url("");
        let registry_url = server.url("").trim_end_matches('/').to_string();
        // `integrity` is captured by the closure; keep it alive via the tgz arc.
        let _ = integrity;
        Self {
            _server: server,
            registry_url,
        }
    }
}

/// Build the fixture tarball with files at the **root** (npm layout), declaring
/// two bins (`demo` and `demo-alt`) pointing at two shebang scripts.
#[cfg(unix)]
fn fixture_tgz() -> Vec<u8> {
    build_tgz(|b| {
        common::add_file(
            b,
            "package.json",
            0o644,
            br#"{"name":"demo-cli","version":"1.0.0","bin":{"demo":"./demo.js","demo-alt":"./alt.js"}}"#,
        );
        common::add_file(
            b,
            "demo.js",
            0o755,
            b"#!/usr/bin/env node\nconsole.log('demo');\n",
        );
        common::add_file(
            b,
            "alt.js",
            0o755,
            b"#!/usr/bin/env node\nconsole.log('alt');\n",
        );
    })
}

fn run_install(
    args: &[&str],
    bin_dir: &Path,
    registry: &str,
    store: &Path,
) -> (bool, String, String) {
    let out = Command::new(bin())
        .args(args)
        .env("BPM_BIN", bin_dir)
        .env("BPM_REGISTRY", registry)
        .env("BPM_STORE", store)
        .output()
        .expect("run bpm install");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[cfg(unix)]
#[test]
fn install_pkg_links_all_declared_bins() {
    let reg = RegistryMock::start();
    let store = tempfile::tempdir().unwrap();
    let bin_dir = tempfile::tempdir().unwrap();

    let (ok, stdout, stderr) = run_install(
        &["install", "demo-cli", "--registry", &reg.registry_url],
        bin_dir.path(),
        &reg.registry_url,
        store.path(),
    );
    assert!(ok, "stderr: {stderr}\nstdout: {stdout}");
    assert!(stderr.contains("resolved demo-cli@1.0.0"), "{stderr}");
    assert!(
        stdout.contains("linked 2 bin(s)"),
        "both bins should link: {stdout}"
    );

    // Both symlinks exist in the bin dir and target the extracted image files.
    let demo_link = bin_dir.path().join("demo");
    let alt_link = bin_dir.path().join("demo-alt");
    assert!(demo_link.exists(), "demo bin missing");
    assert!(alt_link.exists(), "demo-alt bin missing");
    // The link points at a real `demo.js` inside the store image and resolves
    // to an executable file.
    let demo_target = std::fs::read_link(&demo_link).unwrap();
    assert!(
        demo_target.ends_with("demo.js"),
        "demo link should target demo.js: {demo_target:?}"
    );
    assert!(
        demo_target.is_file(),
        "demo link target should exist: {demo_target:?}"
    );
}

#[cfg(unix)]
#[test]
fn install_pkg_uses_project_npmrc_for_metadata_and_tarball_requests() {
    let tgz = fixture_tgz();
    let integrity = integrity_of(&tgz);
    let base: std::sync::Arc<std::sync::Mutex<String>> =
        std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let base_c = base.clone();
    let tgz = std::sync::Arc::new(tgz);
    let server = MiniServer::start_routed(move |path| {
        let base = base_c.lock().unwrap().clone();
        if path == "/demo-cli" {
            let packument = serde_json::json!({
                "dist-tags": { "latest": "1.0.0" },
                "versions": {
                    "1.0.0": {
                        "dist": {
                            "tarball": format!("{base}demo-cli/-/demo-cli-1.0.0.tgz"),
                            "integrity": integrity,
                        }
                    }
                }
            });
            Some(RouteBody(
                serde_json::to_vec(&packument).unwrap(),
                "application/json",
            ))
        } else if path.ends_with(".tgz") {
            Some(RouteBody((*tgz).clone(), "application/gzip"))
        } else {
            None
        }
    });
    *base.lock().unwrap() = server.url("");

    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let bin_dir = tempfile::tempdir().unwrap();
    let npmrc = project.path().join(".npmrc");
    let registry = server.url("");
    let authority = registry
        .strip_prefix("http://")
        .unwrap_or(&registry)
        .trim_end_matches('/');
    std::fs::write(
        &npmrc,
        format!("//{authority}/:_authToken=install-secret\n"),
    )
    .unwrap();

    let out = Command::new(bin())
        .args(["install", "demo-cli", "--registry", &registry])
        .env("BPM_BIN", bin_dir.path())
        .env("BPM_STORE", store.path())
        .current_dir(project.path())
        .output()
        .expect("run bpm install");

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "expected metadata and tarball requests");
    assert!(
        requests
            .iter()
            .all(|request| { request.header("authorization") == Some("Bearer install-secret") }),
        "expected configured auth on both metadata and tarball requests: {requests:?}"
    );
    assert_eq!(requests[0].path, "/demo-cli");
    assert!(requests[1].path.ends_with(".tgz"));
}

#[cfg(unix)]
#[test]
fn install_pkg_is_idempotent() {
    let reg = RegistryMock::start();
    let store = tempfile::tempdir().unwrap();
    let bin_dir = tempfile::tempdir().unwrap();
    let args = ["install", "demo-cli", "--registry", &reg.registry_url];

    let (ok1, _, err1) = run_install(&args, bin_dir.path(), &reg.registry_url, store.path());
    assert!(ok1, "{err1}");
    let (ok2, stdout2, err2) = run_install(&args, bin_dir.path(), &reg.registry_url, store.path());
    assert!(ok2, "{err2}");
    // Re-running links the same bins again without error.
    assert!(stdout2.contains("linked 2 bin(s)"), "{stdout2}");
}

#[test]
fn install_pkg_without_bin_fails_clearly() {
    // A package with no `bin` should error with an actionable message.
    let tgz = build_tgz(|b| {
        common::add_file(
            b,
            "package.json",
            0o644,
            br#"{"name":"nope","version":"1.0.0"}"#,
        );
    });
    let base: std::sync::Arc<std::sync::Mutex<String>> =
        std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let base_c = base.clone();
    let tgz = std::sync::Arc::new(tgz);
    let server = MiniServer::start_routed(move |path| {
        let base = base_c.lock().unwrap().clone();
        if path == "/nope" {
            let packument = serde_json::json!({
                "dist-tags": { "latest": "1.0.0" },
                "versions": {
                    "1.0.0": {
                        "dist": {
                            "tarball": format!("{base}nope/-/nope-1.0.0.tgz"),
                            "integrity": &*integrity_of(&tgz),
                        }
                    }
                }
            });
            Some(RouteBody(
                serde_json::to_vec(&packument).unwrap(),
                "application/json",
            ))
        } else if path.ends_with(".tgz") {
            Some(RouteBody((*tgz).clone(), "application/gzip"))
        } else {
            None
        }
    });
    *base.lock().unwrap() = server.url("");
    let registry = server.url("").trim_end_matches('/').to_string();

    let store = tempfile::tempdir().unwrap();
    let bin_dir = tempfile::tempdir().unwrap();
    let (ok, _stdout, stderr) = run_install(
        &["install", "nope", "--registry", &registry],
        bin_dir.path(),
        &registry,
        store.path(),
    );
    assert!(!ok, "should fail when package declares no bin");
    assert!(
        stderr.contains("no `bin`"),
        "error should mention missing bin: {stderr}"
    );
}
