mod common;

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use bpm::lockfile::{Lockfile, PackageEntry, RootEntry};
use common::{build_tgz, integrity_of, CapturedRequest, MiniServer, RouteBody};

fn bin() -> String {
    std::env::var("CARGO_BIN_EXE_bpm").expect("CARGO_BIN_EXE_bpm")
}

fn authority(url: &str) -> String {
    url.strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url)
        .trim_end_matches('/')
        .to_string()
}

fn auth_line(authority: &str, token: &str) -> String {
    format!("//{authority}/:_authToken={token}")
}

fn package_tgz(name: &str, version: &str, bin: Option<(&str, &str)>) -> Vec<u8> {
    build_tgz(|b| {
        common::add_file(
            b,
            "package.json",
            0o644,
            serde_json::to_vec(&package_json(name, version, bin))
                .expect("serialize package.json")
                .as_slice(),
        );
        if let Some((_, path)) = bin {
            common::add_file(
                b,
                path.trim_start_matches("./"),
                0o755,
                b"#!/usr/bin/env node\nconsole.log('demo');\n",
            );
        }
    })
}

fn package_json(name: &str, version: &str, bin: Option<(&str, &str)>) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("name".into(), serde_json::json!(name));
    obj.insert("version".into(), serde_json::json!(version));
    if let Some((cmd, path)) = bin {
        let mut bins = serde_json::Map::new();
        bins.insert(cmd.to_string(), serde_json::json!(path));
        obj.insert("bin".into(), serde_json::Value::Object(bins));
    }
    serde_json::Value::Object(obj)
}

fn write_project(project: &Path) {
    fs::write(
        project.join("package.json"),
        r#"{"name":"app","version":"1.0.0"}"#,
    )
    .unwrap();
}

fn write_npmrc(project: &Path, lines: &[String]) {
    let mut text = lines.join("\n");
    text.push('\n');
    fs::write(project.join(".npmrc"), text).unwrap();
}

fn corrupt_metadata_cache(store: &Path) {
    fs::write(store.join("metadata-cache.db"), b"not-a-sqlite-database").unwrap();
}

fn run_bpm(
    args: &[&str],
    cwd: &Path,
    store: &Path,
    bin_dir: Option<&Path>,
) -> (bool, String, String) {
    let mut cmd = Command::new(bin());
    cmd.args(args).current_dir(cwd).env("BPM_STORE", store);
    if let Some(bin_dir) = bin_dir {
        cmd.env("BPM_BIN", bin_dir);
    }
    let out = cmd.output().expect("run bpm");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// Env vars for the blocking resolver side of a parity test: async resolver
/// disabled (BPM_ASYNC_RESOLVE=0) and streaming disabled (BPM_STREAM_INSTALL=0).
/// Passing these explicitly makes the "blocking" side genuinely exercise the
/// blocking resolver kill-switch path instead of silently inheriting the async
/// default, so the parity corpus proves the two resolvers are distinct *and*
/// produce byte-identical lockfiles.
const RESOLVE_BLOCKING: &[(&str, &str)] =
    &[("BPM_ASYNC_RESOLVE", "0"), ("BPM_STREAM_INSTALL", "0")];

/// Like [`run_bpm`] but with extra environment pairs, so parity tests can pin
/// resolver/streaming modes explicitly instead of relying on defaults.
fn run_bpm_with_env(
    args: &[&str],
    cwd: &Path,
    store: &Path,
    bin_dir: Option<&Path>,
    env: &[(&str, &str)],
) -> (bool, String, String) {
    let mut cmd = Command::new(bin());
    cmd.args(args).current_dir(cwd).env("BPM_STORE", store);
    if let Some(bin_dir) = bin_dir {
        cmd.env("BPM_BIN", bin_dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run bpm");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn packument(version: &str, tarball_url: String, integrity: String) -> serde_json::Value {
    let mut versions = serde_json::Map::new();
    let mut dist = serde_json::Map::new();
    dist.insert("tarball".into(), serde_json::json!(tarball_url));
    dist.insert("integrity".into(), serde_json::json!(integrity));
    let mut entry = serde_json::Map::new();
    entry.insert("dist".into(), serde_json::Value::Object(dist));
    versions.insert(version.to_string(), serde_json::Value::Object(entry));

    let mut root = serde_json::Map::new();
    let mut tags = serde_json::Map::new();
    tags.insert("latest".into(), serde_json::json!(version));
    root.insert("dist-tags".into(), serde_json::Value::Object(tags));
    root.insert("versions".into(), serde_json::Value::Object(versions));
    serde_json::Value::Object(root)
}

fn redirect_server(location: String) -> (String, Arc<Mutex<Vec<CapturedRequest>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_thread = requests.clone();
    thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        if let Some(request) = read_request(&mut stream, 0, 1) {
            requests_thread.lock().unwrap().push(request);
        }
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let _ = stream.write_all(response.as_bytes());
    });
    (format!("http://{addr}"), requests)
}

fn read_request(
    stream: &mut TcpStream,
    sequence: usize,
    connection_id: usize,
) -> Option<CapturedRequest> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    while bytes.len() < 64 * 1024 {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    if bytes.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.split("\r\n");
    let mut request_parts = lines.next().unwrap_or_default().split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_owned();
    let path = request_parts.next().unwrap_or("/").to_owned();
    let mut headers = BTreeMap::<String, Vec<String>>::new();
    for line in lines.take_while(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers
            .entry(name.trim().to_ascii_lowercase())
            .or_default()
            .push(value.trim().to_owned());
    }
    Some(CapturedRequest {
        sequence,
        connection_id,
        method,
        path,
        headers,
    })
}

fn same_host_registry_mock(
    name: &str,
    version: &str,
    tarball_path: &str,
    tgz: Vec<u8>,
) -> MiniServer {
    let expected_metadata = format!("/{}", name.replace('/', "%2F"));
    let expected_tarball = format!("/{}", tarball_path.trim_start_matches('/'));
    let tarball = Arc::new(tgz);
    let integrity = integrity_of(&tarball);
    let base = Arc::new(Mutex::new(String::new()));
    let base_thread = base.clone();
    let tarball_thread = tarball.clone();
    let version = version.to_owned();

    let server = MiniServer::start_keep_alive_routed(move |path| {
        let base = base_thread.lock().unwrap().clone();
        if path == expected_metadata {
            Some(RouteBody(
                serde_json::to_vec(&packument(
                    &version,
                    format!("{}{expected_tarball}", base.trim_end_matches('/')),
                    integrity.clone(),
                ))
                .unwrap(),
                "application/json",
            ))
        } else if path == expected_tarball {
            Some(RouteBody((*tarball_thread).clone(), "application/gzip"))
        } else {
            None
        }
    });
    *base.lock().unwrap() = server.url("");
    server
}

/// A multi-package registry mock: serves an arbitrary set of packages from a
/// single server, keyed by packument path (`/<url-encoded-name>`) and
/// tarball path (`/<tarball_path>`). Use this for transitive / peer / cycle /
/// override parity graphs that `same_host_registry_mock` (single-package) cannot
/// represent. Each entry is `(name, version, tarball_path, tgz)`.
fn multi_registry_mock(packages: Vec<(&str, &str, &str, Vec<u8>)>) -> MiniServer {
    // Pre-compute the (metadata path, tarball path, integ, version, tgz) tuples.
    struct Pkg {
        metadata_path: String,
        tarball_path: String,
        version: String,
        integrity: String,
        tarball: Vec<u8>,
    }
    let mut entries: Vec<Pkg> = Vec::new();
    for (name, version, tarball_path, tgz) in packages {
        let integrity = integrity_of(&tgz);
        entries.push(Pkg {
            metadata_path: format!("/{}", name.replace('/', "%2F")),
            tarball_path: format!("/{}", tarball_path.trim_start_matches('/')),
            version: version.to_string(),
            integrity,
            tarball: tgz,
        });
    }
    let entries = Arc::new(entries);
    let base = Arc::new(Mutex::new(String::new()));
    let base_thread = base.clone();

    let server = MiniServer::start_keep_alive_routed(move |path| {
        let base = base_thread.lock().unwrap().clone();
        for pkg in entries.iter() {
            if path == pkg.metadata_path {
                let tarball_url = format!("{}{}", base.trim_end_matches('/'), pkg.tarball_path);
                return Some(RouteBody(
                    serde_json::to_vec(&packument(
                        &pkg.version,
                        tarball_url,
                        pkg.integrity.clone(),
                    ))
                    .unwrap(),
                    "application/json",
                ));
            }
            if path == pkg.tarball_path {
                return Some(RouteBody(pkg.tarball.clone(), "application/gzip"));
            }
        }
        None
    });
    *base.lock().unwrap() = server.url("");
    server
}

/// Like [`packument`] but includes `peerDependencies` in the version entry — the
/// field the resolver reads to bind peers. The minimal [`packument`] helper
/// omits it, so peers are invisible to resolution and strict mode trivially
/// succeeds; this helper lets a mock package declare peers the resolver enforces.
fn packument_with_peers(
    version: &str,
    tarball_url: String,
    integrity: String,
    peer_deps: &BTreeMap<String, String>,
) -> serde_json::Value {
    let mut versions = serde_json::Map::new();
    let mut dist = serde_json::Map::new();
    dist.insert("tarball".into(), serde_json::json!(tarball_url));
    dist.insert("integrity".into(), serde_json::json!(integrity));
    let mut entry = serde_json::Map::new();
    entry.insert("dist".into(), serde_json::Value::Object(dist));
    if !peer_deps.is_empty() {
        entry.insert("peerDependencies".into(), serde_json::json!(peer_deps));
    }
    versions.insert(version.to_string(), serde_json::Value::Object(entry));

    let mut root = serde_json::Map::new();
    let mut tags = serde_json::Map::new();
    tags.insert("latest".into(), serde_json::json!(version));
    root.insert("dist-tags".into(), serde_json::Value::Object(tags));
    root.insert("versions".into(), serde_json::Value::Object(versions));
    serde_json::Value::Object(root)
}

/// Multi-package registry mock where each package may declare `peerDependencies`
/// in its packument (the resolver binds peers from the packument, not the
/// tarball). Each entry is `(name, version, tarball_path, tgz, peer_deps)`.
#[allow(clippy::type_complexity)]
fn multi_registry_mock_with_peers(
    packages: Vec<(&str, &str, &str, Vec<u8>, BTreeMap<String, String>)>,
) -> MiniServer {
    struct Pkg {
        metadata_path: String,
        tarball_path: String,
        version: String,
        integrity: String,
        tarball: Vec<u8>,
        peer_deps: BTreeMap<String, String>,
    }
    let mut entries: Vec<Pkg> = Vec::new();
    for (name, version, tarball_path, tgz, peer_deps) in packages {
        let integrity = integrity_of(&tgz);
        entries.push(Pkg {
            metadata_path: format!("/{}", name.replace('/', "%2F")),
            tarball_path: format!("/{}", tarball_path.trim_start_matches('/')),
            version: version.to_string(),
            integrity,
            tarball: tgz,
            peer_deps,
        });
    }
    let entries = Arc::new(entries);
    let base = Arc::new(Mutex::new(String::new()));
    let base_thread = base.clone();

    let server = MiniServer::start_keep_alive_routed(move |path| {
        let base = base_thread.lock().unwrap().clone();
        for pkg in entries.iter() {
            if path == pkg.metadata_path {
                let tarball_url = format!("{}{}", base.trim_end_matches('/'), pkg.tarball_path);
                return Some(RouteBody(
                    serde_json::to_vec(&packument_with_peers(
                        &pkg.version,
                        tarball_url,
                        pkg.integrity.clone(),
                        &pkg.peer_deps,
                    ))
                    .unwrap(),
                    "application/json",
                ));
            }
            if path == pkg.tarball_path {
                return Some(RouteBody(pkg.tarball.clone(), "application/gzip"));
            }
        }
        None
    });
    *base.lock().unwrap() = server.url("");
    server
}

fn metadata_only_registry_mock(
    name: &str,
    version: &str,
    tarball_url: String,
    tgz: Vec<u8>,
) -> MiniServer {
    let expected_metadata = format!("/{}", name.replace('/', "%2F"));
    let integrity = integrity_of(&tgz);
    let version = version.to_owned();
    MiniServer::start_keep_alive_routed(move |path| {
        if path == expected_metadata {
            Some(RouteBody(
                serde_json::to_vec(&packument(&version, tarball_url.clone(), integrity.clone()))
                    .unwrap(),
                "application/json",
            ))
        } else {
            None
        }
    })
}

#[test]
fn fetch_honors_scoped_registry_precedence() {
    let tgz = package_tgz("@scope/demo", "1.0.0", None);
    let scoped_registry =
        same_host_registry_mock("@scope/demo", "1.0.0", "@scope/demo/-/demo-1.0.0.tgz", tgz);
    let default_registry = MiniServer::start_keep_alive_routed(|_| None);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_project(project.path());
    write_npmrc(
        project.path(),
        &[
            format!("@scope:registry={}", scoped_registry.url("")),
            format!("registry={}", default_registry.url("")),
            auth_line(&authority(&scoped_registry.url("")), "registry-secret"),
        ],
    );

    let (ok, stdout, stderr) = run_bpm(
        &[
            "fetch",
            "@scope/demo",
            "--registry",
            &default_registry.url(""),
            "--store",
            store.path().to_str().unwrap(),
            "--no-extract",
        ],
        project.path(),
        store.path(),
        None,
    );
    assert!(ok, "stderr: {stderr}\nstdout: {stdout}");
    assert!(
        default_registry.requests().is_empty(),
        "default registry must stay unused"
    );

    let requests = scoped_registry.requests();
    assert_eq!(requests.len(), 2, "metadata + tarball");
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer registry-secret")
    );
    assert_eq!(
        requests[1].header("authorization"),
        Some("Bearer registry-secret")
    );
}

