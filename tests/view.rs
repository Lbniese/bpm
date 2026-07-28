//! End-to-end tests for `bpm view` (registry metadata lookup).
//!
//! Each test starts a mock registry on `127.0.0.1` that serves a canned
//! packument, then spawns the built `bpm view` pointed at it via `--registry`.
//! No real network is used.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;

/// Path to the built `bpm` binary.
fn bpm_binary() -> &'static str {
    env!("CARGO_BIN_EXE_bpm")
}

/// A packument with three versions; `2.0.0` is `latest` and is deprecated.
fn rich_packument() -> &'static str {
    r#"{
        "name": "demo-pkg",
        "dist-tags": { "latest": "2.0.0", "next": "2.1.0-beta.1" },
        "versions": {
            "1.0.0": {
                "name": "demo-pkg",
                "version": "1.0.0",
                "dependencies": { "lodash": "^4.17.0" },
                "bin": { "demo": "./cli.js" },
                "engines": { "node": ">=14" },
                "dist": {
                    "tarball": "http://example.test/demo-pkg-1.0.0.tgz",
                    "integrity": "sha512-aaa",
                    "shasum": "11111"
                }
            },
            "2.0.0": {
                "name": "demo-pkg",
                "version": "2.0.0",
                "deprecated": "use demo-pkg2 instead",
                "dependencies": { "lodash": "^4.17.21", "ms": "^2.1.3" },
                "peerDependencies": { "react": ">=16" },
                "bin": { "demo": "./index.js" },
                "engines": { "node": ">=18" },
                "dist": {
                    "tarball": "http://example.test/demo-pkg-2.0.0.tgz",
                    "integrity": "sha512-bbb",
                    "shasum": "22222"
                }
            },
            "2.1.0-beta.1": {
                "name": "demo-pkg",
                "version": "2.1.0-beta.1",
                "dependencies": { "lodash": "^4.17.21" },
                "dist": {
                    "tarball": "http://example.test/demo-pkg-2.1.0-beta.1.tgz",
                    "integrity": "sha512-ccc"
                }
            }
        }
    }"#
}

/// A mock registry keyed by package name; unknown packages get a 404.
/// Returns (registry_url, shutdown_flag, server_thread).
fn mock_registry(
    responses: BTreeMap<String, String>,
) -> (
    String,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
    std::thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let registry_url = format!("http://{address}");
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_shutdown = shutdown.clone();

    let server = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        while !server_shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            let (mut stream, _) = match listener.accept() {
                Ok(c) => c,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
                Err(_) => break,
            };
            stream
                .set_read_timeout(Some(std::time::Duration::from_millis(500)))
                .ok();
            let mut request = [0u8; 4096];
            let n = stream.read(&mut request).unwrap_or(0);
            let request_str = String::from_utf8_lossy(&request[..n]);

            let path = request_str
                .lines()
                .next()
                .and_then(|line| line.split(' ').nth(1).map(|p| p.trim_start_matches('/')))
                .unwrap_or("")
                .split('?')
                .next()
                .unwrap_or("");
            let package_name = path.replace("%2F", "/");

            let (status, body) = match responses.get(&package_name) {
                Some(body) => ("200 OK", body.clone()),
                None => ("404 Not Found", "{\"error\":\"not found\"}".to_string()),
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    (registry_url, shutdown, server)
}

/// Start the demo mock registry. Returns (url, shutdown, thread).
fn demo_registry() -> (
    String,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
    std::thread::JoinHandle<()>,
) {
    let mut responses = BTreeMap::new();
    responses.insert("demo-pkg".to_string(), rich_packument().to_string());
    mock_registry(responses)
}

/// Run `bpm view <args>` in `workdir`, pointed at `registry_url` with a local store.
fn run_view(workdir: &Path, registry_url: &str, args: &[&str]) -> (String, String, Option<i32>) {
    let store = workdir.join("store");
    let store_str = store.to_str().unwrap();
    let mut full: Vec<&str> = vec!["--registry", registry_url, "--store", store_str];
    full.extend_from_slice(args);

    let mut cmd = Command::new(bpm_binary());
    cmd.arg("view").args(&full).current_dir(workdir);
    let output = cmd.output().expect("failed to run bpm view");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code(),
    )
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn shows_default_latest_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let (url, shutdown, server) = demo_registry();

    let (stdout, stderr, code) = run_view(tmp.path(), &url, &["demo-pkg"]);
    shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    server.join().unwrap();

    assert_eq!(code, Some(0), "expected zero exit; stderr: {stderr}");
    assert!(
        stdout.contains("demo-pkg@2.0.0"),
        "expected header, got: {stdout}"
    );
    assert!(
        stdout.contains("DEPRECATED: use demo-pkg2 instead"),
        "expected deprecation, got: {stdout}"
    );
    assert!(
        stdout.contains("lodash: ^4.17.21"),
        "expected dep, got: {stdout}"
    );
    assert!(stdout.contains("ms: ^2.1.3"), "expected dep, got: {stdout}");
    assert!(
        stdout.contains("peerDependencies:") && stdout.contains("react: >=16"),
        "expected peer dep, got: {stdout}"
    );
    assert!(
        stdout.contains("shasum: 22222"),
        "expected shasum, got: {stdout}"
    );
    assert!(
        stdout.contains("versions: 3 published"),
        "expected version count, got: {stdout}"
    );
}

