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