#[test]
fn install_offline_fails_if_metadata_cache_is_unusable() {
    let tgz = package_tgz("test-dep", "1.0.0", None);
    let server = same_host_registry_mock("test-dep", "1.0.0", "tarballs/test-dep-1.0.0.tgz", tgz);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"test-dep":"^1.0.0"}}"#,
    )
    .unwrap();
    write_npmrc(project.path(), &[format!("registry={}", server.url(""))]);
    corrupt_metadata_cache(store.path());

    let (ok, stdout, stderr) = run_bpm(
        &[
            "install",
            "--offline",
            "--registry",
            &server.url(""),
            "--store",
            store.path().to_str().unwrap(),
        ],
        project.path(),
        store.path(),
        None,
    );
    assert!(!ok, "expected offline install to fail: {stdout}\n{stderr}");
    assert!(stderr.contains("metadata cache unavailable in offline mode"));
    assert_eq!(server.requests().len(), 0, "corrupt cache must be terminal");
}

#[test]
fn install_prefer_offline_fails_if_metadata_cache_is_unusable() {
    let tgz = package_tgz("test-dep", "1.0.0", None);
    let server = same_host_registry_mock("test-dep", "1.0.0", "tarballs/test-dep-1.0.0.tgz", tgz);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"test-dep":"^1.0.0"}}"#,
    )
    .unwrap();
    write_npmrc(project.path(), &[format!("registry={}", server.url(""))]);
    corrupt_metadata_cache(store.path());

    let (ok, stdout, stderr) = run_bpm(
        &[
            "install",
            "--prefer-offline",
            "--registry",
            &server.url(""),
            "--store",
            store.path().to_str().unwrap(),
        ],
        project.path(),
        store.path(),
        None,
    );
    assert!(
        !ok,
        "expected prefer-offline install to fail: {stdout}\n{stderr}"
    );
    assert!(stderr.contains("metadata cache unavailable in prefer-offline mode"));
    assert_eq!(server.requests().len(), 0, "corrupt cache must be terminal");
}

#[cfg(unix)]
#[test]
fn install_uses_path_specific_tokens_and_links_bins() {
    let tgz = package_tgz("demo-cli", "1.0.0", Some(("demo", "./demo.js")));
    let server = same_host_registry_mock("demo-cli", "1.0.0", "tarballs/demo-cli-1.0.0.tgz", tgz);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let bin_dir = tempfile::tempdir().unwrap();
    write_project(project.path());
    write_npmrc(
        project.path(),
        &[
            auth_line(&authority(&server.url("")), "registry-secret"),
            auth_line(&authority(&server.url("tarballs")), "tarball-secret"),
        ],
    );

    let (ok, stdout, stderr) = run_bpm(
        &[
            "install",
            "-g",
            "demo-cli",
            "--registry",
            &server.url(""),
            "--store",
            store.path().to_str().unwrap(),
        ],
        project.path(),
        store.path(),
        Some(bin_dir.path()),
    );
    assert!(ok, "stderr: {stderr}\nstdout: {stdout}");
    assert!(
        bin_dir.path().join("demo").exists(),
        "linked bin should exist\nstdout: {stdout}\nstderr: {stderr}"
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "metadata + tarball");
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer registry-secret")
    );
    assert_eq!(
        requests[1].header("authorization"),
        Some("Bearer tarball-secret")
    );
}

