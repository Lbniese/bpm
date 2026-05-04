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
///
/// Every regular file under the store root is classified into exactly one
/// category, so the printed category bytes always sum to the printed total.
/// Derived objects, plans, and resolution snapshots (previously invisible)
/// and any unrecognized files (reported under `other`) are now included.
fn run_ls(root: &Path) -> Result<()> {
    println!("bpm cache: {}", root.display());

    if !root.exists() {
        println!("  (cache not initialized)");
        return Ok(());
    }

    let stats = collect_categories(root);
    let total: u64 = stats.iter().map(|s| s.bytes).sum();

    // Stable order for CLI consumers. Sharded content areas (images, derived,
    // graphs) report object (leaf-dir) counts; file-based areas report file
    // counts. Bytes are reported for every category so the total reconciles.
    for (key, label, unit) in CATEGORY_DISPLAY {
        let s = stats[category_index(key)];
        let count = if matches!(*key, "images" | "derived" | "graphs") {
            s.objects
        } else {
            s.files
        };
        println!(
            "  {label:<12}{count:>6} {unit:<13}{}",
            format_bytes(s.bytes)
        );
    }
    println!("  total                         {}", format_bytes(total));
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

/// One cache category: bytes plus a regular-file count, and (for sharded
/// content areas) a leaf-directory object count.
#[derive(Default, Clone, Copy)]
struct CategoryStats {
    bytes: u64,
    files: usize,
    objects: usize,
}

/// Stable display order: `(category key, label, count unit)`. CLI consumers
/// may rely on this ordering and these labels.
const CATEGORY_DISPLAY: &[(&str, &str, &str)] = &[
    ("artifacts", "artifacts", "tarball(s)"),
    ("images", "images", "image(s)"),
    ("derived", "derived", "object(s)"),
    ("graphs", "graphs", "volume(s)"),
    ("plans", "plans", "plan(s)"),
    ("snapshots", "snapshots", "snapshot(s)"),
    ("metadata", "metadata", "db file(s)"),
    ("scratch", "scratch", "file(s)"),
    ("leases", "leases", "lease(s)"),
    ("links", "links", "file(s)"),
    ("other", "other", "file(s)"),
];

/// Index of a category key within the `CategoryStats` vector returned by
/// [`collect_categories`]. `"other"` is the catch-all tail.
fn category_index(key: &str) -> usize {
    match key {
        "artifacts" => 0,
        "images" => 1,
        "derived" => 2,
        "graphs" => 3,
        "plans" => 4,
        "snapshots" => 5,
        "metadata" => 6,
        "scratch" => 7,
        "leases" => 8,
        "links" => 9,
        _ => 10,
    }
}

/// Classify a top-level store subdirectory by name into a category.
fn dir_category(name: &str) -> &'static str {
    match name {
        "artifacts" => "artifacts",
        "images" => "images",
        "derived" => "derived",
        "graphs" => "graphs",
        "plans" => "plans",
        "resolution-snapshots" => "snapshots",
        "locks" | "tmp" => "scratch",
        "leases" => "leases",
        "links" => "links",
        "metadata" => "metadata",
        _ => "other",
    }
}

/// Classify a file that lives directly in the store root (no subdirectory).
fn root_file_category(name: &std::ffi::OsStr) -> &'static str {
    match name.to_str() {
        Some("store.db") | Some("metadata-cache.db") => "metadata",
        _ => "other",
    }
}

/// Walk the store root once and classify every regular file into exactly one
/// category. Symlinks and unreadable entries are skipped (via [`walk`]'s
/// `symlink_metadata` discipline) so a partially-populated or locked store
/// still reports. The returned vector is indexed by [`category_index`].
fn collect_categories(root: &Path) -> Vec<CategoryStats> {
    let mut stats: Vec<CategoryStats> = CATEGORY_DISPLAY
        .iter()
        .map(|_| CategoryStats::default())
        .collect();
    let Ok(entries) = std::fs::read_dir(root) else {
        return stats;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `symlink_metadata` so a dangling/relative symlink is never followed.
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_file() {
            let key = root_file_category(&entry.file_name());
            let s = &mut stats[category_index(key)];
            s.bytes += meta.len();
            s.files += 1;
        } else if meta.is_dir() {
            let key = dir_category(&entry.file_name().to_string_lossy());
            let s = &mut stats[category_index(key)];
            walk(&path, &mut |bytes| {
                s.bytes += bytes;
                s.files += 1;
            });
            // Preserve useful object-count semantics for sharded areas, where
            // each two-level leaf directory is one object/volume/image.
            if matches!(key, "images" | "derived" | "graphs") {
                s.objects += count_sharded_leaves(&path);
            }
        }
    }
    stats
}