#[test]
fn exact_version_resolves_older_release() {
    let tmp = tempfile::tempdir().unwrap();
    let (url, shutdown, server) = demo_registry();

    let (stdout, stderr, code) = run_view(tmp.path(), &url, &["demo-pkg@1.0.0"]);
    shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    server.join().unwrap();

    assert_eq!(code, Some(0), "expected zero exit; stderr: {stderr}");
    assert!(
        stdout.contains("demo-pkg@1.0.0"),
        "expected 1.0.0, got: {stdout}"
    );
    assert!(
        !stdout.contains("demo-pkg@2.0.0"),
        "expected not latest, got: {stdout}"
    );
    assert!(
        stdout.contains("lodash: ^4.17.0"),
        "expected 1.0.0 dep range, got: {stdout}"
    );
}

#[test]
fn range_resolves_highest_satisfying() {
    let tmp = tempfile::tempdir().unwrap();
    let (url, shutdown, server) = demo_registry();

    let (stdout, stderr, code) = run_view(tmp.path(), &url, &["demo-pkg@^1.0.0"]);
    shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    server.join().unwrap();

    assert_eq!(code, Some(0), "expected zero exit; stderr: {stderr}");
    assert!(
        stdout.contains("demo-pkg@1.0.0"),
        "expected ^1.0.0 -> 1.0.0, got: {stdout}"
    );
    assert!(
        !stdout.contains("demo-pkg@2.0.0"),
        "expected no 2.x, got: {stdout}"
    );
}

#[test]
fn field_versions_lists_all() {
    let tmp = tempfile::tempdir().unwrap();
    let (url, shutdown, server) = demo_registry();

    let (stdout, stderr, code) = run_view(tmp.path(), &url, &["demo-pkg", "versions"]);
    shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    server.join().unwrap();

    assert_eq!(code, Some(0), "expected zero exit; stderr: {stderr}");
    assert!(stdout.contains("1.0.0"), "expected 1.0.0, got: {stdout}");
    assert!(stdout.contains("2.0.0"), "expected 2.0.0, got: {stdout}");
    assert!(
        stdout.contains("2.1.0-beta.1"),
        "expected beta, got: {stdout}"
    );
}

#[test]
fn field_dist_tarball_walks_nested_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (url, shutdown, server) = demo_registry();

    let (stdout, stderr, code) = run_view(tmp.path(), &url, &["demo-pkg", "dist.tarball"]);
    shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    server.join().unwrap();

    assert_eq!(code, Some(0), "expected zero exit; stderr: {stderr}");
    assert!(
        stdout.trim().ends_with("demo-pkg-2.0.0.tgz"),
        "expected latest tarball url, got: {stdout:?}"
    );
}

