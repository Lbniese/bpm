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