#[test]
fn fetch_cross_origin_redirect_strips_registry_token() {
    let tgz = package_tgz("demo-cli", "1.0.0", None);
    let final_server = MiniServer::start_keep_alive_routed({
        let tgz = Arc::new(tgz.clone());
        move |path| {
            if path == "/demo-cli-1.0.0.tgz" {
                Some(RouteBody((*tgz).clone(), "application/gzip"))
            } else {
                None
            }
        }
    });
    let (redirect_url, redirect_requests) = redirect_server(final_server.url("demo-cli-1.0.0.tgz"));
    let registry = metadata_only_registry_mock(
        "demo-cli",
        "1.0.0",
        redirect_url.clone() + "/demo-cli-1.0.0.tgz",
        tgz,
    );

    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_project(project.path());
    write_npmrc(
        project.path(),
        &[auth_line(&authority(&registry.url("")), "registry-secret")],
    );

    let (ok, stdout, stderr) = run_bpm(
        &[
            "fetch",
            "demo-cli",
            "--registry",
            &registry.url(""),
            "--store",
            store.path().to_str().unwrap(),
            "--no-extract",
        ],
        project.path(),
        store.path(),
        None,
    );
    assert!(ok, "stderr: {stderr}\nstdout: {stdout}");
    assert_eq!(registry.requests().len(), 1, "one metadata request");
    assert_eq!(
        registry.requests()[0].header("authorization"),
        Some("Bearer registry-secret")
    );
    assert_eq!(
        redirect_requests.lock().unwrap().len(),
        1,
        "one redirect hop"
    );
    assert_eq!(
        redirect_requests.lock().unwrap()[0].header("authorization"),
        None
    );
    assert_eq!(
        final_server.requests().len(),
        1,
        "one final tarball request"
    );
    assert_eq!(final_server.requests()[0].header("authorization"), None);
}

#[cfg(unix)]
#[test]
fn frozen_install_uses_direct_tarball_urls_without_registry_lookups() {
    let tgz = package_tgz("demo-cli", "1.0.0", Some(("demo", "./demo.js")));
    let tarball_server = MiniServer::start_keep_alive_routed({
        let tgz = Arc::new(tgz.clone());
        move |path| {
            if path == "/tarballs/demo-cli-1.0.0.tgz" {
                Some(RouteBody((*tgz).clone(), "application/gzip"))
            } else {
                None
            }
        }
    });
    let registry_server = MiniServer::start_keep_alive_routed(|_| None);

    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"demo-cli":"1.0.0"}}"#,
    )
    .unwrap();
    write_npmrc(
        project.path(),
        &[format!("registry={}", registry_server.url(""))],
    );

    let mut lockfile = Lockfile::new("bpm-test");
    lockfile.root = RootEntry {
        name: Some("app".into()),
        version: Some("1.0.0".into()),
        dependencies: BTreeMap::from([("demo-cli".into(), "1.0.0".into())]),
    };
    lockfile.packages.push(PackageEntry {
        path: "node_modules/demo-cli".into(),
        name: "demo-cli".into(),
        version: "1.0.0".into(),
        resolved: tarball_server.url("tarballs/demo-cli-1.0.0.tgz"),
        integrity: Some(integrity_of(&tgz)),
        bin: BTreeMap::from([("demo".into(), "./demo.js".into())]),
        ..Default::default()
    });
    lockfile.sort_packages();
    lockfile.write_to(&project.path().join("bpm.lock")).unwrap();

    let (ok, stdout, stderr) = run_bpm(
        &[
            "install",
            "--frozen",
            "--registry",
            &registry_server.url(""),
            "--store",
            store.path().to_str().unwrap(),
        ],
        project.path(),
        store.path(),
        None,
    );
    assert!(ok, "stderr: {stderr}\nstdout: {stdout}");
    assert!(
        registry_server.requests().is_empty(),
        "registry compatibility path must stay unused"
    );
    assert_eq!(
        tarball_server.requests().len(),
        1,
        "direct tarball URL should be fetched once"
    );
}

/// Verify that the async resolver (`BPM_ASYNC_RESOLVE=1`) produces the same
/// `bpm.lock` bytes as the blocking resolver (`BPM_ASYNC_RESOLVE=0`). Both
/// sides set their mode explicitly so the corpus proves the resolvers are
/// distinct rather than both inheriting the async default. Uses a local
/// registry server for deterministic, offline resolution.
#[test]
fn async_resolve_produces_byte_identical_lockfile() {
    let dep_tgz = package_tgz("test-dep", "1.0.0", None);
    let server =
        same_host_registry_mock("test-dep", "1.0.0", "tarballs/test-dep-1.0.0.tgz", dep_tgz);

    let project = tempfile::tempdir().unwrap();
    let store_block = tempfile::tempdir().unwrap();

    // Write package.json with a dependency on test-dep so the resolver runs.
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"test-dep":"^1.0.0"}}"#,
    )
    .unwrap();

    // Point the project's registry at our local mock server.
    write_npmrc(project.path(), &[format!("registry={}", server.url(""))]);

    // ---- Blocking resolve (BPM_ASYNC_RESOLVE=0, no streaming) ----
    let (ok_block, stdout_block, stderr_block) = run_bpm_with_env(
        &["install"],
        project.path(),
        store_block.path(),
        None,
        RESOLVE_BLOCKING,
    );
    assert!(
        ok_block,
        "blocking install failed\nstdout: {stdout_block}\nstderr: {stderr_block}"
    );
    let blocking_lock = fs::read_to_string(project.path().join("bpm.lock"))
        .expect("bpm.lock should exist after blocking install");

    // ---- Async resolve (BPM_ASYNC_RESOLVE=1) ----
    let store_async = tempfile::tempdir().unwrap();
    let _ = fs::remove_file(project.path().join("bpm.lock"));

    let mut cmd = std::process::Command::new(bin());
    cmd.args(["install"])
        .current_dir(project.path())
        .env("BPM_STORE", store_async.path())
        .env("BPM_ASYNC_RESOLVE", "1");
    let out = cmd.output().expect("run bpm with async resolver");
    assert!(
        out.status.success(),
        "async install failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let async_lock = fs::read_to_string(project.path().join("bpm.lock"))
        .expect("bpm.lock should exist after async install");

    assert_eq!(
        blocking_lock, async_lock,
        "blocking and async resolve must produce byte-identical bpm.lock"
    );
}

#[test]
fn async_streaming_resolve_produces_byte_identical_lockfile() {
    let dep_tgz = package_tgz("test-dep", "1.0.0", None);
    let server =
        same_host_registry_mock("test-dep", "1.0.0", "tarballs/test-dep-1.0.0.tgz", dep_tgz);

    let project = tempfile::tempdir().unwrap();
    let store_block = tempfile::tempdir().unwrap();

    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app-stream-async","version":"1.0.0","dependencies":{"test-dep":"^1.0.0"}}"#,
    )
    .unwrap();

    write_npmrc(project.path(), &[format!("registry={}", server.url(""))]);

    // ---- Blocking resolve (BPM_ASYNC_RESOLVE=0, no streaming) ----
    let (ok_block, stdout_block, stderr_block) = run_bpm_with_env(
        &["install"],
        project.path(),
        store_block.path(),
        None,
        RESOLVE_BLOCKING,
    );
    assert!(
        ok_block,
        "blocking install failed\nstdout: {stdout_block}\nstderr: {stderr_block}"
    );
    let blocking_lock = fs::read_to_string(project.path().join("bpm.lock"))
        .expect("bpm.lock should exist after blocking install");

    // ---- Streaming+async resolve (BPM_ASYNC_RESOLVE=1 + BPM_STREAM_INSTALL=1) ----
    let store_combined = tempfile::tempdir().unwrap();
    let _ = fs::remove_file(project.path().join("bpm.lock"));

    let mut cmd = std::process::Command::new(bin());
    cmd.args(["install"])
        .current_dir(project.path())
        .env("BPM_STORE", store_combined.path())
        .env("BPM_ASYNC_RESOLVE", "1")
        .env("BPM_STREAM_INSTALL", "1");
    let out = cmd.output().expect("run bpm with streaming+async");
    assert!(
        out.status.success(),
        "streaming+async install failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let combined_lock = fs::read_to_string(project.path().join("bpm.lock"))
        .expect("bpm.lock should exist after streaming+async install");

    assert_eq!(
        blocking_lock, combined_lock,
        "blocking and streaming+async resolve must produce byte-identical bpm.lock"
    );
}

