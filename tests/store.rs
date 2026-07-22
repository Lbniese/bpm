//! Store behavior tests (AGENTS "Store changes"): integrity mismatch,
//! interrupted writes, concurrent writers (in-process threads), corrupt
//! existing objects, read-only publication, atomic reuse, and image reuse.

mod common;

use std::fs;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use bpm::integrity::Integrity;
use bpm::metrics::Metrics;
use bpm::store::ArtifactStore;

use common::{build_tgz, integrity_of, MiniServer};

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

#[test]
fn atomic_reuse_skips_download_and_extraction() {
    let tgz = fixture_tgz();
    let integrity = integrity_of(&tgz);
    let integ = Integrity::parse(&integrity).unwrap();
    let server = MiniServer::start(tgz.clone());
    let url = server.url_for();

    let store_dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::open(store_dir.path()).unwrap();

    let mut m = Metrics::new();
    let a = store.ensure_artifact(&url, Some(&integ), &mut m).unwrap();
    assert!(!a.cached);
    assert_eq!(server.hits(), 1);

    let mut m2 = Metrics::new();
    let b = store.ensure_artifact(&url, Some(&integ), &mut m2).unwrap();
    assert!(b.cached);
    assert_eq!(server.hits(), 1, "cache hit must not re-download");

    // Same artifact path reused.
    assert_eq!(a.path, b.path);

    // Extract once, then reuse.
    let img1 = store.ensure_image(&a.id, &mut m2).unwrap();
    assert!(!img1.cached);
    assert!(img1.path.join("package.json").is_file());
    let img2 = store.ensure_image(&a.id, &mut m).unwrap();
    assert!(img2.cached);
}

#[test]
fn image_without_current_layout_marker_is_rebuilt() {
    let tgz = fixture_tgz();
    let integrity = integrity_of(&tgz);
    let integ = Integrity::parse(&integrity).unwrap();
    let store_dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::open(store_dir.path()).unwrap();
    let archive = tempfile::NamedTempFile::new().unwrap();
    fs::write(archive.path(), &tgz).unwrap();

    let mut metrics = Metrics::new();
    let artifact = store
        .ensure_artifact(archive.path().to_str().unwrap(), Some(&integ), &mut metrics)
        .unwrap();
    let image = store.ensure_image(&artifact.id, &mut metrics).unwrap();
    fs::remove_file(store.image_path(&artifact.id).with_extension("version")).unwrap();
    fs::write(image.path.join("package.json"), b"stale").unwrap();

    let rebuilt = store.ensure_image(&artifact.id, &mut metrics).unwrap();
    assert!(!rebuilt.cached);
    assert_eq!(
        fs::read(rebuilt.path.join("package.json")).unwrap(),
        br#"{"name":"app","version":"1.0.0"}"#
    );
}

#[test]
fn integrity_mismatch_is_rejected_and_tmp_cleaned() {
    let tgz = fixture_tgz();
    let server = MiniServer::start(tgz.clone());
    let url = server.url_for();

    let store_dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::open(store_dir.path()).unwrap();

    let lying = Integrity::sha512(bpm::integrity::Sha512Digest::hash_bytes(b"not the tarball"));
    let mut m = Metrics::new();
    let err = store
        .ensure_artifact(&url, Some(&lying), &mut m)
        .expect_err("mismatch must fail");
    assert!(
        format!("{err}").contains("integrity verification failed"),
        "got: {err}"
    );
    assert!(format!("{err}").contains(&lying.to_npm_string()));

    // No published artifact, and no leftover tmp scratch.
    assert!(!store.artifact_path(lying.digest()).exists());
    let tmp_count = fs::read_dir(store.root().join("tmp")).unwrap().count();
    assert_eq!(tmp_count, 0, "temp scratch not cleaned after mismatch");
    assert_eq!(server.hits(), 1);
}

/// Plan 012: a retrieval error (not just an integrity mismatch) must not leave a
/// scratch file in the store's temp directory.
#[test]
fn retrieval_error_leaves_no_tmp_scratch() {
    let store_dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::open(store_dir.path()).unwrap();

    // A file:// URL that points at a non-existent file: the download layer
    // fails before any bytes are written, surfacing StoreError::Download.
    let missing = store_dir.path().join("does-not-exist.tgz");
    let url = format!("file://{}", missing.display());
    let mut m = Metrics::new();
    let err = store
        .ensure_artifact(&url, None, &mut m)
        .expect_err("retrieval of a missing source must fail");
    assert!(
        format!("{err}").contains("download"),
        "expected a download error; got: {err}"
    );

    let tmp_count = fs::read_dir(store.root().join("tmp")).unwrap().count();
    assert_eq!(
        tmp_count, 0,
        "temp scratch not cleaned after retrieval error"
    );
}

