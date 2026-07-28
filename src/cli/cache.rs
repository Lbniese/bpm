//! `bpm cache` — inspect and reclaim the global artifact + metadata cache.
//!
//! npm `cache` compatibility. The store root hosts both the content-addressable
//! [`ArtifactStore`] (`artifacts/sha512`, `images/sha512`, `graphs/blake3`,
//! `tmp`, `locks`) and the [`MetadataRepository`] (`store.db` + namespace
//! dirs). All four subcommands operate on that single root.
//!
//! - `bpm cache` / `bpm cache ls` — read-only size + object-count breakdown.
//! - `bpm cache verify` — run a repair + garbage-collection pass and report.
//! - `bpm cache clean` — reclaim every unreferenced object (protected installs
//!   are preserved by their durable leases/registrations).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use bpm::metadata::{GcReport, MetadataRepository};

use crate::cli::fetch::store_root;

pub(crate) struct Options {
    pub(crate) action: Option<String>,
    pub(crate) store: Option<PathBuf>,
}

pub(crate) fn run(options: Options) -> Result<()> {
    let root = store_root(options.store)?;
    let action = options.action.as_deref().unwrap_or("ls");
    match action {
        "ls" | "list" => run_ls(&root),
        "verify" => run_verify(&root),
        "clean" => run_clean(&root),
        other => {
            anyhow::bail!("unknown cache action {other:?}; expected one of: ls, verify, clean")
        }
    }
}

/// Read-only breakdown of cache size and object counts by area.
fn run_ls(root: &Path) -> Result<()> {
    println!("bpm cache: {}", root.display());

    if !root.exists() {
        println!("  (cache not initialized)");
        return Ok(());
    }

    let artifacts = area(root, "artifacts/sha512");
    let images = area(root, "images/sha512");
    let graphs = area(root, "graphs/blake3");
    let db = root.join("store.db");
    let db_bytes = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
    let scratch = scratch_area(root);
    let total = total_size(root);

    println!(
        "  artifacts   {:>6} tarball(s)   {}",
        artifacts.count,
        format_bytes(artifacts.bytes)
    );
    println!(
        "  images      {:>6} image(s)     {}",
        images.dirs,
        format_bytes(images.bytes)
    );
    println!(
        "  graphs      {:>6} volume(s)    {}",
        graphs.dirs,
        format_bytes(graphs.bytes)
    );
    println!("  metadata    store.db          {}", format_bytes(db_bytes));
    println!("  scratch     tmp + locks       {}", format_bytes(scratch));
    println!(
        "  total                         {}",
        format_bytes(total.bytes)
    );
    Ok(())
}

/// Repair + garbage-collect with the default policy, then report.
fn run_verify(root: &Path) -> Result<()> {
    println!("verifying cache: {}", root.display());
    let repository =
        MetadataRepository::open(root).with_context(|| format!("open store {}", root.display()))?;
    let report = repository.collect(bpm::gc::policy::GcPolicy::default())?;
    print_report(&report);
    Ok(())
}

/// Reclaim every unreferenced object. Protected installs (held by durable
/// leases/registrations) are preserved, so a clean never breaks a running or
/// registered install.
fn run_clean(root: &Path) -> Result<()> {
    println!("cleaning cache: {}", root.display());
    let repository =
        MetadataRepository::open(root).with_context(|| format!("open store {}", root.display()))?;
    let report = repository.collect(bpm::gc::policy::GcPolicy {
        grace: Duration::ZERO,
        max_size_bytes: Some(0),
    })?;
    print_report(&report);
    Ok(())
}

fn print_report(report: &GcReport) {
    let repaired = &report.repaired;
    if repaired.removed_stale > 0 || !repaired.unknown_entries.is_empty() {
        println!(
            "  repaired: {} stale lock(s), {} unknown entr(y/ies)",
            repaired.removed_stale,
            repaired.unknown_entries.len()
        );
    }
    println!(
        "  reclaimed {} object(s), {}",
        report.deleted,
        format_bytes(report.deleted_bytes)
    );
    if report.preserved > 0 {
        println!(
            "  preserved {} object(s) (protected or in-use)",
            report.preserved
        );
    }
    if let Some(evaluation) = report.evaluation.as_ref() {
        if !evaluation.cap_reachable {
            eprintln!(
                "  warning: max-size cannot be reached without deleting protected or recent objects"
            );
        }
    }
}