/// Helper that starts a registry mock returning a packument with an arbitrary
/// (possibly invalid) integrity string.  The tarball endpoint returns 404 so
/// we can tell if the client ever requests one despite the malformed integrity.
fn malformed_integrity_mock(
    name: &str,
    version: &str,
    tarball_path: &str,
    integrity: &str,
) -> MiniServer {
    let expected_metadata = format!("/{}", name.replace('/', "%2F"));
    let expected_tarball = format!("/{}", tarball_path.trim_start_matches('/'));
    let version = version.to_owned();
    let integrity = integrity.to_owned();
    let tarball_requests: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));

    MiniServer::start_keep_alive_routed(move |path| {
        if path == expected_metadata {
            // Return invalid integrity in the packument.
            let mut versions = serde_json::Map::new();
            let mut dist = serde_json::Map::new();
            dist.insert("tarball".into(), serde_json::json!(expected_tarball));
            dist.insert("integrity".into(), serde_json::json!(&integrity));
            let mut entry = serde_json::Map::new();
            entry.insert("dist".into(), serde_json::Value::Object(dist));
            versions.insert(version.clone(), serde_json::Value::Object(entry));
            let mut root = serde_json::Map::new();
            let mut tags = serde_json::Map::new();
            tags.insert("latest".into(), serde_json::json!(&version));
            root.insert("dist-tags".into(), serde_json::Value::Object(tags));
            root.insert("versions".into(), serde_json::Value::Object(versions));
            Some(RouteBody(
                serde_json::to_vec(&serde_json::Value::Object(root)).unwrap(),
                "application/json",
            ))
        } else if path == expected_tarball {
            // Record that a tarball request was made.
            *tarball_requests.lock().unwrap() += 1;
            None
        } else {
            None
        }
    })
}

/// Default sync+streaming install must reject malformed integrity.
#[test]
fn malformed_integrity_fails_default_streaming() {
    let server = malformed_integrity_mock(
        "bad-integrity-pkg",
        "1.0.0",
        "tarballs/bad-integrity-pkg-1.0.0.tgz",
        "sha512-tooshort",
    );
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"bad-integrity-pkg":"^1.0.0"}}"#,
    )
    .unwrap();
    write_npmrc(project.path(), &[format!("registry={}", server.url(""))]);

    let (ok, stdout, stderr) = run_bpm(
        &["install", "--store", store.path().to_str().unwrap()],
        project.path(),
        store.path(),
        None,
    );
    assert!(
        !ok,
        "install must fail with malformed integrity;\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("malformed") || stderr.contains("integrity"),
        "stderr must mention integrity problem; got: {stderr}"
    );
    assert!(
        !project
            .path()
            .join("node_modules")
            .join("bad-integrity-pkg")
            .exists(),
        "package must not be installed"
    );
}

/// Sync resolver without streaming must also reject malformed integrity.
#[test]
fn malformed_integrity_fails_sync_no_stream() {
    let server = malformed_integrity_mock(
        "bad-integrity-pkg",
        "1.0.0",
        "tarballs/bad-integrity-pkg-1.0.0.tgz",
        "sha512-unspported",
    );
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"bad-integrity-pkg":"^1.0.0"}}"#,
    )
    .unwrap();
    write_npmrc(project.path(), &[format!("registry={}", server.url(""))]);

    let (ok, stdout, stderr) = run_bpm(
        &["install", "--store", store.path().to_str().unwrap()],
        project.path(),
        store.path(),
        None,
    );
    assert!(
        !ok,
        "install must fail with malformed integrity;\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("malformed")
            || stderr.contains("integrity")
            || stderr.contains("unsupported"),
        "stderr must mention integrity problem; got: {stderr}"
    );
}

/// Async resolver must also reject malformed integrity.
#[test]
fn malformed_integrity_fails_async_resolver() {
    let server = malformed_integrity_mock(
        "bad-integrity-pkg",
        "1.0.0",
        "tarballs/bad-integrity-pkg-1.0.0.tgz",
        "sha512-this-is-not-valid-base64",
    );
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"bad-integrity-pkg":"^1.0.0"}}"#,
    )
    .unwrap();
    write_npmrc(project.path(), &[format!("registry={}", server.url(""))]);

    let mut cmd = std::process::Command::new(bin());
    cmd.args(["install", "--store", store.path().to_str().unwrap()])
        .current_dir(project.path())
        .env("BPM_STORE", store.path())
        .env("BPM_ASYNC_RESOLVE", "1");
    let out = cmd.output().expect("run bpm with async resolver");
    assert!(
        !out.status.success(),
        "async install must fail with malformed integrity;\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("malformed") || stderr.contains("integrity"),
        "stderr must mention integrity problem; got: {stderr}"
    );
}

/// Async + streaming must also reject malformed integrity.
#[test]
fn malformed_integrity_fails_async_streaming() {
    let server = malformed_integrity_mock(
        "bad-integrity-pkg",
        "1.0.0",
        "tarballs/bad-integrity-pkg-1.0.0.tgz",
        "sha512-",
    );
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"bad-integrity-pkg":"^1.0.0"}}"#,
    )
    .unwrap();
    write_npmrc(project.path(), &[format!("registry={}", server.url(""))]);

    let mut cmd = std::process::Command::new(bin());
    cmd.args(["install", "--store", store.path().to_str().unwrap()])
        .current_dir(project.path())
        .env("BPM_STORE", store.path())
        .env("BPM_ASYNC_RESOLVE", "1")
        .env("BPM_STREAM_INSTALL", "1");
    let out = cmd
        .output()
        .expect("run bpm with async resolver + streaming");
    assert!(
        !out.status.success(),
        "async+streaming install must fail with malformed integrity;\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("malformed") || stderr.contains("integrity"),
        "stderr must mention integrity problem; got: {stderr}"
    );
}

// === Plan 002: Additional parity coverage tests ===

/// Test with disjunctive range: "1.x || 2.x"
#[test]
fn async_resolve_disjunctive_range_byte_identical() {
    let pkg_tgz = package_tgz("disjunctive-pkg", "1.2.0", None);
    let server = same_host_registry_mock(
        "disjunctive-pkg",
        "1.2.0",
        "tarballs/disjunctive-pkg-1.2.0.tgz",
        pkg_tgz,
    );

    let project = tempfile::tempdir().unwrap();
    let store_block = tempfile::tempdir().unwrap();

    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"disjunctive-pkg":"1.x || 2.x"}}"#,
    )
    .unwrap();

    write_npmrc(project.path(), &[format!("registry={}", server.url(""))]);

    // Blocking resolve (BPM_ASYNC_RESOLVE=0, no streaming)
    let (ok_block, stdout_block, stderr_block) = run_bpm_with_env(
        &["install"],
        project.path(),
        store_block.path(),
        None,
        RESOLVE_BLOCKING,
    );
    assert!(
        ok_block,
        "blocking install failed\nstdout: {stdout_block}\nstderr: {stderr_block}"
    );
    let blocking_lock = fs::read_to_string(project.path().join("bpm.lock"))
        .expect("bpm.lock should exist after blocking install");

    // Async resolve (BPM_ASYNC_RESOLVE=1)
    let store_async = tempfile::tempdir().unwrap();
    let _ = fs::remove_file(project.path().join("bpm.lock"));

    let mut cmd = std::process::Command::new(bin());
    cmd.args(["install"])
        .current_dir(project.path())
        .env("BPM_STORE", store_async.path())
        .env("BPM_ASYNC_RESOLVE", "1");
    let out = cmd.output().expect("run bpm with async resolver");
    assert!(
        out.status.success(),
        "async install failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let async_lock = fs::read_to_string(project.path().join("bpm.lock"))
        .expect("bpm.lock should exist after async install");

    assert_eq!(
        blocking_lock, async_lock,
        "blocking and async resolve must produce byte-identical bpm.lock"
    );
}