#[test]
fn interrupted_writes_do_not_block_refetch() {
    let tgz = fixture_tgz();
    let integrity = integrity_of(&tgz);
    let integ = Integrity::parse(&integrity).unwrap();
    let server = MiniServer::start(tgz.clone());
    let url = server.url_for();

    let store_dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::open(store_dir.path()).unwrap();

    // Simulate a crashed previous writer: a stray partial temp file, no final.
    let stray = store.root().join("tmp").join("dl-crashed.999.999.dead.tmp");
    fs::write(&stray, b"partial garbage").unwrap();

    let mut m = Metrics::new();
    let a = store.ensure_artifact(&url, Some(&integ), &mut m).unwrap();
    assert!(!a.cached);

    // The published artifact is intact and verifiable.
    assert!(a.path.exists());
    store
        .verify_artifact(&a.id)
        .expect("published artifact verifies");
}

#[test]
fn corrupt_existing_artifact_is_detected() {
    let tgz = fixture_tgz();
    let integrity = integrity_of(&tgz);
    let integ = Integrity::parse(&integrity).unwrap();
    let server = MiniServer::start(tgz);
    let url = server.url_for();

    let store_dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::open(store_dir.path()).unwrap();

    let mut m = Metrics::new();
    let a = store.ensure_artifact(&url, Some(&integ), &mut m).unwrap();

    // Tamper with the stored artifact (mimicking disk corruption).
    fs::write(&a.path, b"corrupted contents").unwrap();

    let err = store
        .verify_artifact(&a.id)
        .expect_err("corruption must be detected");
    assert!(format!("{err}").contains("corruption"), "got: {err}");
}

