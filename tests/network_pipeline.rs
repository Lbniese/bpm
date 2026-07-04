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
/// `bpm.lock` bytes as the default blocking resolver. Uses a local registry
/// server for deterministic, offline resolution.
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

    // ---- Blocking resolve (default) ----
    let (ok_block, stdout_block, stderr_block) = run_bpm(
        &["install"],
        project.path(),
        store_block.path(),
        None,
    );
    assert!(
        ok_block,
        "blocking install failed\nstdout: {stdout_block}\nstderr: {stderr_block}"
    );
    let blocking_lock =
        fs::read_to_string(project.path().join("bpm.lock"))
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
    let async_lock =
        fs::read_to_string(project.path().join("bpm.lock"))
            .expect("bpm.lock should exist after async install");

    assert_eq!(
        blocking_lock, async_lock,
        "blocking and async resolve must produce byte-identical bpm.lock"
    );
}
