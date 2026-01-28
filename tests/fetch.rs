//! End-to-end CLI tests for `bpm fetch`: two-process concurrency, cache reuse
//! ("repeated fetch performs no network or extraction work"), trace output,
//! and JSON metrics.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;

use common::{build_tgz, integrity_of, MiniServer};

fn bin() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_BIN_EXE_bpm").expect("CARGO_BIN_EXE_bpm"))
}

fn fixture_tgz() -> Vec<u8> {
    build_tgz(|b| {
        common::add_dir(b, "package", 0o755);
        common::add_file(
            b,
            "package/package.json",
            0o644,
            br#"{"name":"app","version":"1.0.0"}"#,
        );
        common::add_file(b, "package/index.js", 0o644, b"module.exports = 42;");
    })
}

fn count_artifacts(store: &Path) -> usize {
    let base = store.join("artifacts/sha512");
    let mut n = 0;
    if let Ok(groups) = fs::read_dir(&base) {
        for g in groups.flatten() {
            if let Ok(files) = fs::read_dir(g.path()) {
                for f in files.flatten() {
                    if f.path().extension().and_then(|e| e.to_str()) == Some("tgz") {
                        n += 1;
                    }
                }
            }
        }
    }
    n
}

#[test]
fn fetch_extracts_image_with_default_options() {
    let tgz = fixture_tgz();
    let integrity = integrity_of(&tgz);
    let image_id = bpm::integrity::Sha512Digest::hash_bytes(&tgz);
    let server = MiniServer::start(tgz);
    let url = server.url_for();
    let store = tempfile::tempdir().unwrap();

    let out = Command::new(bin())
        .args([
            "fetch",
            &url,
            "--integrity",
            &integrity,
            "--store",
            store.path().to_str().unwrap(),
        ])
        .output()
        .expect("run bpm fetch");
    assert!(
        out.status.success(),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("artifact"), "{stdout}");
    assert!(stdout.contains("image"), "{stdout}");

    // Image contents are the package root (package.json at top of image).
    let id = image_id;
    let image_dir = store
        .path()
        .join("images/sha512")
        .join(&id.to_hex()[..2])
        .join(id.to_hex());
    assert!(
        image_dir.join("package.json").is_file(),
        "image not extracted"
    );
}

#[test]
fn repeated_fetch_performs_no_work() {
    let tgz = fixture_tgz();
    let integrity = integrity_of(&tgz);
    let server = MiniServer::start(tgz);
    let url = server.url_for();
    let store = tempfile::tempdir().unwrap();

    let first = Command::new(bin())
        .args([
            "fetch",
            &url,
            "--integrity",
            &integrity,
            "--no-extract",
            "--store",
            store.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(server.hits(), 1);
    let cached_text = String::from_utf8_lossy(&first.stdout);
    assert!(cached_text.contains("stored"), "{cached_text}");

    let second = Command::new(bin())
        .args([
            "fetch",
            &url,
            "--integrity",
            &integrity,
            "--no-extract",
            "--store",
            store.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(server.hits(), 1, "cache hit must not re-download");
    let out = String::from_utf8_lossy(&second.stdout);
    assert!(out.contains("cached"), "expected cached artifact: {out}");
}

#[test]
fn concurrent_processes_publish_once() {
    let tgz = fixture_tgz();
    let integrity = integrity_of(&tgz);
    let server = Arc::new(MiniServer::start(tgz));
    let url = server.url_for();
    let store = Arc::new(tempfile::tempdir().unwrap());
    let store_path = store.path().to_path_buf();

    let n = 4;
    let mut handles = Vec::new();
    for _ in 0..n {
        let bin = bin();
        let url = url.clone();
        let integ = integrity.clone();
        let store_path = store_path.clone();
        handles.push(thread::spawn(move || {
            Command::new(bin)
                .args([
                    "fetch",
                    &url,
                    "--integrity",
                    &integ,
                    "--no-extract",
                    "--store",
                    store_path.to_str().unwrap(),
                ])
                .output()
                .expect("run bpm fetch")
                .status
                .success()
        }));
    }
    let oks: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(oks.iter().all(|&ok| ok), "not all processes succeeded");

    assert_eq!(
        count_artifacts(&store_path),
        1,
        "exactly one artifact expected"
    );
    let hits = server.hits();
    assert!(hits >= 1 && hits <= n, "unexpected downloads: {hits}");
    assert_eq!(
        hits, 1,
        "per-artifact lock should serialize to one download"
    );
}

#[test]
fn fetch_without_integrity_computes_id_and_is_reusable() {
    let tgz = fixture_tgz();
    let server = MiniServer::start(tgz.clone());
    let url = server.url_for();
    let store = tempfile::tempdir().unwrap();

    let first = Command::new(bin())
        .args([
            "fetch",
            &url,
            "--no-extract",
            "--store",
            store.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(server.hits(), 1);
    assert_eq!(count_artifacts(store.path()), 1);

    // Re-derive the id and re-fetch WITH integrity: must hit the cache (no new download).
    let integ = integrity_of(&tgz);
    let second = Command::new(bin())
        .args([
            "fetch",
            &url,
            "--integrity",
            &integ,
            "--no-extract",
            "--store",
            store.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(server.hits(), 1, "named artifact should be reused");
    let out = String::from_utf8_lossy(&second.stdout);
    assert!(out.contains("cached"), "{out}");
}

#[test]
fn bpm_trace_emits_phase_trace() {
    let tgz = fixture_tgz();
    let integrity = integrity_of(&tgz);
    let server = MiniServer::start(tgz);
    let url = server.url_for();
    let store = tempfile::tempdir().unwrap();

    let out = Command::new(bin())
        .env("BPM_TRACE", "1")
        .args([
            "fetch",
            &url,
            "--integrity",
            &integrity,
            "--no-extract",
            "--store",
            store.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("artifact_download"),
        "trace missing phase: {stderr}"
    );
}

#[test]
fn json_metrics_is_written_and_valid() {
    let tgz = fixture_tgz();
    let integrity = integrity_of(&tgz);
    let server = MiniServer::start(tgz);
    let url = server.url_for();
    let store = tempfile::tempdir().unwrap();
    let metrics_path = store.path().join("metrics.json");

    let out = Command::new(bin())
        .args([
            "fetch",
            &url,
            "--integrity",
            &integrity,
            "--json-metrics",
            metrics_path.to_str().unwrap(),
            "--store",
            store.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let data = fs::read_to_string(&metrics_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&data).expect("metrics JSON parses");
    assert!(
        v["phases"]["artifact_download"].is_number(),
        "missing phase: {data}"
    );
    assert!(
        v["phases"]["artifact_extract"].is_number(),
        "missing extract phase: {data}"
    );
    assert!(v["total_ms"].is_number());
}
