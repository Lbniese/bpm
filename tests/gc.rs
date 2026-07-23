use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bpm::gc::policy::GcPolicy;
use bpm::metadata::{MetadataRepository, ObjectKey, ObjectRecord, Timestamp};

fn id(byte: u8, length: usize) -> String {
    std::iter::repeat_n(format!("{byte:x}"), length).collect()
}

#[test]
fn collects_old_unreferenced_objects_but_keeps_recent_objects() {
    let temp = tempfile::tempdir().unwrap();
    let repository = MetadataRepository::open(temp.path()).unwrap();
    let old = id(1, 128);
    let recent = id(2, 128);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    for (value, published) in [(&old, 1), (&recent, now)] {
        let key = ObjectKey::artifact(value.clone()).unwrap();
        let path = temp.path().join("artifacts/sha512").join(&value[..2]);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join(format!("{value}.tgz")), vec![0_u8; 4]).unwrap();
        repository
            .record_publication(&ObjectRecord {
                key,
                size_bytes: 4,
                published_at: Timestamp::from_millis(published),
            })
            .unwrap();
    }

    let report = repository
        .collect(GcPolicy {
            grace: Duration::from_secs(1),
            max_size_bytes: None,
        })
        .unwrap();
    assert_eq!(report.deleted, 1);
    assert!(!temp
        .path()
        .join("artifacts/sha512/11")
        .join(format!("{old}.tgz"))
        .exists());
    assert!(temp
        .path()
        .join("artifacts/sha512/22")
        .join(format!("{recent}.tgz"))
        .exists());
}

#[test]
fn size_limit_only_reclaims_eligible_objects() {
    let temp = tempfile::tempdir().unwrap();
    let repository = MetadataRepository::open(temp.path()).unwrap();
    let value = id(3, 128);
    let path = temp.path().join("artifacts/sha512/33");
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join(format!("{value}.tgz")), vec![0_u8; 10]).unwrap();
    repository
        .record_publication(&ObjectRecord {
            key: ObjectKey::artifact(value).unwrap(),
            size_bytes: 10,
            published_at: Timestamp::from_millis(1),
        })
        .unwrap();
    let report = repository
        .collect(GcPolicy {
            grace: Duration::from_secs(1),
            max_size_bytes: Some(1),
        })
        .unwrap();
    assert!(report.evaluation.unwrap().cap_reachable);
    assert_eq!(report.deleted, 1);
}

#[test]
fn gc_collects_orphaned_derived_image_discovered_from_disk() {
    // The derived store is filesystem-authoritative: in normal operation the
    // metadata DB is never told about a derived image directly (the Phase 2
    // adapter is a no-op). GC must still bound `<store>/derived/` growth, so
    // repair_index rebuilds the index from disk and an aged-out, orphaned
    // derived image becomes collectible without any publish call. This is the
    // safety gate that lets the derived store default on without unbounded disk
    // growth.
    let temp = tempfile::tempdir().unwrap();
    let repository = MetadataRepository::open(temp.path()).unwrap();

    let stale = id(1, 64); // 64-char hex (BLAKE3) derived id
    let fresh = id(2, 64);
    for value in [stale.as_str(), fresh.as_str()] {
        let dir = temp
            .path()
            .join("derived/blake3")
            .join(&value[..2])
            .join(value);
        fs::create_dir_all(dir.join("image")).unwrap();
        fs::write(dir.join("metadata.json"), b"{}").unwrap();
        fs::write(dir.join("image").join("package.json"), b"{}").unwrap();
        // Mirror the real store's sealed layout: regular files are published
        // read-only for content immutability, while directories stay writable
        // so the tree remains deletable. GC must still collect this shape.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for file in [
                dir.join("metadata.json"),
                dir.join("image").join("package.json"),
            ] {
                fs::set_permissions(&file, fs::Permissions::from_mode(0o444)).unwrap();
            }
        }
    }
    // Age the stale image well past the grace window; leave the fresh one
    // recent (mtime ~ now).
    let old = SystemTime::now() - Duration::from_secs(60);
    fs::File::open(temp.path().join("derived/blake3/11").join(&stale))
        .unwrap()
        .set_modified(old)
        .unwrap();

    let report = repository
        .collect(GcPolicy {
            grace: Duration::from_secs(1),
            max_size_bytes: None,
        })
        .unwrap();
    assert_eq!(
        report.deleted, 1,
        "the aged-out derived image should be collected",
    );
    assert!(
        !temp.path().join("derived/blake3/11").join(&stale).exists(),
        "stale derived image must be removed from disk",
    );
    assert!(
        temp.path().join("derived/blake3/22").join(&fresh).exists(),
        "fresh derived image must be retained",
    );
}