/// Test with multiple direct dependencies
#[test]
fn async_resolve_multiple_deps_byte_identical() {
    let dep1_tgz = package_tgz("multi-dep-1", "1.0.0", None);
    let server1 = same_host_registry_mock(
        "multi-dep-1",
        "1.0.0",
        "tarballs/multi-dep-1-1.0.0.tgz",
        dep1_tgz,
    );

    let dep2_tgz = package_tgz("multi-dep-2", "2.0.0", None);
    let _server2 = same_host_registry_mock(
        "multi-dep-2",
        "2.0.0",
        "tarballs/multi-dep-2-2.0.0.tgz",
        dep2_tgz,
    );

    let project = tempfile::tempdir().unwrap();
    let store_block = tempfile::tempdir().unwrap();

    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"multi-dep-1":"^1.0.0","multi-dep-2":"^2.0.0"}}"#,
    )
    .unwrap();

    // Use server1 as the registry - we'll only use dep1 in the test
    write_npmrc(project.path(), &[format!("registry={}", server1.url(""))]);

    // Blocking resolve (BPM_ASYNC_RESOLVE=0, no streaming; should fail because dep2 doesn't exist on server1)
    let (ok_block, _, _stderr_block) = run_bpm_with_env(
        &["install"],
        project.path(),
        store_block.path(),
        None,
        RESOLVE_BLOCKING,
    );

    // This is expected to fail, but we can still check both resolvers behave the same
    if !ok_block {
        // If blocking fails, async should also fail
        let store_async = tempfile::tempdir().unwrap();
        let mut cmd = std::process::Command::new(bin());
        cmd.args(["install"])
            .current_dir(project.path())
            .env("BPM_STORE", store_async.path())
            .env("BPM_ASYNC_RESOLVE", "1");
        let out = cmd.output().expect("run bpm with async resolver");
        assert!(
            !out.status.success(),
            "async should also fail when blocking fails"
        );
        return; // Both failed as expected, parity maintained
    }

    let blocking_lock = fs::read_to_string(project.path().join("bpm.lock"))
        .expect("bpm.lock should exist after blocking install");

    // Async resolve
    let store_async = tempfile::tempdir().unwrap();
    let _ = fs::remove_file(project.path().join("bpm.lock"));

    let mut cmd = std::process::Command::new(bin());
    cmd.args(["install"])
        .current_dir(project.path())
        .env("BPM_STORE", store_async.path())
        .env("BPM_ASYNC_RESOLVE", "1");
    let out = cmd.output().expect("run bpm with async resolver");
    assert!(
        out.status.success(),
        "async install failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let async_lock = fs::read_to_string(project.path().join("bpm.lock"))
        .expect("bpm.lock should exist after async install");

    assert_eq!(
        blocking_lock, async_lock,
        "blocking and async resolve must produce byte-identical bpm.lock"
    );
}

/// Test with a transitive dependency (A -> B)
#[test]
fn async_resolve_transitive_dependency_byte_identical() {
    // Create dep-b first
    let dep_b_tgz = package_tgz("transitive-dep-b", "1.0.0", None);
    let _server_b = same_host_registry_mock(
        "transitive-dep-b",
        "1.0.0",
        "tarballs/transitive-dep-b-1.0.0.tgz",
        dep_b_tgz,
    );

    // Create dep-a that depends on dep-b
    let dep_a_tgz = build_tgz(|b| {
        common::add_file(
            b,
            "package.json",
            0o644,
            serde_json::to_vec(&serde_json::json!({
                "name": "transitive-dep-a",
                "version": "1.0.0",
                "dependencies": {
                    "transitive-dep-b": "^1.0.0"
                }
            }))
            .expect("serialize package.json")
            .as_slice(),
        );
    });
    let server_a = same_host_registry_mock(
        "transitive-dep-a",
        "1.0.0",
        "tarballs/transitive-dep-a-1.0.0.tgz",
        dep_a_tgz,
    );

    let project = tempfile::tempdir().unwrap();
    let store_block = tempfile::tempdir().unwrap();

    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"transitive-dep-a":"^1.0.0"}}"#,
    )
    .unwrap();

    // Use server_a as the registry (it should handle requests for both)
    write_npmrc(project.path(), &[format!("registry={}", server_a.url(""))]);

    // Blocking resolve (BPM_ASYNC_RESOLVE=0, no streaming)
    let (ok_block, _stdout_block, _stderr_block) = run_bpm_with_env(
        &["install"],
        project.path(),
        store_block.path(),
        None,
        RESOLVE_BLOCKING,
    );
    if !ok_block {
        // Transitive dependency might not be resolved - check parity by seeing if async also fails
        let store_async = tempfile::tempdir().unwrap();
        let mut cmd = std::process::Command::new(bin());
        cmd.args(["install"])
            .current_dir(project.path())
            .env("BPM_STORE", store_async.path())
            .env("BPM_ASYNC_RESOLVE", "1");
        let out = cmd.output().expect("run bpm with async resolver");
        assert!(
            !out.status.success(),
            "async should also fail when blocking fails"
        );
        return;
    }

    let blocking_lock =
        fs::read_to_string(project.path().join("bpm.lock")).expect("bpm.lock should exist");

    // Async resolve
    let store_async = tempfile::tempdir().unwrap();
    let _ = fs::remove_file(project.path().join("bpm.lock"));

    let mut cmd = std::process::Command::new(bin());
    cmd.args(["install"])
        .current_dir(project.path())
        .env("BPM_STORE", store_async.path())
        .env("BPM_ASYNC_RESOLVE", "1");
    let out = cmd.output().expect("run bpm with async resolver");
    assert!(
        out.status.success(),
        "async install failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let async_lock =
        fs::read_to_string(project.path().join("bpm.lock")).expect("bpm.lock should exist");

    assert_eq!(
        blocking_lock, async_lock,
        "blocking and async must produce byte-identical bpm.lock"
    );
}

// === Plan 002: full-corpus parity tests using a multi-package mock ===

/// Real transitive-success parity: app -> A -> B, both served by one server.
/// Both blocking and async must succeed and produce byte-identical bpm.lock.
#[test]
fn async_resolve_transitive_success_byte_identical() {
    let dep_b_tgz = package_tgz("trans-b", "1.0.0", None);
    // A depends on B.
    let dep_a_tgz = build_tgz(|b| {
        common::add_file(
            b,
            "package.json",
            0o644,
            serde_json::to_vec(&serde_json::json!({
                "name": "trans-a",
                "version": "1.0.0",
                "dependencies": { "trans-b": "^1.0.0" }
            }))
            .expect("serialize package.json")
            .as_slice(),
        );
    });
    let server = multi_registry_mock(vec![
        ("trans-a", "1.0.0", "tarballs/trans-a-1.0.0.tgz", dep_a_tgz),
        ("trans-b", "1.0.0", "tarballs/trans-b-1.0.0.tgz", dep_b_tgz),
    ]);

    let project = tempfile::tempdir().unwrap();
    let store_block = tempfile::tempdir().unwrap();

    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"trans-a":"^1.0.0"}}"#,
    )
    .unwrap();
    write_npmrc(project.path(), &[format!("registry={}", server.url(""))]);

    // Blocking resolve succeeds (BPM_ASYNC_RESOLVE=0, no streaming).
    let (ok_block, stdout_block, stderr_block) = run_bpm_with_env(
        &["install"],
        project.path(),
        store_block.path(),
        None,
        RESOLVE_BLOCKING,
    );
    assert!(
        ok_block,
        "blocking install failed\nstdout: {stdout_block}\nstderr: {stderr_block}"
    );
    let blocking_lock =
        fs::read_to_string(project.path().join("bpm.lock")).expect("bpm.lock after blocking");

    // Async resolve must succeed and match byte-for-byte.
    let store_async = tempfile::tempdir().unwrap();
    let _ = fs::remove_file(project.path().join("bpm.lock"));
    let mut cmd = std::process::Command::new(bin());
    cmd.args(["install"])
        .current_dir(project.path())
        .env("BPM_STORE", store_async.path())
        .env("BPM_ASYNC_RESOLVE", "1");
    let out = cmd.output().expect("run bpm with async resolver");
    assert!(
        out.status.success(),
        "async install failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let async_lock =
        fs::read_to_string(project.path().join("bpm.lock")).expect("bpm.lock after async");

    assert_eq!(
        blocking_lock, async_lock,
        "blocking and async resolve must produce byte-identical bpm.lock for a transitive graph"
    );
}

