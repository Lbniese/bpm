//! Canonical advisory locks for store objects.
//!
//! One source of truth for the lock-file name of every immutable object so
//! that store/volume/derived *writers*, install *lease acquisition*, and GC
//! *deletion* cannot drift to different names and race. Lock files always live
//! beneath `<store>/locks`; a database path is never trusted as a lock target.
//!
//! The names here exactly reproduce the historical writer conventions:
//!
//! | kind     | lock file            |
//! |----------|----------------------|
//! | artifact | `<id>.lock`          |
//! | image    | `img-<id>.lock`      |
//! | derived  | `derived-<id>.lock`  |
//! | graph    | `graph-<id>.lock`    |
//! | plan     | `plan-<id>.lock`     |

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use crate::metadata::{ObjectKey, ObjectKind};

const LOCKS: &str = "locks";

/// `<store>/locks`.
pub(crate) fn lock_dir(store_root: &Path) -> PathBuf {
    store_root.join(LOCKS)
}

/// The lock-file stem (without the `.lock` suffix) for one object.
pub(crate) fn lock_stem(key: &ObjectKey) -> String {
    let id = key.id();
    match key.kind() {
        ObjectKind::Artifact => id.to_owned(),
        ObjectKind::Image => format!("img-{id}"),
        ObjectKind::Derived => format!("derived-{id}"),
        ObjectKind::Graph => format!("graph-{id}"),
        ObjectKind::Plan => format!("plan-{id}"),
    }
}

/// The exact lock-file path for one object beneath `<store>/locks`.
pub(crate) fn lock_path(store_root: &Path, key: &ObjectKey) -> PathBuf {
    lock_dir(store_root).join(format!("{}.lock", lock_stem(key)))
}

/// RAII guard holding an exclusive advisory lock on one object's lock file.
/// Dropping it closes (and thus releases) the underlying file.
pub(crate) struct ObjectLock {
    _file: fs::File,
}

/// Acquire an exclusive advisory lock (blocking) on `key`'s canonical lock
/// file beneath `<store>/locks`. Returns when the lock is held.
pub(crate) fn acquire(store_root: &Path, key: &ObjectKey) -> std::io::Result<ObjectLock> {
    fs::create_dir_all(lock_dir(store_root))?;
    let path = lock_path(store_root, key);
    // `truncate(false)`: lock files are markers only and must never disturb
    // a lock concurrently held by another process.
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    // `std::fs::File` inherent exclusive advisory lock (stable Rust 1.68+).
    file.lock()?;
    Ok(ObjectLock { _file: file })
}

/// Acquire locks over a canonical sorted, deduplicated object set.
///
/// Keys are sorted by `ObjectKey` order (and de-duplicated) before acquisition
/// so that any two callers locking overlapping sets acquire them in the same
/// global order and cannot deadlock.
pub(crate) fn acquire_all(
    store_root: &Path,
    keys: &[ObjectKey],
) -> std::io::Result<Vec<ObjectLock>> {
    let mut ordered: Vec<&ObjectKey> = keys.iter().collect();
    ordered.sort();
    ordered.dedup();
    ordered
        .into_iter()
        .map(|key| acquire(store_root, key))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn art(c: char) -> ObjectKey {
        ObjectKey::artifact(c.to_string().repeat(128)).unwrap()
    }
    fn img(c: char) -> ObjectKey {
        ObjectKey::image(c.to_string().repeat(128)).unwrap()
    }
    fn der(c: char) -> ObjectKey {
        ObjectKey::derived(c.to_string().repeat(64)).unwrap()
    }
    fn gra(c: char) -> ObjectKey {
        ObjectKey::graph(c.to_string().repeat(64)).unwrap()
    }
    fn plan(c: char) -> ObjectKey {
        ObjectKey::plan(c.to_string().repeat(64)).unwrap()
    }

    #[test]
    fn lock_stems_match_writer_conventions_exactly() {
        // These strings must equal what src/store.rs, src/derived/store.rs,
        // and src/volume.rs historically built inline.
        assert_eq!(lock_stem(&art('a')), "a".repeat(128));
        assert_eq!(lock_stem(&img('a')), format!("img-{}", "a".repeat(128)));
        assert_eq!(lock_stem(&der('b')), format!("derived-{}", "b".repeat(64)));
        assert_eq!(lock_stem(&gra('c')), format!("graph-{}", "c".repeat(64)));
        assert_eq!(lock_stem(&plan('d')), format!("plan-{}", "d".repeat(64)));
    }

    #[test]
    fn lock_paths_live_under_locks_dir() {
        let root = Path::new("/tmp/bpm-store");
        assert_eq!(
            lock_path(root, &art('a')),
            Path::new("/tmp/bpm-store/locks").join(format!("{}.lock", "a".repeat(128)))
        );
        assert_eq!(
            lock_path(root, &gra('c')),
            Path::new("/tmp/bpm-store/locks").join(format!("graph-{}.lock", "c".repeat(64)))
        );
    }

    #[test]
    fn acquire_all_sorts_and_dedups_keys() {
        let temp = tempfile::tempdir().unwrap();
        let a = art('a');
        let a2 = art('a');
        let b = art('b');
        // Same kind, distinct ids; duplicates collapsed; sorted by key. Held
        // until the guard vector drops, proving two distinct locks coexist.
        let guards = acquire_all(temp.path(), &[b.clone(), a.clone(), a2]).unwrap();
        assert_eq!(guards.len(), 2, "duplicate keys must collapse to one lock");
        drop(guards);
        // After release, both locks are independently re-acquirable.
        let _g1 = acquire(temp.path(), &a).unwrap();
        let _g2 = acquire(temp.path(), &b).unwrap();
    }
}