#[test]
fn incomplete_legacy_graph_is_preserved_and_protects_artifacts() {
    // A graph volume whose durable metadata lacks a complete inventory (a
    // legacy/pre-inventory volume) must be marked `complete=0` by repair_index
    // and protected from collection. Because its dependency set is unknown, GC
    // must also fail closed for artifacts/images/derived while any incomplete
    // graph exists.
    let temp = tempfile::tempdir().unwrap();
    let repository = MetadataRepository::open(temp.path()).unwrap();

    let artifact = id(1, 128);
    let graph = id(7, 64); // 64-hex graph id

    // Publish an artifact.
    let art_dir = temp.path().join("artifacts/sha512").join(&artifact[..2]);
    fs::create_dir_all(&art_dir).unwrap();
    fs::write(art_dir.join(format!("{artifact}.tgz")), vec![0_u8; 4]).unwrap();

    // Publish a graph volume with a LEGACY marker (no inventory_version field
    // => inventory_version=0 => incomplete). repair_index must treat it as
    // complete=0 (protected) and never reconstruct edges.
    let graph_dir = temp
        .path()
        .join("graphs/blake3")
        .join(&graph[..2])
        .join(&graph);
    fs::create_dir_all(&graph_dir).unwrap();
    fs::write(
        graph_dir.join("metadata.json"),
        format!(
            r#"{{"graph_id_hex":"{graph}","layout_version":6,"packages_materialized":1,"bins_linked":0}}"#
        ),
    )
    .unwrap();

    // Age both well past the grace window.
    let old = SystemTime::now() - Duration::from_secs(60);
    for path in [
        art_dir.join(format!("{artifact}.tgz")).as_path(),
        graph_dir.as_path(),
    ] {
        fs::File::open(path).unwrap().set_modified(old).unwrap();
    }

    let report = repository
        .collect(GcPolicy {
            grace: Duration::from_secs(1),
            max_size_bytes: None,
        })
        .unwrap();

    // Nothing may be collected: the incomplete graph protects itself and,
    // because its dependency set is unknown, fail-closes artifact reclamation.
    assert_eq!(
        report.deleted, 0,
        "incomplete graph + its potential deps must be preserved"
    );
    assert!(art_dir.join(format!("{artifact}.tgz")).exists());
    assert!(graph_dir.exists());
}

#[test]
fn lease_protects_object_from_collection_until_released() {
    use bpm::metadata::{LeaseGuard, LeaseOptions};

    // An active install lease protects its objects from GC even before any
    // project registration exists. Once the lease is released (install done)
    // and the objects are aged, a complete, unreferenced object becomes
    // collectible again — proving GC is not globally disabled.
    let temp = tempfile::tempdir().unwrap();
    let repository = MetadataRepository::open(temp.path()).unwrap();

    let artifact = id(9, 128);
    let art_dir = temp.path().join("artifacts/sha512").join(&artifact[..2]);
    fs::create_dir_all(&art_dir).unwrap();
    fs::write(art_dir.join(format!("{artifact}.tgz")), vec![0_u8; 4]).unwrap();
    let key = ObjectKey::artifact(artifact.clone()).unwrap();
    repository
        .record_publication(&ObjectRecord {
            key: key.clone(),
            size_bytes: 4,
            published_at: Timestamp::from_millis(1),
        })
        .unwrap();

    // Fast lease options keep the test brisk while honoring ttl >= 3*renew.
    let options = LeaseOptions {
        ttl: Duration::from_millis(3000),
        renew_every: Duration::from_millis(1000),
    };
    let lease: LeaseGuard = repository.acquire_lease(&[key], options).unwrap();

    // Age the artifact, then GC while the lease is held: it must be retained.
    let old = SystemTime::now() - Duration::from_secs(60);
    fs::File::open(art_dir.join(format!("{artifact}.tgz")))
        .unwrap()
        .set_modified(old)
        .unwrap();
    let report = repository
        .collect(GcPolicy {
            grace: Duration::from_secs(1),
            max_size_bytes: None,
        })
        .unwrap();
    assert_eq!(report.deleted, 0, "leased object must survive GC");
    assert!(art_dir.join(format!("{artifact}.tgz")).exists());

    // Release the lease (install completes); the now-unreferenced, aged object
    // becomes collectible.
    drop(lease);
    let report = repository
        .collect(GcPolicy {
            grace: Duration::from_secs(1),
            max_size_bytes: None,
        })
        .unwrap();
    assert_eq!(
        report.deleted, 1,
        "released/unreferenced object must be reclaimable"
    );
    assert!(!art_dir.join(format!("{artifact}.tgz")).exists());
}