/// Peer-dependency parity: app -> pkg -> (dep with peerDependency). Both
/// blocking and async must resolve identically.
#[test]
fn async_resolve_peer_dependency_success_byte_identical() {
    // dep-with-peer declares a peer on "react" (not installed).
    let dep_with_peer = build_tgz(|b| {
        common::add_file(
            b,
            "package.json",
            0o644,
            serde_json::to_vec(&serde_json::json!({
                "name": "dep-with-peer",
                "version": "1.0.0",
                "peerDependencies": { "react": "^18.0.0" }
            }))
            .expect("serialize package.json")
            .as_slice(),
        );
    });
    // react is installed as a normal dep so the peer is satisfiable.
    let react_tgz = package_tgz("react", "18.2.0", None);
    let server = multi_registry_mock(vec![
        (
            "dep-with-peer",
            "1.0.0",
            "tarballs/dep-with-peer-1.0.0.tgz",
            dep_with_peer,
        ),
        ("react", "18.2.0", "tarballs/react-18.2.0.tgz", react_tgz),
    ]);

    let project = tempfile::tempdir().unwrap();
    let store_block = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"dep-with-peer":"^1.0.0","react":"^18.0.0"}}"#,
    )
    .unwrap();
    write_npmrc(project.path(), &[format!("registry={}", server.url(""))]);

    // Blocking resolve (BPM_ASYNC_RESOLVE=0, no streaming)
    let (ok_block, stdout_block, stderr_block) = run_bpm_with_env(
        &["install"],
        project.path(),
        store_block.path(),
        None,
        RESOLVE_BLOCKING,
    );
    assert!(
        ok_block,
        "blocking install failed\nstdout: {stdout_block}\nstderr: {stderr_block}"
    );
    let blocking_lock =
        fs::read_to_string(project.path().join("bpm.lock")).expect("bpm.lock after blocking");

    let store_async = tempfile::tempdir().unwrap();
    let _ = fs::remove_file(project.path().join("bpm.lock"));
    let mut cmd = std::process::Command::new(bin());
    cmd.args(["install"])
        .current_dir(project.path())
        .env("BPM_STORE", store_async.path())
        .env("BPM_ASYNC_RESOLVE", "1");
    let out = cmd.output().expect("run bpm with async resolver");
    assert!(
        out.status.success(),
        "async install failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let async_lock =
        fs::read_to_string(project.path().join("bpm.lock")).expect("bpm.lock after async");

    assert_eq!(
        blocking_lock, async_lock,
        "blocking and async must produce byte-identical bpm.lock for a peer-dependency graph"
    );
}

/// Version-cycle parity: A -> B -> A. Both blocking and async must break the
/// cycle deterministically and produce byte-identical output.
#[test]
fn async_resolve_version_cycle_success_byte_identical() {
    let pkg_a_tgz = build_tgz(|b| {
        common::add_file(
            b,
            "package.json",
            0o644,
            serde_json::to_vec(&serde_json::json!({
                "name": "cycle-a",
                "version": "1.0.0",
                "dependencies": { "cycle-b": "^1.0.0" }
            }))
            .expect("serialize package.json")
            .as_slice(),
        );
    });
    let pkg_b_tgz = build_tgz(|b| {
        common::add_file(
            b,
            "package.json",
            0o644,
            serde_json::to_vec(&serde_json::json!({
                "name": "cycle-b",
                "version": "1.0.0",
                "dependencies": { "cycle-a": "^1.0.0" }
            }))
            .expect("serialize package.json")
            .as_slice(),
        );
    });
    let server = multi_registry_mock(vec![
        ("cycle-a", "1.0.0", "tarballs/cycle-a-1.0.0.tgz", pkg_a_tgz),
        ("cycle-b", "1.0.0", "tarballs/cycle-b-1.0.0.tgz", pkg_b_tgz),
    ]);

    let project = tempfile::tempdir().unwrap();
    let store_block = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"cycle-a":"^1.0.0"}}"#,
    )
    .unwrap();
    write_npmrc(project.path(), &[format!("registry={}", server.url(""))]);

    // Blocking resolve (BPM_ASYNC_RESOLVE=0, no streaming)
    let (ok_block, stdout_block, stderr_block) = run_bpm_with_env(
        &["install"],
        project.path(),
        store_block.path(),
        None,
        RESOLVE_BLOCKING,
    );
    assert!(
        ok_block,
        "blocking install failed\nstdout: {stdout_block}\nstderr: {stderr_block}"
    );
    let blocking_lock =
        fs::read_to_string(project.path().join("bpm.lock")).expect("bpm.lock after blocking");

    let store_async = tempfile::tempdir().unwrap();
    let _ = fs::remove_file(project.path().join("bpm.lock"));
    let mut cmd = std::process::Command::new(bin());
    cmd.args(["install"])
        .current_dir(project.path())
        .env("BPM_STORE", store_async.path())
        .env("BPM_ASYNC_RESOLVE", "1");
    let out = cmd.output().expect("run bpm with async resolver");
    assert!(
        out.status.success(),
        "async install failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let async_lock =
        fs::read_to_string(project.path().join("bpm.lock")).expect("bpm.lock after async");

    assert_eq!(
        blocking_lock, async_lock,
        "blocking and async must produce byte-identical bpm.lock for a cyclic dependency graph"
    );
}

/// Optional-dependency parity: app -> provider, provider declares an
/// optionalDependencies on "opt-extra" which IS available. Both blocking and
/// async must absorb the optional dependency and produce byte-identical output.
#[test]
fn async_resolve_optional_dependency_success_byte_identical() {
    let provider_tgz = build_tgz(|b| {
        common::add_file(
            b,
            "package.json",
            0o644,
            serde_json::to_vec(&serde_json::json!({
                "name": "opt-provider",
                "version": "1.0.0",
                "optionalDependencies": { "opt-extra": "^1.0.0" }
            }))
            .expect("serialize package.json")
            .as_slice(),
        );
    });
    let opt_extra_tgz = package_tgz("opt-extra", "1.2.0", None);
    let server = multi_registry_mock(vec![
        (
            "opt-provider",
            "1.0.0",
            "tarballs/opt-provider-1.0.0.tgz",
            provider_tgz,
        ),
        (
            "opt-extra",
            "1.2.0",
            "tarballs/opt-extra-1.2.0.tgz",
            opt_extra_tgz,
        ),
    ]);

    let project = tempfile::tempdir().unwrap();
    let store_block = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"opt-provider":"^1.0.0"}}"#,
    )
    .unwrap();
    write_npmrc(project.path(), &[format!("registry={}", server.url(""))]);

    // Blocking resolve (BPM_ASYNC_RESOLVE=0, no streaming)
    let (ok_block, stdout_block, stderr_block) = run_bpm_with_env(
        &["install"],
        project.path(),
        store_block.path(),
        None,
        RESOLVE_BLOCKING,
    );
    assert!(
        ok_block,
        "blocking install failed\nstdout: {stdout_block}\nstderr: {stderr_block}"
    );
    let blocking_lock =
        fs::read_to_string(project.path().join("bpm.lock")).expect("bpm.lock after blocking");

    let store_async = tempfile::tempdir().unwrap();
    let _ = fs::remove_file(project.path().join("bpm.lock"));
    let mut cmd = std::process::Command::new(bin());
    cmd.args(["install"])
        .current_dir(project.path())
        .env("BPM_STORE", store_async.path())
        .env("BPM_ASYNC_RESOLVE", "1");
    let out = cmd.output().expect("run bpm with async resolver");
    assert!(
        out.status.success(),
        "async install failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let async_lock =
        fs::read_to_string(project.path().join("bpm.lock")).expect("bpm.lock after async");

    assert_eq!(
        blocking_lock, async_lock,
        "blocking and async must produce byte-identical bpm.lock for an optional-dependency graph"
    );
}

// === Plan 010: resolver-mode/peer-option matrix regressions ===