#[test]
fn concurrent_writers_publish_once() {
    let tgz = fixture_tgz();
    let integrity = integrity_of(&tgz);
    let server = MiniServer::start(tgz.clone());
    let url = server.url_for();

    let store_dir = Arc::new(tempfile::tempdir().unwrap());
    let store = Arc::new(ArtifactStore::open(store_dir.path()).unwrap());
    let integ = Integrity::parse(&integrity).unwrap();
    let id = *integ.digest();

    let n = 4;
    let handles: Vec<_> = (0..n)
        .map(|_| {
            let store = store.clone();
            let url = url.clone();
            let integ = integ.clone();
            thread::spawn(move || {
                let mut m = Metrics::new();
                store.ensure_artifact(&url, Some(&integ), &mut m).unwrap()
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    for r in &results {
        assert!(
            r.path.exists(),
            "every writer must see a published artifact"
        );
    }
    assert_eq!(results.len(), n);

    // Exactly one published archive.
    let artifacts: Vec<_> = glob::glob_artifacts(&store);
    assert_eq!(
        artifacts.len(),
        1,
        "concurrent writers must publish exactly once"
    );
    // The lock guarantees a single download; allow up to n defensively.
    let hits = server.hits();
    assert!(hits >= 1 && hits <= n, "unexpected downloads: {hits}");
    assert_eq!(
        hits, 1,
        "per-artifact lock should serialize to one download"
    );

    let _ = id;
    let _ = store_dir; // keep tempdir alive for assertions
}

const STRESS_STORE_ENV: &str = "BPM_STORE_STRESS_ROOT";
const STRESS_URL_ENV: &str = "BPM_STORE_STRESS_URL";
const STRESS_INTEGRITY_ENV: &str = "BPM_STORE_STRESS_INTEGRITY";

/// Child-process entry point for [`high_process_count_same_artifact_publication`].
/// A normal test-suite run leaves the environment unset, making this a no-op.
#[test]
fn same_artifact_stress_worker() {
    let Some(root) = std::env::var_os(STRESS_STORE_ENV) else {
        return;
    };
    let url = std::env::var(STRESS_URL_ENV).expect("stress URL");
    let integrity = std::env::var(STRESS_INTEGRITY_ENV).expect("stress integrity");
    let integrity = Integrity::parse(&integrity).expect("valid stress integrity");
    let store = ArtifactStore::open(std::path::Path::new(&root)).expect("open shared store");
    let mut metrics = Metrics::new();

    let artifact = store
        .ensure_artifact(&url, Some(&integrity), &mut metrics)
        .expect("publish or reuse shared artifact");
    assert!(artifact.path.is_file());
    store
        .verify_artifact(&artifact.id)
        .expect("shared artifact must verify");
}

#[test]
fn high_process_count_same_artifact_publication() {
    let tgz = fixture_tgz();
    let integrity = integrity_of(&tgz);
    let parsed = Integrity::parse(&integrity).unwrap();
    let server = MiniServer::start(tgz);
    let store_dir = tempfile::tempdir().unwrap();

    // Keep enough contention to exercise the OS-level advisory lock while
    // bounding process pressure on small CI runners.
    let process_count = thread::available_parallelism()
        .map(|parallelism| parallelism.get().saturating_mul(2))
        .unwrap_or(16)
        .clamp(12, 32);
    let test_binary = std::env::current_exe().expect("current integration-test executable");
    let mut children = Vec::with_capacity(process_count);
    for _ in 0..process_count {
        children.push(
            Command::new(&test_binary)
                .arg("--exact")
                .arg("same_artifact_stress_worker")
                .arg("--test-threads=1")
                .env(STRESS_STORE_ENV, store_dir.path())
                .env(STRESS_URL_ENV, server.url_for())
                .env(STRESS_INTEGRITY_ENV, &integrity)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn stress worker"),
        );
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut statuses = vec![None; process_count];
    loop {
        for (index, child) in children.iter_mut().enumerate() {
            if statuses[index].is_none() {
                statuses[index] = child.try_wait().expect("poll stress worker");
            }
        }
        if statuses.iter().all(Option::is_some) {
            break;
        }
        if Instant::now() >= deadline {
            let completed = statuses.iter().filter(|status| status.is_some()).count();
            for (child, status) in children.iter_mut().zip(&statuses) {
                if status.is_none() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            panic!(
                "same-artifact publication deadlocked or exceeded 20s: {completed}/{process_count} workers completed"
            );
        }
        thread::sleep(Duration::from_millis(20));
    }

    for (index, status) in statuses.into_iter().enumerate() {
        assert!(
            status.expect("worker status").success(),
            "stress worker {index} failed"
        );
    }

    let store = ArtifactStore::open(store_dir.path()).unwrap();
    store
        .verify_artifact(parsed.digest())
        .expect("published artifact verifies after process contention");
    assert_eq!(glob::glob_artifacts(&store).len(), 1);
    assert_eq!(
        server.hits(),
        1,
        "per-artifact process lock must permit exactly one download"
    );
    assert_eq!(
        fs::read_dir(store.root().join("tmp")).unwrap().count(),
        0,
        "stress publication must leave no temporary files"
    );
}

#[test]
#[cfg(unix)]
fn read_only_publication_fails_clearly() {
    use std::os::unix::fs::PermissionsExt as _;
    let tgz = fixture_tgz();
    let integrity = integrity_of(&tgz);
    let integ = Integrity::parse(&integrity).unwrap();
    let server = MiniServer::start(tgz);
    let url = server.url_for();

    let store_dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::open(store_dir.path()).unwrap();

    // Pre-create the artifact prefix dir as read-only.
    let prefix = &integ.digest().to_hex()[..2];
    let prefix_dir = store.root().join("artifacts/sha512").join(prefix);
    fs::create_dir_all(&prefix_dir).unwrap();
    fs::set_permissions(&prefix_dir, fs::Permissions::from_mode(0o555)).unwrap();

    let mut m = Metrics::new();
    let err = store
        .ensure_artifact(&url, Some(&integ), &mut m)
        .expect_err("write to read-only store must fail");
    // Must surface an io store error referencing the path, not panic.
    assert!(format!("{err:#}").contains("io error") || format!("{err:#}").contains("store"));

    // Restore so the tempdir can be cleaned up.
    fs::set_permissions(&prefix_dir, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Test-local helper counting archive files under the store.
mod glob {
    use bpm::store::ArtifactStore;
    use std::path::PathBuf;
    pub fn glob_artifacts(store: &ArtifactStore) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for grp in std::fs::read_dir(store.root().join("artifacts/sha512")).unwrap() {
            let grp = grp.unwrap();
            for f in std::fs::read_dir(grp.path()).unwrap() {
                let f = f.unwrap();
                if f.path().extension().and_then(|e| e.to_str()) == Some("tgz") {
                    out.push(f.path());
                }
            }
        }
        out
    }
}