#[test]
fn field_dependencies_prints_object() {
    let tmp = tempfile::tempdir().unwrap();
    let (url, shutdown, server) = demo_registry();

    let (stdout, stderr, code) = run_view(tmp.path(), &url, &["demo-pkg", "dependencies"]);
    shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    server.join().unwrap();

    assert_eq!(code, Some(0), "expected zero exit; stderr: {stderr}");
    assert!(
        stdout.contains("\"lodash\": \"^4.17.21\"") && stdout.contains("\"ms\": \"^2.1.3\""),
        "expected deps object, got: {stdout}"
    );
}

#[test]
fn json_full_is_structured() {
    let tmp = tempfile::tempdir().unwrap();
    let (url, shutdown, server) = demo_registry();

    let (stdout, stderr, code) = run_view(tmp.path(), &url, &["demo-pkg", "--json"]);
    shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    server.join().unwrap();

    assert_eq!(code, Some(0), "expected zero exit; stderr: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("invalid JSON: {e}; stdout: {stdout}"));
    assert_eq!(value["name"], "demo-pkg");
    assert_eq!(value["version"], "2.0.0");
    assert_eq!(value["deprecated"], "use demo-pkg2 instead");
    assert_eq!(value["dependencies"]["lodash"], "^4.17.21");
    assert_eq!(value["dependencies"]["ms"], "^2.1.3");
    assert_eq!(value["peerDependencies"]["react"], ">=16");
    assert_eq!(
        value["dist"]["tarball"],
        "http://example.test/demo-pkg-2.0.0.tgz"
    );
    assert_eq!(value["dist"]["shasum"], "22222");
}

#[test]
fn json_field_prints_just_that_value() {
    let tmp = tempfile::tempdir().unwrap();
    let (url, shutdown, server) = demo_registry();

    let (stdout, stderr, code) =
        run_view(tmp.path(), &url, &["demo-pkg", "dependencies", "--json"]);
    shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    server.join().unwrap();

    assert_eq!(code, Some(0), "expected zero exit; stderr: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("invalid JSON: {e}; stdout: {stdout}"));
    assert_eq!(value["lodash"], "^4.17.21");
}

#[test]
fn errors_when_package_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let (url, shutdown, server) = demo_registry();

    let (_stdout, stderr, code) = run_view(tmp.path(), &url, &["no-such-package"]);
    shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    server.join().unwrap();

    assert_ne!(code, Some(0), "expected non-zero exit code");
    assert!(
        stderr.contains("not found"),
        "expected 'not found', got: {stderr}"
    );
}

#[test]
fn errors_when_field_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let (url, shutdown, server) = demo_registry();

    let (_stdout, stderr, code) = run_view(tmp.path(), &url, &["demo-pkg", "nonexistent-field"]);
    shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    server.join().unwrap();

    assert_ne!(code, Some(0), "expected non-zero exit code");
    assert!(
        stderr.contains("no field 'nonexistent-field'"),
        "expected no-field error, got: {stderr}"
    );
}

#[test]
fn errors_when_offline_without_cache() {
    let tmp = tempfile::tempdir().unwrap();

    // No mock needed: --offline must not contact any registry.
    let store = tmp.path().join("store");
    let mut cmd = Command::new(bpm_binary());
    cmd.arg("view")
        .args(["--store", store.to_str().unwrap(), "--offline", "demo-pkg"])
        .current_dir(tmp.path());
    let output = cmd.output().expect("failed to run bpm view");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert_ne!(output.status.code(), Some(0), "expected non-zero exit code");
    assert!(
        stderr.contains("offline"),
        "expected offline error, got: {stderr}"
    );
}

#[test]
fn errors_when_spec_is_invalid() {
    let tmp = tempfile::tempdir().unwrap();
    let (url, shutdown, server) = demo_registry();

    // ".bad-name" is not a valid npm package name.
    let (_stdout, stderr, code) = run_view(tmp.path(), &url, &[".bad-name"]);
    shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    server.join().unwrap();

    assert_ne!(code, Some(0), "expected non-zero exit code");
    assert!(
        stderr.contains("invalid package spec"),
        "expected invalid-spec error, got: {stderr}"
    );
}