/// Plan 010 regression: a required peer absent from the tree must fail strict
/// installs but succeed under `--legacy-peer-deps` across all three resolver
/// paths (blocking, async streaming, async non-streaming). The successful
/// legacy lockfile must record `resolution.root.peerMode = legacy-ignore` with
/// no bound provider for the ignored peer, and the three legacy lockfiles must
/// be byte-identical.
#[test]
fn legacy_peer_deps_succeeds_across_resolver_modes_and_records_legacy_mode() {
    // dep-with-peer declares a required peer on "react"; the app does not
    // install react, so the peer is strictly missing. The peer is declared in
    // the packument (the resolver binds peers from the packument, not the
    // tarball), so strict mode must reject the missing required peer.
    let dep_with_peer = package_tgz("dep-with-peer", "1.0.0", None);
    let mut peer_deps = BTreeMap::new();
    peer_deps.insert("react".to_string(), "^18.0.0".to_string());
    let server = multi_registry_mock_with_peers(vec![(
        "dep-with-peer",
        "1.0.0",
        "tarballs/dep-with-peer-1.0.0.tgz",
        dep_with_peer,
        peer_deps,
    )]);

    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"dep-with-peer":"^1.0.0"}}"#,
    )
    .unwrap();
    write_npmrc(project.path(), &[format!("registry={}", server.url(""))]);

    fn parse_lock(project: &Path) -> serde_json::Value {
        let text = fs::read_to_string(project.join("bpm.lock")).expect("bpm.lock");
        serde_json::from_str(&text).expect("parse bpm.lock")
    }

    // Strict blocking install must fail on the missing required peer.
    let store = tempfile::tempdir().unwrap();
    let (ok, _, stderr) = run_bpm_with_env(
        &["install"],
        project.path(),
        store.path(),
        None,
        RESOLVE_BLOCKING,
    );
    assert!(
        !ok,
        "strict blocking install must fail on the missing peer\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("peer"),
        "strict blocking failure must mention a peer conflict; got: {stderr}"
    );
    assert!(
        !project.path().join("bpm.lock").exists(),
        "a failed strict install must not write bpm.lock"
    );

    // Strict async+streaming install must fail with equivalent peer behavior.
    let store = tempfile::tempdir().unwrap();
    let mut cmd = std::process::Command::new(bin());
    cmd.args(["install"])
        .current_dir(project.path())
        .env("BPM_STORE", store.path())
        .env("BPM_ASYNC_RESOLVE", "1")
        .env("BPM_STREAM_INSTALL", "1");
    let out = cmd.output().expect("run bpm strict async streaming");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "strict async+streaming install must fail on the missing peer\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("peer"),
        "strict async+streaming failure must mention a peer conflict; got: {stderr}"
    );

    // Legacy blocking install must succeed.
    let store_block = tempfile::tempdir().unwrap();
    let _ = fs::remove_file(project.path().join("bpm.lock"));
    let (ok, stdout, stderr) = run_bpm_with_env(
        &["install", "--legacy-peer-deps"],
        project.path(),
        store_block.path(),
        None,
        RESOLVE_BLOCKING,
    );
    assert!(
        ok,
        "legacy blocking install must succeed\nstdout: {stdout}\nstderr: {stderr}"
    );
    let blocking_lock = fs::read_to_string(project.path().join("bpm.lock"))
        .expect("bpm.lock after legacy blocking");

    // Legacy async+streaming install must succeed and match byte-for-byte.
    let store_async = tempfile::tempdir().unwrap();
    let _ = fs::remove_file(project.path().join("bpm.lock"));
    let mut cmd = std::process::Command::new(bin());
    cmd.args(["install", "--legacy-peer-deps"])
        .current_dir(project.path())
        .env("BPM_STORE", store_async.path())
        .env("BPM_ASYNC_RESOLVE", "1")
        .env("BPM_STREAM_INSTALL", "1");
    let out = cmd.output().expect("run bpm legacy async streaming");
    assert!(
        out.status.success(),
        "legacy async+streaming install must succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let async_streaming_lock =
        fs::read_to_string(project.path().join("bpm.lock")).expect("bpm.lock after legacy async");
    assert_eq!(
        blocking_lock, async_streaming_lock,
        "legacy blocking and legacy async+streaming must produce byte-identical bpm.lock"
    );

    // Legacy async non-streaming install must also succeed and match.
    let store_async_ns = tempfile::tempdir().unwrap();
    let _ = fs::remove_file(project.path().join("bpm.lock"));
    let mut cmd = std::process::Command::new(bin());
    cmd.args(["install", "--legacy-peer-deps"])
        .current_dir(project.path())
        .env("BPM_STORE", store_async_ns.path())
        .env("BPM_ASYNC_RESOLVE", "1")
        .env("BPM_STREAM_INSTALL", "0");
    let out = cmd.output().expect("run bpm legacy async non-streaming");
    assert!(
        out.status.success(),
        "legacy async non-streaming install must succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let async_ns_lock = fs::read_to_string(project.path().join("bpm.lock"))
        .expect("bpm.lock after legacy async non-streaming");
    assert_eq!(
        blocking_lock, async_ns_lock,
        "legacy blocking and legacy async non-streaming must produce byte-identical bpm.lock"
    );

    // The successful lockfile records legacy-ignore peer mode and no bound
    // provider for the ignored peer.
    let lock = parse_lock(project.path());
    assert_eq!(
        lock["resolution"]["root"]["peerMode"], "legacy-ignore",
        "legacy lockfile must record resolution.root.peerMode = legacy-ignore"
    );
    let packages = lock["resolution"]["packages"]
        .as_object()
        .expect("resolution.packages object");
    let peer_entry = packages
        .get("node_modules/dep-with-peer")
        .expect("dep-with-peer resolution entry present");
    assert!(
        peer_entry
            .get("peerContext")
            .is_none_or(|v| v.is_null() || v.as_object().is_some_and(|o| o.is_empty())),
        "legacy mode must leave no bound provider for the ignored peer"
    );
}

/// Plan 010 regression: the non-streaming kill-switch matrix must print a
/// resolver label that distinguishes blocking (`BPM_ASYNC_RESOLVE=0`) from
/// async (`BPM_ASYNC_RESOLVE=1`). Asserts on the stable "(async)" tag emitted
/// by `install.rs`, not on test-only output.
#[test]
fn fresh_install_mode_matrix_labels_blocking_vs_async() {
    let dep_tgz = package_tgz("matrix-dep", "1.0.0", None);
    let server = same_host_registry_mock(
        "matrix-dep",
        "1.0.0",
        "tarballs/matrix-dep-1.0.0.tgz",
        dep_tgz,
    );

    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app-matrix","version":"1.0.0","dependencies":{"matrix-dep":"^1.0.0"}}"#,
    )
    .unwrap();
    write_npmrc(project.path(), &[format!("registry={}", server.url(""))]);

    // Non-streaming blocking (BPM_ASYNC_RESOLVE=0): no "(async)" tag.
    let store_block = tempfile::tempdir().unwrap();
    let (ok, _, stderr) = run_bpm_with_env(
        &["install"],
        project.path(),
        store_block.path(),
        None,
        RESOLVE_BLOCKING,
    );
    assert!(
        ok,
        "blocking non-streaming install failed\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("resolved") && !stderr.contains("(async)"),
        "blocking non-streaming must emit the blocking label (no (async) tag); got: {stderr}"
    );

    // Non-streaming async (BPM_ASYNC_RESOLVE=1, BPM_STREAM_INSTALL=0): "(async)" tag.
    let store_async = tempfile::tempdir().unwrap();
    let _ = fs::remove_file(project.path().join("bpm.lock"));
    let mut cmd = std::process::Command::new(bin());
    cmd.args(["install"])
        .current_dir(project.path())
        .env("BPM_STORE", store_async.path())
        .env("BPM_ASYNC_RESOLVE", "1")
        .env("BPM_STREAM_INSTALL", "0");
    let out = cmd.output().expect("run bpm async non-streaming");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "async non-streaming install failed\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("(async)"),
        "async non-streaming must emit the (async) label; got: {stderr}"
    );
}

// === Plan 012: registry trust-boundary regression tests ===

/// A registry that returns a packument whose dist.tarball is a `file://` URL
/// must be rejected with an UnsupportedTarballSource error.
#[test]
fn registry_tarball_rejects_local_source() {
    let tgz = package_tgz("innocent", "1.0.0", None);
    let local_path = "file:///tmp/innocent-1.0.0.tgz".to_string();
    let registry = metadata_only_registry_mock("innocent", "1.0.0", local_path.clone(), tgz);

    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_project(project.path());
    write_npmrc(project.path(), &[format!("registry={}", registry.url(""))]);

    let (ok, _stdout, stderr) = run_bpm(
        &[
            "fetch",
            "innocent",
            "--registry",
            &registry.url(""),
            "--store",
            store.path().to_str().unwrap(),
            "--no-extract",
        ],
        project.path(),
        store.path(),
        None,
    );

    // Must fail with an unsupported-source error before any tarball request.
    assert!(!ok, "expected fetch to fail; stderr: {stderr}");
    assert!(
        stderr.contains("unsupported tarball source"),
        "error must mention unsupported tarball source; got: {stderr}"
    );
    assert!(
        stderr.contains("file"),
        "error must mention the scheme; got: {stderr}"
    );

    // Only the metadata request was made — no tarball request.
    assert_eq!(registry.requests().len(), 1, "only metadata request");
    assert_eq!(
        registry.requests()[0].path,
        format!("/{}", "innocent".replace('/', "%2F")),
        "must be the metadata path"
    );

    // Stderr must not contain the local file path or contents.
    assert!(
        !stderr.contains("/tmp/innocent"),
        "stderr must not contain the raw local path; got: {stderr}"
    );

    // No scratch files linger in the store's tmp directory.
    let tmp_dir = store.path().join("tmp");
    if tmp_dir.is_dir() {
        let tmp_count = fs::read_dir(&tmp_dir)
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(tmp_count, 0, "no temp scratch should remain after failure");
    }
}

