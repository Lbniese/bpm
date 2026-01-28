//! Store behavior tests (AGENTS "Store changes"): integrity mismatch,
//! interrupted writes, concurrent writers (in-process threads), corrupt
//! existing objects, read-only publication, atomic reuse, and image reuse.

mod common;

use std::fs;
use std::sync::Arc;
use std::thread;

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