#[derive(Default)]
struct AreaStats {
    bytes: u64,
    /// Count of regular files (used for `artifacts`, where each is a tarball).
    count: usize,
    /// Count of sharded leaf directories (used for `images`/`graphs`).
    dirs: usize,
}

/// Stats for one sharded content area (`artifacts/sha512`, `images/sha512`, or
/// `graphs/blake3`). `count` counts files; `dirs` counts the two-level sharded
/// leaf directories.
fn area(root: &Path, sub: &str) -> AreaStats {
    let base = root.join(sub);
    let mut stats = AreaStats::default();
    if !base.is_dir() {
        return stats;
    }
    // Sum bytes over the whole subtree and count regular files.
    walk(&base, &mut |bytes| {
        stats.bytes += bytes;
        stats.count += 1;
    });
    // Count leaf directories: base/<prefix>/<hash>.
    if let Ok(prefixes) = std::fs::read_dir(&base) {
        for prefix in prefixes.flatten() {
            if prefix.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Ok(leaves) = std::fs::read_dir(prefix.path()) {
                    for leaf in leaves.flatten() {
                        if leaf.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                            stats.dirs += 1;
                        }
                    }
                }
            }
        }
    }
    stats
}

/// Combined size of the `tmp` and `locks` scratch directories.
fn scratch_area(root: &Path) -> u64 {
    let mut bytes = 0u64;
    for sub in ["tmp", "locks"] {
        let dir = root.join(sub);
        walk(&dir, &mut |b| bytes += b);
    }
    bytes
}

/// Total bytes (and file count) across the entire store root.
fn total_size(root: &Path) -> AreaStats {
    let mut stats = AreaStats::default();
    walk(root, &mut |bytes| {
        stats.bytes += bytes;
        stats.count += 1;
    });
    stats
}

/// Recursively sum the size of every regular file under `dir`, invoking
/// `emit` once per file with its length. Symlinks and unreadable entries are
/// skipped silently so a partially-populated or locked store still reports.
fn walk(dir: &Path, emit: &mut impl FnMut(u64)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Use symlink_metadata so a dangling/relative symlink is never followed.
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_file() {
            emit(meta.len());
        } else if meta.is_dir() {
            walk(&path, emit);
        }
    }
}

/// Human-readable byte size (binary units), e.g. `1.4 GiB`, `512 B`.
fn format_bytes(bytes: u64) -> String {
    const UNITS: &[(&str, u64)] = &[
        ("PiB", 1u64 << 50),
        ("TiB", 1u64 << 40),
        ("GiB", 1u64 << 30),
        ("MiB", 1u64 << 20),
        ("KiB", 1u64 << 10),
    ];
    for (unit, threshold) in UNITS {
        if bytes >= *threshold {
            return format!("{:.1} {unit}", bytes as f64 / *threshold as f64);
        }
    }
    format!("{bytes} B")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_human_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes((1.5 * (1u64 << 30) as f64) as u64), "1.5 GiB");
    }

    #[test]
    fn walk_sums_regular_files_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.tgz"), [0u8; 100]).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("b.tgz"), [0u8; 50]).unwrap();
        // A symlink should not be followed into its target's size.
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("a.tgz"), root.join("link")).unwrap();

        let mut bytes = 0u64;
        let mut files = 0usize;
        walk(root, &mut |b| {
            bytes += b;
            files += 1;
        });
        assert_eq!(bytes, 150);
        // a.tgz, b.tgz; the symlink is not a regular file so it is not counted.
        assert_eq!(files, 2);
    }

    #[test]
    fn area_counts_sharded_leaves() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        // Mimic images/sha512/<prefix>/<hash> with two leaves.
        std::fs::create_dir_all(root.join("images/sha512/ab/abcd")).unwrap();
        std::fs::create_dir_all(root.join("images/sha512/ab/abef")).unwrap();
        std::fs::create_dir_all(root.join("images/sha512/cd/cdef")).unwrap();
        std::fs::write(root.join("images/sha512/ab/abcd/file"), [0u8; 10]).unwrap();

        let stats = area(root, "images/sha512");
        assert_eq!(stats.dirs, 3, "three leaf image dirs");
        assert_eq!(stats.count, 1, "one regular file");
        assert_eq!(stats.bytes, 10);
    }

    #[test]
    fn area_handles_missing_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stats = area(tmp.path(), "artifacts/sha512");
        assert_eq!(stats.bytes, 0);
        assert_eq!(stats.count, 0);
        assert_eq!(stats.dirs, 0);
    }
}