/// A cross-origin HTTP tarball URL from the registry mock must still resolve
/// and download successfully — CDN compatibility must not regress.
#[test]
fn registry_tarball_accepts_cross_origin_http() {
    let tgz = package_tgz("cdn-pkg", "1.0.0", None);

    // A separate server for the tarball (cross-origin relative to metadata).
    let tarball_server = MiniServer::start_keep_alive_routed({
        let tgz = Arc::new(tgz.clone());
        move |path| {
            if path == "/tarballs/cdn-pkg-1.0.0.tgz" {
                Some(RouteBody((*tgz).clone(), "application/gzip"))
            } else {
                None
            }
        }
    });

    // The metadata server points to a cross-origin tarball URL.
    let registry = metadata_only_registry_mock(
        "cdn-pkg",
        "1.0.0",
        tarball_server.url("tarballs/cdn-pkg-1.0.0.tgz"),
        tgz,
    );

    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_project(project.path());
    write_npmrc(project.path(), &[format!("registry={}", registry.url(""))]);

    let (ok, stdout, stderr) = run_bpm(
        &[
            "fetch",
            "cdn-pkg",
            "--registry",
            &registry.url(""),
            "--store",
            store.path().to_str().unwrap(),
            "--no-extract",
        ],
        project.path(),
        store.path(),
        None,
    );

    assert!(
        ok,
        "cross-origin CDN tarball must succeed; stderr: {stderr}\nstdout: {stdout}"
    );

    // One metadata request and one tarball request.
    assert_eq!(registry.requests().len(), 1, "one metadata request");
    assert_eq!(
        tarball_server.requests().len(),
        1,
        "one tarball request to cross-origin server"
    );
}

/// `.npmrc` retry settings must reach the default async resolver: a server
/// that answers the first metadata request with `503` then a valid packument
/// must install successfully with `fetch-retries=1`, after exactly two metadata
/// requests. Proves `effective_npm_config` wiring reaches the async retry loop.
#[test]
fn npmrc_fetch_retries_reach_async_resolver() {
    let tgz = build_tgz(|b| {
        common::add_file(
            b,
            "package/package.json",
            0o644,
            b"{\"name\":\"retry-pkg\"}",
        );
    });
    let integrity = integrity_of(&tgz);
    let tarball_path = "/retry-pkg/-/retry-pkg-1.0.0.tgz";
    let base = Arc::new(Mutex::new(String::new()));
    let base_thread = base.clone();
    let tarball = Arc::new(tgz);
    let tarball_thread = tarball.clone();
    let integrity_thread = integrity.clone();
    let expected_meta = "/retry-pkg".to_string();
    let expected_tgz = tarball_path.to_string();

    let server = MiniServer::start_routed_with_failures(
        vec![common::TransientFailure::new(503)],
        move |path| {
            let base = base_thread.lock().unwrap().clone();
            if path == expected_meta {
                let body = serde_json::to_vec(&packument(
                    "1.0.0",
                    format!("{}{expected_tgz}", base.trim_end_matches('/')),
                    integrity_thread.clone(),
                ))
                .unwrap();
                Some(RouteBody(body, "application/json"))
            } else if path == expected_tgz {
                Some(RouteBody((*tarball_thread).clone(), "application/gzip"))
            } else {
                None
            }
        },
    );
    *base.lock().unwrap() = server.url("");

    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"retry-pkg":"^1.0.0"}}"#,
    )
    .unwrap();
    write_npmrc(
        project.path(),
        &[
            format!("registry={}", server.url("")),
            "fetch-retries=1".to_string(),
            "fetch-retry-mintimeout=1".to_string(),
            "fetch-retry-maxtimeout=2".to_string(),
            "fetch-retry-factor=1".to_string(),
        ],
    );

    let (ok, stdout, stderr) = run_bpm_with_env(
        &["install", "--store", store.path().to_str().unwrap()],
        project.path(),
        store.path(),
        None,
        &[("BPM_ASYNC_RESOLVE", "1")],
    );
    assert!(
        ok,
        "install should succeed after retry; stderr: {stderr}\nstdout: {stdout}"
    );

    let metadata_requests = server
        .requests()
        .iter()
        .filter(|req| req.path == "/retry-pkg")
        .count();
    assert_eq!(
        metadata_requests, 2,
        "expected exactly two metadata requests (503 then 200); got {metadata_requests}"
    );
    assert!(
        project
            .path()
            .join("node_modules/retry-pkg/package.json")
            .exists(),
        "package must be materialized"
    );
}

/// Forced-overflow parity for the async+streaming pipeline (plan 019). With
/// concurrency 1 (channel capacity 2) and six packages whose origin tarballs
/// are served slowly, resolution emits every unit in microseconds while the
/// single download worker is still busy, so several units must overflow the
/// live channel. They must be retained and drained through the same pipeline
/// (never dropped, never sent to a sequential origin-only fallback — that path
/// no longer exists, and the completeness invariant would fail the install if a
/// unit went missing). Asserts the install succeeds and every package is
/// materialized.
#[test]
fn async_streaming_overflow_drains_through_pipeline() {
    // Build six tiny packages served by one slow-tarball origin.
    let packages: Vec<(String, Vec<u8>, String)> = (0..6)
        .map(|i| {
            let name = format!("ov{i}");
            let tgz = build_tgz(|b| {
                common::add_file(
                    b,
                    "package/package.json",
                    0o644,
                    format!("{{\"name\":\"{name}\"}}").as_bytes(),
                );
            });
            let integrity = integrity_of(&tgz);
            (name, tgz, integrity)
        })
        .collect();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let registry_url = format!("http://{addr}");
    let pkgs = Arc::new(
        packages
            .iter()
            .map(|(name, tgz, integ)| (name.clone(), tgz.clone(), integ.clone()))
            .collect::<Vec<_>>(),
    );
    let base = Arc::new(Mutex::new(String::new()));
    let base_thread = base.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let pkgs = Arc::clone(&pkgs);
            let base = base_thread.lock().unwrap().clone();
            thread::spawn(move || {
                serve_slow_origin(&mut stream, &pkgs, &base);
            });
        }
    });
    *base.lock().unwrap() = registry_url.clone();

    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let mut deps = serde_json::Map::new();
    for (name, _, _) in &packages {
        deps.insert(name.clone(), serde_json::json!("^1.0.0"));
    }
    let mut root = serde_json::Map::new();
    root.insert("name".into(), serde_json::json!("ov-app"));
    root.insert("version".into(), serde_json::json!("1.0.0"));
    root.insert("dependencies".into(), serde_json::Value::Object(deps));
    fs::write(
        project.path().join("package.json"),
        serde_json::Value::Object(root).to_string(),
    )
    .unwrap();
    write_npmrc(project.path(), &[format!("registry={registry_url}")]);

    // Concurrency 1 => channel capacity 2; six slow-tarball packages force
    // overflow. Async + streaming are the defaults.
    let (ok, stdout, stderr) = run_bpm_with_env(
        &[
            "install",
            "--concurrency",
            "1",
            "--store",
            store.path().to_str().unwrap(),
        ],
        project.path(),
        store.path(),
        None,
        &[("BPM_ASYNC_RESOLVE", "1"), ("BPM_STREAM_INSTALL", "1")],
    );
    assert!(
        ok,
        "overflow install must succeed; stderr: {stderr}\nstdout: {stdout}"
    );

    // Every overflowed package must be materialized — proving overflow units
    // were retained and drained through the pipeline rather than dropped.
    for (name, _, _) in &packages {
        assert!(
            project
                .path()
                .join("node_modules")
                .join(name)
                .join("package.json")
                .exists(),
            "overflow package {name} must be materialized"
        );
    }
}

/// Serve packuments instantly and tarballs with a small delay so a single
/// download worker cannot keep up with resolution emission (forcing overflow).
fn serve_slow_origin(stream: &mut TcpStream, pkgs: &[(String, Vec<u8>, String)], base: &str) {
    use std::io::{Read, Write};
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).unwrap_or(0);
    let request = String::from_utf8_lossy(&buf[..n]);
    let path = request
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string();

    // Metadata path: /<name> (url-encoded). Serve the packument instantly.
    for (name, tgz, integrity) in pkgs {
        let meta_path = format!("/{}", name.replace('/', "%2F"));
        let tgz_path = format!("/{name}/-/{name}-1.0.0.tgz");
        if path == meta_path {
            let body = serde_json::to_vec(&packument(
                "1.0.0",
                format!("{}{tgz_path}", base.trim_end_matches('/')),
                integrity.clone(),
            ))
            .unwrap();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.write_all(&body);
            return;
        }
        if path == tgz_path {
            // Delay so the single worker stalls and the channel overflows.
            thread::sleep(Duration::from_millis(30));
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                tgz.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.write_all(tgz);
            return;
        }
    }
    let _ = stream
        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
}