/// Count the two-level sharded leaf directories under `base`
/// (`base/<prefix>/<hash>`). Used for the object counts of `images`, `derived`,
/// and `graphs`.
fn count_sharded_leaves(base: &Path) -> usize {
    let mut count = 0;
    let Ok(prefixes) = std::fs::read_dir(base) else {
        return 0;
    };
    for prefix in prefixes.flatten() {
        if !prefix.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if let Ok(leaves) = std::fs::read_dir(prefix.path()) {
            for leaf in leaves.flatten() {
                if leaf.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    count += 1;
                }
            }
        }
    }
    count
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
    fn collect_categories_reconciles_to_total() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        // One representative regular file per category, including previously
        // invisible areas (derived, plans, resolution-snapshots) and an
        // unknown directory + stray root file routed to `other`.
        let mk = |sub: &str, name: &str, bytes: usize| {
            let dir = root.join(sub);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(name), vec![0u8; bytes]).unwrap();
        };
        mk("artifacts/sha512/ab", "c1.tgz", 100);
        mk("images/sha512/ab/cd", "meta", 10);
        mk("derived/blake3/ab/cd", "out", 40);
        mk("graphs/blake3/ab/cd", "vol", 200);
        mk("plans/blake3/ab", "p.bin", 30);
        mk("resolution-snapshots", "s.json", 16);
        mk("tmp", "x", 5);
        mk("locks", "l", 3);
        mk("leases", "lease1", 7);
        mk("links", "lnk", 9);
        mk("metadata", "migrations.sql", 11);
        std::fs::write(root.join("store.db"), vec![0u8; 80]).unwrap();
        std::fs::write(root.join("metadata-cache.db"), vec![0u8; 12]).unwrap();
        // Unknown dir + unknown root file land in `other`.
        mk("future-area", "thing", 22);
        std::fs::write(root.join("stray.bin"), vec![0u8; 4]).unwrap();
        // A symlink must never be followed into its target's size.
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("store.db"), root.join("store.db.link")).unwrap();

        let stats = collect_categories(root);
        let sum: u64 = stats.iter().map(|s| s.bytes).sum();
        let expected = 100 + 10 + 40 + 200 + 30 + 16 + 5 + 3 + 7 + 9 + 11 + 80 + 12 + 22 + 4;
        assert_eq!(sum, expected);

        // The category sum must equal an independent full-tree walk.
        let mut total = 0u64;
        walk(root, &mut |b| total += b);
        assert_eq!(
            sum, total,
            "category bytes must reconcile to total regular-file bytes"
        );

        // Spot-check classifications.
        assert_eq!(stats[category_index("artifacts")].bytes, 100);
        assert_eq!(stats[category_index("images")].bytes, 10);
        assert_eq!(stats[category_index("derived")].bytes, 40);
        assert_eq!(stats[category_index("snapshots")].bytes, 16);
        assert_eq!(stats[category_index("metadata")].bytes, 11 + 80 + 12);
        assert_eq!(stats[category_index("other")].bytes, 22 + 4);
        // Sharded leaf-object counts are preserved.
        assert_eq!(stats[category_index("images")].objects, 1);
        assert_eq!(stats[category_index("derived")].objects, 1);
        assert_eq!(stats[category_index("graphs")].objects, 1);
        // File-based areas are not leaf-counted.
        assert_eq!(stats[category_index("artifacts")].objects, 0);
        assert_eq!(stats[category_index("artifacts")].files, 1);
    }

    #[test]
    fn collect_categories_handles_missing_store() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stats = collect_categories(tmp.path());
        let sum: u64 = stats.iter().map(|s| s.bytes).sum();
        assert_eq!(sum, 0);
    }
}
