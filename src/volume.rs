//! Reusable graph volumes (IMPLEMENTATION §13 — Milestone 4).
//!
//! A graph volume is an immutable, complete `node_modules` projection held in
//! the global store, keyed by [`GraphId`]. Building it is a one-time,
//! graph-keyed, idempotent operation; any project that shares the same graph
//! reuses it.
//!
//! Project attachment is **shallow**: instead of symlinking every locked
//! package into the project, the project's `node_modules` becomes a small set
//! of top-level relays (one per top-level entry in the volume's `node_modules`,
//! including `.bin`) pointing into the volume. So a second project with the
//! same graph does O(top-level-entries) filesystem work rather than
//! O(all-packages) — the headline performance win.
//!
//! Nested package paths (`node_modules/a/node_modules/b`) live entirely inside
//! the volume and are reached transitively through the top-level relays, so
//! project-relative resolution keeps working (IMPLEMENTATION §13: "Begin with
//! the shallow project-local root").

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::graph::{graph_id_with_prepared, ManagedEntry, IDENTITY_RELAY, IDENTITY_TREE};
use crate::integrity::ArtifactId;
use crate::lockfile::Lockfile;
#[cfg(unix)]
use crate::materializer::hardlink_tree;
#[cfg(unix)]
use crate::materializer::reflink_tree;
use crate::materializer::{
    materialize_with_backend, MaterializeBackend, MaterializeError, MaterializeStats,
};
use crate::metrics::Metrics;
use crate::store::ArtifactStore;

/// Marker file written at `<volume>/metadata.json` once the volume is complete.
/// Its presence with the right graph id is the reuse signal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct VolumeMeta {
    graph_id_hex: String,
    /// On-disk layout generation this volume was built with. A cached volume
    /// whose recorded layout differs from [`VOLUME_LAYOUT_VERSION`] is rebuilt.
    #[serde(default)]
    layout_version: u32,
    packages_materialized: usize,
    bins_linked: usize,
}

const META_FILE: &str = "metadata.json";

/// Bumped when the on-disk volume layout changes (e.g. symlink -> hardlink
/// materialization of package images). A cached volume whose recorded layout
/// differs is discarded and rebuilt so every project sees the current layout.
const VOLUME_LAYOUT_VERSION: u32 = 6;

#[derive(Debug, Error)]
pub enum VolumeError {
    #[error(transparent)]
    Materialize(MaterializeError),
    #[error("store io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl From<MaterializeError> for VolumeError {
    fn from(e: MaterializeError) -> Self {
        VolumeError::Materialize(e)
    }
}

/// A reference to a built (or pre-existing) graph volume.
#[derive(Debug, Clone)]
pub struct VolumeRef {
    /// `<store>/graphs/blake3/<prefix>/<graph-id>`
    pub path: PathBuf,
    /// `true` when the volume already was complete (no build performed).
    pub cached: bool,
    pub stats: MaterializeStats,
}

/// Result of ensuring a graph volume: either a ready (cached) volume or a
/// pending build that must be completed via [`PendingVolume::publish`].
#[derive(Debug)]
pub enum EnsuredVolume {
    Ready(VolumeRef),
    Building(PendingVolume),
}

/// A graph volume being built under a staging directory with an exclusive
/// per-graph lock held.  Must be published via [`publish`](PendingVolume::publish)
/// or dropped (which cleans up staging and releases the lock).
#[derive(Debug)]
pub struct PendingVolume {
    staging: PathBuf,
    final_path: PathBuf,
    graph_id_hex: String,
    stats: MaterializeStats,
    /// The lock file guard.  Kept alive for the lifetime of this object.
    _lock: fs::File,
}

/// Ensure the graph volume for `graph_hex` exists and is complete. Idempotent:
/// if `<volume>/metadata.json` already records this graph id, the volume is
/// reused untouched (cached). Otherwise the volume's `node_modules` is built
/// once from the lockfile + resolved artifact ids (reusing the materializer),
/// then the marker is written.
pub fn ensure_graph_volume(
    store: &ArtifactStore,
    lockfile: &Lockfile,
    artifact_ids: &[Option<ArtifactId>],
    metrics: &mut Metrics,
) -> Result<VolumeRef, VolumeError> {
    ensure_graph_volume_with_prepared(store, lockfile, artifact_ids, &BTreeMap::new(), metrics)
}

/// Ensure a graph volume with prepared package images overlaid on raw images.
///
/// `prepared` maps lockfile package paths to immutable derived images. The
/// graph key includes each image key, so a volume built from raw Git sources
/// can never be reused after preparation becomes available or changes.
pub fn ensure_graph_volume_with_prepared(
    store: &ArtifactStore,
    lockfile: &Lockfile,
    artifact_ids: &[Option<ArtifactId>],
    prepared: &BTreeMap<String, crate::lifecycle::PreparedImage>,
    metrics: &mut Metrics,
) -> Result<VolumeRef, VolumeError> {
    let prepared_keys = prepared
        .iter()
        .map(|(path, image)| (path.clone(), *image.key.as_bytes()))
        .collect::<BTreeMap<_, _>>();
    let gid = graph_id_with_prepared(lockfile, &prepared_keys);
    let graph_hex = gid.to_hex();
    let volume_dir = store.graph_volume_path(&graph_hex);

    // Reuse: a complete marker whose graph id AND layout version match means
    // another process/project already built this exact volume; attaching to it
    // is the fast path. A graph or layout mismatch means the on-disk volume is
    // stale and must be rebuilt from scratch.
    let mut stale = false;
    if let Ok(meta_bytes) = fs::read(volume_dir.join(META_FILE)) {
        if let Ok(meta) = serde_json::from_slice::<VolumeMeta>(&meta_bytes) {
            if meta.graph_id_hex == graph_hex && meta.layout_version == VOLUME_LAYOUT_VERSION {
                metrics.record("graph_volume_hit", std::time::Duration::ZERO);
                return Ok(VolumeRef {
                    path: volume_dir,
                    cached: true,
                    stats: MaterializeStats::default(),
                });
            }
            stale = true;
        }
    }

    // Acquire an exclusive per-graph lock to serialise concurrent builders.
    let lock = acquire_graph_lock(store, &graph_hex)?;

    // Double-check after acquiring the lock: another builder may have
    // completed the volume under lock while we were waiting.
    if let Ok(meta_bytes) = fs::read(volume_dir.join(META_FILE)) {
        if let Ok(meta) = serde_json::from_slice::<VolumeMeta>(&meta_bytes) {
            if meta.graph_id_hex == graph_hex && meta.layout_version == VOLUME_LAYOUT_VERSION {
                metrics.record("graph_volume_hit", std::time::Duration::ZERO);
                return Ok(VolumeRef {
                    path: volume_dir,
                    cached: true,
                    stats: MaterializeStats::default(),
                });
            }
        }
    }
    if stale {
        // Discard the stale projection now that we hold the lock, so the
        // rebuild contains no orphan entries from the previous layout/graph.
        // Removing hardlinked entries only unlinks directory entries; the
        // shared store images persist.
        let _ = fs::remove_dir_all(&volume_dir);
    }

    // Build under a staging directory so that a crash during materialization
    // or overlay leaves no partial volume visible.  Staging is cleaned up by
    // PendingVolume::drop if publish is not called.
    let staging_base = store.root().join("tmp");
    fs::create_dir_all(&staging_base).map_err(|source| VolumeError::Io {
        path: staging_base.display().to_string(),
        source,
    })?;
    let staging = staging_base.join(format!("graph-{}-{}", graph_hex, std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(staging.join("node_modules")).map_err(|source| io_err(&staging, source))?;

    // Materialize the full node_modules projection inside staging as
    // HARDLINKS (real directories sharing inodes with the immutable store
    // images) rather than symlinks into the store. A package's realpath then
    // lands inside the volume, where `node_modules/<self>` is reachable as a
    // sibling, so self-referential requires (e.g. `require('next/...')` issued
    // from within next's own code) resolve instead of escaping into the store
    // (which has no node_modules and breaks them).
    let resolved: Vec<(_, ArtifactId)> = artifact_ids
        .iter()
        .zip(lockfile.packages.iter())
        .filter_map(|(maybe_id, pkg)| maybe_id.map(|id| (pkg, id)))
        .collect();
    let stats = materialize_with_backend(
        staging.as_path(),
        store,
        &resolved,
        MaterializeBackend::Hardlink,
    )?;
    for package in lockfile.packages.iter().filter(|package| package.link) {
        let Some(source) = package.workspace_target.as_deref() else {
            continue;
        };
        let target = staging.join(&package.path);
        overlay_prepared_image(Path::new(source), &target)?;
    }
    for (package_path, prepared_image) in prepared {
        let target = staging.join(package_path);
        overlay_prepared_image(&prepared_image.image_path, &target)?;
    }

    // Atomically publish the staging directory to the final path.
    let pending = PendingVolume {
        staging,
        final_path: volume_dir,
        graph_id_hex: graph_hex,
        stats,
        _lock: lock,
    };
    let vref = pending.publish()?;

    metrics.record("graph_volume_build", std::time::Duration::ZERO);
    Ok(vref)
}

/// Acquire an exclusive per-graph lock file at `<store>/locks/graph-<hex>.lock`.
pub fn acquire_graph_lock(store: &ArtifactStore, graph_hex: &str) -> Result<fs::File, VolumeError> {
    let lock_dir = store.root().join("locks");
    fs::create_dir_all(&lock_dir).map_err(|source| VolumeError::Io {
        path: lock_dir.display().to_string(),
        source,
    })?;
    let lock_path = lock_dir.join(format!("graph-{graph_hex}.lock"));
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| VolumeError::Io {
            path: lock_path.display().to_string(),
            source,
        })?;
    // Acquire exclusive advisory lock (Rust 1.68+).
    file.lock().map_err(|source| VolumeError::Io {
        path: lock_path.display().to_string(),
        source,
    })?;
    Ok(file)
}

impl PendingVolume {
    /// The staging path where lifecycle and materialization happen.
    pub fn path(&self) -> &Path {
        &self.staging
    }

    /// Atomically publish the staging tree to the final graph path.
    /// After success the volume is ready for reuse.
    pub fn publish(mut self) -> Result<VolumeRef, VolumeError> {
        // Write metadata inside staging before the rename.
        let meta = VolumeMeta {
            graph_id_hex: self.graph_id_hex.clone(),
            layout_version: VOLUME_LAYOUT_VERSION,
            packages_materialized: self.stats.packages_materialized,
            bins_linked: self.stats.bins_linked,
        };
        let meta_bytes = serde_json::to_vec_pretty(&meta).unwrap_or_default();
        fs::write(self.staging.join(META_FILE), meta_bytes)
            .map_err(|source| io_err(&self.staging.join(META_FILE), source))?;

        // Atomically rename staging to final path.  The lock serializes
        // concurrent builders, so the destination should not exist.  If it
        // does (e.g. a crash left a partial tree), remove it first.
        // Ensure the parent directory exists — the original code relied on
        // create_dir_all(volume_dir.join("node_modules")) for this, but
        // staging is created under tmp/ so the final path may not have a
        // parent yet.
        if let Some(parent) = self.final_path.parent() {
            fs::create_dir_all(parent).map_err(|source| VolumeError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let _ = fs::remove_dir_all(&self.final_path);
        fs::rename(&self.staging, &self.final_path).map_err(|source| VolumeError::Io {
            path: self.final_path.display().to_string(),
            source,
        })?;

        let vref = VolumeRef {
            path: self.final_path.clone(),
            cached: false,
            stats: std::mem::take(&mut self.stats),
        };
        // Prevent drop from cleaning the now-published tree.
        self.staging = PathBuf::new();
        Ok(vref)
    }
}

impl Drop for PendingVolume {
    fn drop(&mut self) {
        if !self.staging.as_os_str().is_empty() && self.staging.exists() {
            let _ = fs::remove_dir_all(&self.staging);
        }
        // Lock file is closed/cleaned up by the OS when the fd drops.
    }
}

fn overlay_prepared_image(source: &Path, target: &Path) -> Result<(), VolumeError> {
    fs::create_dir_all(target).map_err(|error| io_err(target, error))?;
    for entry in fs::read_dir(source).map_err(|error| io_err(source, error))? {
        let entry = entry.map_err(|error| io_err(source, error))?;
        if entry.file_name() == "node_modules" {
            continue;
        }
        let destination = target.join(entry.file_name());
        remove_any(&destination).map_err(|error| io_err(&destination, error))?;
        let source_path = entry.path();
        copy_overlay_entry(&source_path, &destination)?;
    }
    Ok(())
}

fn copy_overlay_entry(source: &Path, destination: &Path) -> Result<(), VolumeError> {
    let kind = fs::symlink_metadata(source).map_err(|error| io_err(source, error))?;
    if kind.is_dir() {
        fs::create_dir_all(destination).map_err(|error| io_err(destination, error))?;
        for entry in fs::read_dir(source).map_err(|error| io_err(source, error))? {
            let entry = entry.map_err(|error| io_err(source, error))?;
            copy_overlay_entry(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else if kind.file_type().is_symlink() {
        let _target = fs::read_link(source).map_err(|error| io_err(source, error))?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| io_err(parent, error))?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&_target, destination)
            .map_err(|error| io_err(destination, error))?;
        #[cfg(not(unix))]
        fs::copy(source, destination)
            .map(|_| ())
            .map_err(|error| io_err(destination, error))?;
    } else {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| io_err(parent, error))?;
        }
        fs::copy(source, destination).map_err(|error| io_err(destination, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(source)
                .map_err(|error| io_err(source, error))?
                .permissions();
            permissions.set_mode(permissions.mode() | 0o200);
            fs::set_permissions(destination, permissions)
                .map_err(|error| io_err(destination, error))?;
        }
    }
    Ok(())
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Counters for a shallow project attachment into a graph volume.
#[derive(Debug, Default, Clone, Copy)]
pub struct AttachStats {
    pub relays_created: usize,
    pub relays_unchanged: usize,
}

/// Result of attaching a graph volume into a project: counters plus the sorted,
/// deduplicated [`ManagedEntry`] set BPM actually created (one per shallow
/// top-level `node_modules` entry, including `.bin` and `@scope` containers).
/// Ownership is derived from the live post-attachment state, never inferred
/// from lockfile package paths, so reconciliation can prove BPM still owns
/// exactly these entries before removing any.
#[derive(Debug, Clone)]
pub struct AttachOutcome {
    pub stats: AttachStats,
    pub owned: Vec<ManagedEntry>,
}

impl AttachOutcome {
    /// Build an outcome from counters and an unsorted set of entries, sorting
    /// and deduplicating by path (the persisted contract requires sorted,
    /// unique ownership).
    fn new(stats: AttachStats, mut owned: Vec<ManagedEntry>) -> Self {
        owned.sort_by(|a, b| a.path.cmp(&b.path));
        owned.dedup_by(|a, b| a.path == b.path);
        Self { stats, owned }
    }
}

/// Deterministic BLAKE3 fingerprint of a directory tree, used as the identity
/// for local/reflink project-view entries. Walks entries in sorted relative-
/// path order using `symlink_metadata` (never follows a symlink): each entry
/// contributes a length-prefixed type byte, normalized relative path, and
/// content — file bytes for regular files, the `read_link` target for symlinks
/// (directories carry only their marker). Returns an error on any unreadable
/// entry rather than silently omitting it, so the fingerprint is a proof of
/// exact tree identity, not a best-effort digest.
pub fn tree_fingerprint(root: &Path) -> Result<String, VolumeError> {
    let mut entries = Vec::new();
    collect_relative_entries(root, &PathBuf::new(), &mut entries)?;
    entries.sort();
    let mut hasher = blake3::Hasher::new();
    for rel in &entries {
        let abs = root.join(rel);
        let meta = fs::symlink_metadata(&abs).map_err(|source| io_err(&abs, source))?;
        let ft = meta.file_type();
        let kind: u8 = if ft.is_symlink() {
            b's'
        } else if ft.is_dir() {
            b'd'
        } else {
            b'f'
        };
        hasher.update(&[kind]);
        len_str(&mut hasher, rel.as_bytes());
        match kind {
            b's' => {
                let target = fs::read_link(&abs).map_err(|source| io_err(&abs, source))?;
                len_str(&mut hasher, target.to_string_lossy().as_bytes());
            }
            b'f' => {
                let bytes = fs::read(&abs).map_err(|source| io_err(&abs, source))?;
                len_str(&mut hasher, &bytes);
            }
            _ => {}
        }
    }
    Ok(format!("{IDENTITY_TREE}{}", hasher.finalize().to_hex()))
}

/// Recursively collect every entry under `root` as a normalized `/`-separated
/// relative path string, recursing only into real directories (symlink targets
/// are never followed). The root itself is not included.
fn collect_relative_entries(
    root: &Path,
    rel: &Path,
    out: &mut Vec<String>,
) -> Result<(), VolumeError> {
    let abs = if rel.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let entries = fs::read_dir(&abs).map_err(|source| io_err(&abs, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| io_err(&abs, source))?;
        let name = entry.file_name();
        let child_rel = if rel.as_os_str().is_empty() {
            PathBuf::from(&name)
        } else {
            rel.join(&name)
        };
        out.push(relative_string(&child_rel));
        let child_abs = root.join(&child_rel);
        let meta = fs::symlink_metadata(&child_abs).map_err(|source| io_err(&child_abs, source))?;
        if meta.file_type().is_dir() {
            collect_relative_entries(root, &child_rel, out)?;
        }
    }
    Ok(())
}

fn relative_string(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn len_str(h: &mut blake3::Hasher, bytes: &[u8]) {
    let len = bytes.len() as u64;
    h.update(&len.to_le_bytes());
    h.update(bytes);
}

/// Record a single shallow top-level `node_modules/<name>` entry as owned.
/// `name` is the bare entry name (no separators); `proj_path` is the
/// project-relative joined path just attached; `mode`/`identity` describe the
/// live result. Symlinks/junctions record a `relay:` identity from `read_link`;
/// directory views record a `tree-blake3-v1:` fingerprint. Missing/unreadable
/// results skip the entry rather than claim unverified ownership.
fn record_entry(name: &str, proj_path: &Path, mode: &str, owned: &mut Vec<ManagedEntry>) {
    let path = format!("node_modules/{name}");
    let identity = if mode == "relay" {
        // On Unix relays are symlinks; `read_link` is the exact target.
        fs::read_link(proj_path)
            .ok()
            .map(|t| format!("{IDENTITY_RELAY}{}", t.to_string_lossy()))
    } else {
        tree_fingerprint(proj_path).ok()
    };
    if let Some(identity) = identity {
        owned.push(ManagedEntry {
            path,
            mode: mode.to_string(),
            identity,
        });
    }
}

/// Attach a project to a graph volume via shallow top-level relays: for every
/// top-level entry in `<volume>/node_modules`, create `<project>/node_modules/<entry>`
/// as a symlink to `<volume>/node_modules/<entry>` (created or confirmed;
/// a wrong target is replaced). Gated by `#[cfg(unix)]` since it needs symlinks.
#[cfg(unix)]
pub fn attach_project(
    project_root: &Path,
    volume: &VolumeRef,
) -> Result<AttachOutcome, VolumeError> {
    use std::os::unix::fs::symlink;
    let vol_nm = volume.path.join("node_modules");
    let proj_nm = project_root.join("node_modules");
    fs::create_dir_all(&proj_nm).map_err(|source| io_err(&proj_nm, source))?;

    let mut stats = AttachStats::default();
    let mut owned = Vec::new();
    let entries = fs::read_dir(&vol_nm).map_err(|source| io_err(&vol_nm, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| io_err(&vol_nm, source))?;
        let name = entry.file_name();
        // Only symlink top-level entries; nested trees live inside the volume.
        let vol_target = entry.path();
        let proj_link = proj_nm.join(&name);
        // Idempotent: if it already points at the volume entry, leave it.
        if let Ok(existing) = fs::read_link(&proj_link) {
            if existing == vol_target {
                stats.relays_unchanged += 1;
                record_entry(&name.to_string_lossy(), &proj_link, "relay", &mut owned);
                continue;
            }
            let _ = fs::remove_file(&proj_link);
        } else if proj_link.exists() {
            if proj_link.is_dir() {
                fs::remove_dir_all(&proj_link).map_err(|source| io_err(&proj_link, source))?;
            } else {
                fs::remove_file(&proj_link).map_err(|source| io_err(&proj_link, source))?;
            }
        }
        symlink(&vol_target, &proj_link).map_err(|source| io_err(&proj_link, source))?;
        stats.relays_created += 1;
        record_entry(&name.to_string_lossy(), &proj_link, "relay", &mut owned);
    }
    Ok(AttachOutcome::new(stats, owned))
}

#[cfg(windows)]
pub fn attach_project(
    project_root: &Path,
    volume: &VolumeRef,
) -> Result<AttachOutcome, VolumeError> {
    // Correctness-first Windows view: hardlinks where possible, copies as a
    // cross-volume fallback. No junctions or privileged symlinks are needed.
    attach_project_local(project_root, volume)
}

#[cfg(all(not(unix), not(windows)))]
pub fn attach_project(
    _project_root: &Path,
    _volume: &VolumeRef,
) -> Result<AttachOutcome, VolumeError> {
    Err(VolumeError::Materialize(
        MaterializeError::SymlinksUnsupported,
    ))
}

/// Reconciliation outcome: how many stale entries were removed and which owned
/// paths were preserved because BPM could not prove it still owned them
/// (identity mismatch or unknown mode). Preserved entries are reported, not
/// silently deleted; they block neither the install nor future reconciliation.
#[derive(Debug, Default, Clone)]
pub struct ReconcileOutcome {
    /// Stale owned entries removed after exact identity verification.
    pub removed: usize,
    /// Owned paths preserved because the live state no longer matches the
    /// recorded identity or the mode is unknown. The caller should warn.
    pub preserved: Vec<String>,
}

/// Remove stale BPM-owned project-view entries that are no longer desired.
/// Uses the prior plan's `owned_entries` for exact identity preflight: a relay
/// entry is removed only when the live symlink still points at the recorded
/// target, and a local/reflink entry is removed only when the live tree
/// fingerprint still equals the recorded one. `new_desired` is the set of
/// project-relative paths the new attachment owns (skipped untouched).
///
/// A mismatched or unverifiable entry is **preserved and reported**, never
/// deleted on assumption. Empty `@scope` parent containers may be tidied only
/// with `remove_dir` (non-recursive); a non-empty scope is left in place.
pub fn reconcile_project_view(
    project_root: &Path,
    old_owned: &[ManagedEntry],
    new_desired: &std::collections::BTreeSet<String>,
) -> Result<ReconcileOutcome, VolumeError> {
    let mut outcome = ReconcileOutcome::default();
    // Deepest paths first so nested stale entries clear before their scope
    // parent is considered for non-recursive removal.
    let mut sorted_old: Vec<&ManagedEntry> = old_owned.iter().collect();
    sorted_old.sort_by(|a, b| b.path.cmp(&a.path));

    for entry in sorted_old {
        if new_desired.contains(&entry.path) {
            continue;
        }
        // Reject any owned path that is not a proven project-relative
        // `node_modules/...` entry before joining — never trust persisted paths.
        if !ManagedEntry::path_is_valid(&entry.path) {
            outcome.preserved.push(entry.path.clone());
            continue;
        }
        let full_path = project_root.join(&entry.path);
        if !full_path.exists() {
            continue; // already removed by the user or a prior run
        }
        let meta = fs::symlink_metadata(&full_path).map_err(|source| io_err(&full_path, source))?;

        let safe_to_remove = match entry.mode.as_str() {
            "relay" | "direct" => {
                entry
                    .identity
                    .strip_prefix(IDENTITY_RELAY)
                    .is_some_and(|recorded_target| {
                        meta.file_type().is_symlink()
                            && fs::read_link(&full_path)
                                .map(|target| target.to_string_lossy() == recorded_target)
                                .unwrap_or(false)
                    })
            }
            "local" | "reflink" => {
                meta.is_dir()
                    && entry.identity.starts_with(IDENTITY_TREE)
                    && tree_fingerprint(&full_path)
                        .map(|live| live == entry.identity)
                        .unwrap_or(false)
            }
            _ => false,
        };

        if !safe_to_remove {
            outcome.preserved.push(entry.path.clone());
            continue;
        }

        if meta.file_type().is_symlink() || meta.is_file() {
            fs::remove_file(&full_path).map_err(|source| io_err(&full_path, source))?;
        } else if meta.is_dir() {
            fs::remove_dir_all(&full_path).map_err(|source| io_err(&full_path, source))?;
        }
        outcome.removed += 1;

        // Tidy an empty `@scope` parent only with `remove_dir` (fails if
        // non-empty, which we ignore). Never recursively remove a scope parent.
        if let Some(parent) = full_path.parent() {
            if parent
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('@'))
            {
                let _ = fs::remove_dir(parent);
            }
        }
    }
    Ok(outcome)
}

/// Conservatively infer prior ownership for a pre-fix (version-2) plan whose
/// `owned_entries` were persisted empty. Becausedeletion safety depends on an
/// exact recorded identity, this helper claims a project entry only when it is
/// **provably** still a BPM attachment of the prior immutable graph volume:
///
/// - a project symlink is claimed as `relay` only when its `read_link` target
///   exactly equals the corresponding prior volume entry path;
/// - a project directory is claimed as `local` only when its deterministic tree
///   fingerprint equals the prior volume entry's fingerprint;
/// - missing, unreadable, or mismatched paths are skipped (never inferred from
///   the current lockfile alone).
///
/// An absent or unusable prior volume returns an empty set; that is not
/// permission to delete — subsequent reconciliation preserves unverified
/// entries. The supplied `prior_volume_path` is `<prior-store>/graphs/...`
/// (the volume root, whose `node_modules` holds the prior top-level entries).
pub fn infer_prior_ownership(project_root: &Path, prior_volume_path: &Path) -> Vec<ManagedEntry> {
    let vol_nm = prior_volume_path.join("node_modules");
    let proj_nm = project_root.join("node_modules");
    let Ok(entries) = fs::read_dir(&vol_nm) else {
        return Vec::new();
    };
    let mut inferred = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy().into_owned();
        let proj_path = proj_nm.join(&name);
        let Ok(meta) = fs::symlink_metadata(&proj_path) else {
            continue;
        };
        let ft = meta.file_type();
        if ft.is_symlink() {
            let Ok(target) = fs::read_link(&proj_path) else {
                continue;
            };
            // Volume entry path is the prior volume's node_modules/<name>.
            if target == vol_nm.join(&name) {
                inferred.push(ManagedEntry {
                    path: format!("node_modules/{name_str}"),
                    mode: "relay".to_string(),
                    identity: format!("{IDENTITY_RELAY}{}", target.to_string_lossy()),
                });
            }
        } else if ft.is_dir() {
            let (Ok(live), Ok(recorded)) = (
                tree_fingerprint(&proj_path),
                tree_fingerprint(&vol_nm.join(&name)),
            ) else {
                continue;
            };
            if live == recorded {
                inferred.push(ManagedEntry {
                    path: format!("node_modules/{name_str}"),
                    mode: "local".to_string(),
                    identity: live,
                });
            }
        }
    }
    inferred.sort_by(|a, b| a.path.cmp(&b.path));
    inferred.dedup_by(|a, b| a.path == b.path);
    inferred
}

/// Attach a project with a local hardlink view of the graph volume.
///
/// Unlike relay attachment, every package file gets a project-local directory
/// entry (hardlinked to the volume where possible, copied otherwise). This
/// costs O(files) metadata work but keeps realpaths inside the project, which
/// is required by tools such as Turbopack that reject dependency files outside
/// the project root. Relative `.bin` symlinks are preserved, so Node resolves
/// bin scripts relative to their package rather than the `.bin` directory.
#[cfg(unix)]
pub fn attach_project_local(
    project_root: &Path,
    volume: &VolumeRef,
) -> Result<AttachOutcome, VolumeError> {
    attach_project_local_with_backend(project_root, volume, MaterializeBackend::Hardlink)
}

/// Like [`attach_project_local`], but selects the per-package linking strategy
/// from `backend`. `MaterializeBackend::Reflink` copy-on-write clones each
/// package tree into the project (cheaper than a full copy, and isolated from
/// the store image on filesystems that support reflink); any other backend
/// hardlinks (the established local view). `.bin` stays relative symlinks.
#[cfg(unix)]
pub fn attach_project_local_with_backend(
    project_root: &Path,
    volume: &VolumeRef,
    backend: MaterializeBackend,
) -> Result<AttachOutcome, VolumeError> {
    let vol_nm = volume.path.join("node_modules");
    let proj_nm = project_root.join("node_modules");
    fs::create_dir_all(&proj_nm).map_err(|source| io_err(&proj_nm, source))?;

    let mut entries = fs::read_dir(&vol_nm)
        .map_err(|source| io_err(&vol_nm, source))?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());

    let mode = if matches!(backend, MaterializeBackend::Reflink) {
        "reflink"
    } else {
        "local"
    };
    let mut stats = AttachStats::default();
    let mut owned = Vec::new();
    for entry in entries {
        let source = entry.path();
        let name = entry.file_name();
        let target = proj_nm.join(&name);
        if matches!(backend, MaterializeBackend::Reflink) {
            reflink_tree(&source, &target).map_err(VolumeError::Materialize)?;
        } else {
            hardlink_tree(&source, &target).map_err(VolumeError::Materialize)?;
        }
        stats.relays_created += 1;
        record_entry(&name.to_string_lossy(), &target, mode, &mut owned);
    }
    Ok(AttachOutcome::new(stats, owned))
}

#[cfg(windows)]
pub fn attach_project_local(
    project_root: &Path,
    volume: &VolumeRef,
) -> Result<AttachOutcome, VolumeError> {
    let vol_nm = volume.path.join("node_modules");
    let proj_nm = project_root.join("node_modules");
    fs::create_dir_all(&proj_nm).map_err(|source| io_err(&proj_nm, source))?;
    let mut entries = fs::read_dir(&vol_nm)
        .map_err(|source| io_err(&vol_nm, source))?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    let mut stats = AttachStats::default();
    let mut owned = Vec::new();
    for entry in entries {
        let name = entry.file_name();
        let target = proj_nm.join(&name);
        // Use junction_tree: tries a directory junction for directories,
        // falling back to hardlink→copy. Files always use hardlink. Inspecting
        // the result records the actual mode/identity conservatively: a
        // junction is a reparse point (symlink-like), recorded as relay; a
        // hardlink/copy fallback is a real directory recorded as local.
        crate::materializer::junction_tree(&entry.path(), &target)
            .map_err(VolumeError::Materialize)?;
        stats.relays_created += 1;
        let mode = if fs::symlink_metadata(&target)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            "relay"
        } else {
            "local"
        };
        record_entry(&name.to_string_lossy(), &target, mode, &mut owned);
    }
    Ok(AttachOutcome::new(stats, owned))
}

/// Windows has no reflink syscall binding; the backend argument is accepted
/// for API symmetry but the local view always hardlinks (copy fallback).
#[cfg(windows)]
pub fn attach_project_local_with_backend(
    project_root: &Path,
    volume: &VolumeRef,
    _backend: MaterializeBackend,
) -> Result<AttachOutcome, VolumeError> {
    // The explicit local backend is the deterministic hardlink/copy fallback
    // on Windows. Keep it separate from `attach_project`, which may use a
    // junction when the platform permits it; callers selecting this backend
    // need stable `local` ownership identities for reconciliation and tests.
    let vol_nm = volume.path.join("node_modules");
    let proj_nm = project_root.join("node_modules");
    fs::create_dir_all(&proj_nm).map_err(|source| io_err(&proj_nm, source))?;
    let mut entries = fs::read_dir(&vol_nm)
        .map_err(|source| io_err(&vol_nm, source))?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    let mut stats = AttachStats::default();
    let mut owned = Vec::new();
    for entry in entries {
        let name = entry.file_name();
        let target = proj_nm.join(&name);
        crate::materializer::hardlink_tree(&entry.path(), &target)
            .map_err(VolumeError::Materialize)?;
        stats.relays_created += 1;
        record_entry(&name.to_string_lossy(), &target, "local", &mut owned);
    }
    Ok(AttachOutcome::new(stats, owned))
}

#[cfg(all(not(unix), not(windows)))]
pub fn attach_project_local_with_backend(
    _project_root: &Path,
    _volume: &VolumeRef,
    _backend: MaterializeBackend,
) -> Result<AttachOutcome, VolumeError> {
    Err(VolumeError::Materialize(
        MaterializeError::SymlinksUnsupported,
    ))
}

/// Whether a project's `node_modules` still correctly relays into a graph
/// volume. Every top-level entry in the volume's `node_modules` must have a
/// matching symlink under the project (`<project>/node_modules/<entry>` →
/// `<volume>/node_modules/<entry>`). A single missing or wrong relay invalidates
/// attachment, so deleting a package relay forces a re-attach on the next install.
#[cfg(unix)]
pub fn project_attached(project_root: &Path, volume_path: &Path) -> bool {
    let proj_nm = project_root.join("node_modules");
    let vol_nm = volume_path.join("node_modules");
    if !proj_nm.exists() || !vol_nm.exists() {
        return false;
    }
    let entries = match fs::read_dir(&vol_nm) {
        Ok(e) => e,
        Err(_) => return false,
    };
    let mut seen = 0usize;
    for entry in entries.flatten() {
        seen += 1;
        let vol_target = entry.path();
        let proj_link = proj_nm.join(entry.file_name());
        match fs::read_link(&proj_link) {
            Ok(t) if t == vol_target => {}
            Ok(_) => return false,
            Err(_) if proj_link.is_dir() || proj_link.is_file() => {
                // A local compatibility view is also valid. Its package files
                // are hardlinked/copied from the volume, while `.bin` entries
                // remain relative symlinks inside the project tree.
            }
            Err(_) => return false,
        }
    }
    seen > 0
}

#[cfg(windows)]
pub fn project_attached(project_root: &Path, volume_path: &Path) -> bool {
    let proj_nm = project_root.join("node_modules");
    let vol_nm = volume_path.join("node_modules");
    let Ok(entries) = fs::read_dir(&vol_nm) else {
        return false;
    };
    let mut found = false;
    for entry in entries.flatten() {
        found = true;
        let name = entry.file_name();
        let project_entry = proj_nm.join(name);
        if !project_entry.exists() {
            return false;
        }
    }
    found && proj_nm.exists()
}

#[cfg(all(not(unix), not(windows)))]
pub fn project_attached(_project_root: &Path, _volume_path: &Path) -> bool {
    false
}

fn io_err(path: &Path, source: std::io::Error) -> VolumeError {
    VolumeError::Io {
        path: path.display().to_string(),
        source,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, MetadataExt};
    use tempfile::tempdir;

    #[test]
    fn local_attachment_keeps_realpaths_inside_project_and_bins_relative() {
        let volume_root = tempdir().unwrap();
        let project = tempdir().unwrap();
        let volume = volume_root.path().join("node_modules");
        fs::create_dir_all(volume.join("foo")).unwrap();
        fs::create_dir_all(volume.join(".bin")).unwrap();
        fs::write(
            volume.join("foo/package.json"),
            br#"{"name":"foo","version":"1.0.0"}"#,
        )
        .unwrap();
        fs::write(volume.join("foo/cli.js"), b"#!/usr/bin/env node\n").unwrap();
        symlink("../foo/cli.js", volume.join(".bin/foo")).unwrap();

        let volume_ref = VolumeRef {
            path: volume_root.path().to_path_buf(),
            cached: false,
            stats: MaterializeStats::default(),
        };
        let stats = attach_project_local(project.path(), &volume_ref).unwrap();
        assert_eq!(stats.stats.relays_created, 2);

        let project_pkg = project.path().join("node_modules/foo");
        assert!(project_pkg.is_dir());
        assert!(!fs::symlink_metadata(&project_pkg)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::metadata(project_pkg.join("package.json"))
                .unwrap()
                .ino(),
            fs::metadata(volume.join("foo/package.json")).unwrap().ino()
        );
        assert_eq!(
            fs::read_link(project.path().join("node_modules/.bin/foo")).unwrap(),
            PathBuf::from("../foo/cli.js")
        );
        assert!(project_attached(project.path(), volume_root.path()));
    }

    /// `attach_project_local_with_backend(.., Reflink)` must produce CoW clones:
    /// a distinct inode per file (not a hardlink) whose writes never reach the
    /// shared graph volume. Skipped on filesystems without reflink support.
    #[test]
    fn reflink_attachment_isolates_writes_from_the_volume() {
        let dir = tempdir().unwrap();
        let project = tempdir().unwrap();
        let volume_root = dir.path().join("vol");
        let volume = volume_root.join("node_modules");
        fs::create_dir_all(volume.join("foo")).unwrap();
        let payload = b"original volume bytes";
        fs::write(volume.join("foo/file.txt"), payload).unwrap();

        let volume_ref = VolumeRef {
            path: volume_root.clone(),
            cached: false,
            stats: MaterializeStats::default(),
        };

        // Skip on filesystems without reflink support.
        let caps = crate::materializer::probe_fs_capabilities(&volume_root);
        if !caps.reflink {
            eprintln!("skipping: filesystem does not support reflink");
            return;
        }

        let stats = attach_project_local_with_backend(
            project.path(),
            &volume_ref,
            MaterializeBackend::Reflink,
        )
        .unwrap();
        assert_eq!(stats.stats.relays_created, 1);

        // A reflink is a distinct inode (new inode, shared data extents), not a
        // hardlink to the volume file. This also catches a regression where
        // reflink silently fell back to hardlink on a supporting filesystem.
        let project_file = project.path().join("node_modules/foo/file.txt");
        let volume_file = volume.join("foo/file.txt");
        assert_ne!(
            fs::metadata(&project_file).unwrap().ino(),
            fs::metadata(&volume_file).unwrap().ino(),
            "reflink must create a distinct inode, not a hardlink"
        );

        // CoW isolation: mutating the project clone must not touch the volume.
        fs::write(&project_file, b"mutated project bytes").unwrap();
        assert_eq!(
            fs::read(&volume_file).unwrap(),
            payload,
            "a project-view write reached the shared graph volume (CoW broken)"
        );
    }
}

// === Plan 011: project-view ownership tests ===

#[cfg(all(test, unix))]
mod ownership_tests {
    use super::*;
    use crate::materializer::MaterializeStats;
    use std::collections::BTreeSet;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    fn volume_ref(root: &Path) -> VolumeRef {
        VolumeRef {
            path: root.to_path_buf(),
            cached: false,
            stats: MaterializeStats::default(),
        }
    }

    fn make_volume(volume_root: &Path, pkgs: &[(&str, &str)]) {
        let vol = volume_root.join("node_modules");
        for (name, body) in pkgs {
            fs::create_dir_all(vol.join(name)).unwrap();
            fs::write(vol.join(name).join("package.json"), body).unwrap();
        }
    }

    fn read_owned_names(outcome: &AttachOutcome) -> Vec<String> {
        outcome.owned.iter().map(|e| e.path.clone()).collect()
    }

    #[test]
    fn tree_fingerprint_is_stable_and_sensitive() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("tree");
        fs::create_dir_all(root.join("pkg")).unwrap();
        fs::write(root.join("pkg/package.json"), b"v1").unwrap();

        let fp1 = tree_fingerprint(&root).unwrap();
        let fp2 = tree_fingerprint(&root).unwrap();
        assert_eq!(fp1, fp2, "fingerprint must be deterministic");
        assert!(
            fp1.starts_with(IDENTITY_TREE),
            "fingerprint must carry the versioned tree prefix"
        );

        fs::write(root.join("pkg/package.json"), b"v2").unwrap();
        let fp3 = tree_fingerprint(&root).unwrap();
        assert_ne!(fp1, fp3, "a content change must alter the fingerprint");
    }

    #[test]
    fn tree_fingerprint_does_not_follow_symlinks() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("tree");
        fs::create_dir_all(root.join("pkg")).unwrap();
        fs::write(root.join("pkg/real.js"), b"real").unwrap();
        symlink("../pkg/real.js", root.join("pkg/link.js")).unwrap();

        let fp = tree_fingerprint(&root).unwrap();
        // Mutating the symlink target (keeping the link itself) must change the
        // fingerprint, because the link target is recorded, not dereferenced.
        fs::remove_file(root.join("pkg/link.js")).unwrap();
        symlink("../pkg/other.js", root.join("pkg/link.js")).unwrap();
        let fp2 = tree_fingerprint(&root).unwrap();
        assert_ne!(fp, fp2, "symlink target change must alter the fingerprint");
    }

    #[test]
    fn relay_attachment_records_exact_owned_paths_and_identities() {
        let volume_root = tempdir().unwrap();
        let project = tempdir().unwrap();
        make_volume(
            volume_root.path(),
            &[("foo", r#"{"name":"foo","version":"1.0.0"}"#)],
        );
        let outcome = attach_project(project.path(), &volume_ref(volume_root.path())).unwrap();
        assert_eq!(outcome.stats.relays_created, 1);
        assert_eq!(
            read_owned_names(&outcome),
            vec!["node_modules/foo".to_string()]
        );
        let entry = &outcome.owned[0];
        assert_eq!(entry.mode, "relay");
        assert!(
            entry.identity.starts_with(IDENTITY_RELAY),
            "relay identity must carry the relay prefix"
        );
        let target = entry.identity.strip_prefix(IDENTITY_RELAY).unwrap();
        assert_eq!(
            Path::new(target),
            volume_root.path().join("node_modules").join("foo")
        );
    }

    #[test]
    fn local_attachment_records_tree_fingerprint_identities() {
        let volume_root = tempdir().unwrap();
        let project = tempdir().unwrap();
        make_volume(
            volume_root.path(),
            &[("foo", r#"{"name":"foo","version":"1.0.0"}"#)],
        );
        let outcome =
            attach_project_local(project.path(), &volume_ref(volume_root.path())).unwrap();
        assert_eq!(
            read_owned_names(&outcome),
            vec!["node_modules/foo".to_string()]
        );
        let entry = &outcome.owned[0];
        assert_eq!(entry.mode, "local");
        assert!(entry.identity.starts_with(IDENTITY_TREE));
        // The recorded identity must match the live project tree.
        let live = tree_fingerprint(&project.path().join("node_modules").join("foo")).unwrap();
        assert_eq!(entry.identity, live);
    }

    fn desired(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reconcile_removes_exact_relay_on_identity_match() {
        let project = tempdir().unwrap();
        let nm = project.path().join("node_modules");
        fs::create_dir_all(&nm).unwrap();
        // The relay target lives outside node_modules/gone so removing the
        // symlink does not disturb it.
        let link_target = project.path().join(".volume").join("gone");
        fs::create_dir_all(&link_target).unwrap();
        symlink(&link_target, nm.join("gone")).unwrap();
        let identity = format!("{IDENTITY_RELAY}{}", link_target.to_string_lossy());
        let old = vec![ManagedEntry {
            path: "node_modules/gone".into(),
            mode: "relay".into(),
            identity,
        }];
        let outcome = reconcile_project_view(project.path(), &old, &desired(&[])).unwrap();
        assert_eq!(outcome.removed, 1);
        assert!(outcome.preserved.is_empty());
        assert!(
            !nm.join("gone").exists(),
            "the matched relay must be removed"
        );
        assert!(link_target.exists(), "the relay target must be untouched");
    }

    #[test]
    fn reconcile_removes_local_on_fingerprint_match() {
        let project = tempdir().unwrap();
        let nm = project.path().join("node_modules").join("gone");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("package.json"), b"v1").unwrap();
        let identity = tree_fingerprint(&nm).unwrap();
        let old = vec![ManagedEntry {
            path: "node_modules/gone".into(),
            mode: "local".into(),
            identity,
        }];
        let outcome = reconcile_project_view(project.path(), &old, &desired(&[])).unwrap();
        assert_eq!(outcome.removed, 1);
        assert!(outcome.preserved.is_empty());
        assert!(!nm.exists());
    }

    #[test]
    fn reconcile_preserves_a_user_replaced_directory_on_mismatch() {
        let project = tempdir().unwrap();
        let nm = project.path().join("node_modules").join("kept");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("package.json"), b"original").unwrap();
        let recorded = tree_fingerprint(&nm).unwrap();
        // User replaces the directory contents after BPM recorded ownership.
        fs::write(nm.join("package.json"), b"replaced-by-user").unwrap();
        let old = vec![ManagedEntry {
            path: "node_modules/kept".into(),
            mode: "local".into(),
            identity: recorded,
        }];
        let outcome = reconcile_project_view(project.path(), &old, &desired(&[])).unwrap();
        assert_eq!(outcome.removed, 0, "a mismatched dir must not be deleted");
        assert_eq!(outcome.preserved, vec!["node_modules/kept".to_string()]);
        assert!(nm.exists(), "the replaced dir must survive");
    }

    #[test]
    fn reconcile_preserves_unknown_mode_and_invalid_path() {
        let project = tempdir().unwrap();
        let nm = project.path().join("node_modules").join("mystery");
        fs::create_dir_all(&nm).unwrap();
        let old = vec![
            ManagedEntry {
                path: "node_modules/mystery".into(),
                mode: "weird".into(),
                identity: "x".into(),
            },
            ManagedEntry {
                path: "../node_modules/bad".into(),
                mode: "relay".into(),
                identity: "relay:x".into(),
            },
        ];
        let outcome = reconcile_project_view(project.path(), &old, &desired(&[])).unwrap();
        assert_eq!(outcome.removed, 0);
        assert!(nm.exists(), "unknown-mode entry must survive");
        assert_eq!(outcome.preserved.len(), 2);
    }

    #[test]
    fn reconcile_tidies_empty_scope_parent_non_recursively() {
        let project = tempdir().unwrap();
        let pkg = project.path().join("node_modules/@scope/gone");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("f"), b"x").unwrap();
        let identity = tree_fingerprint(&pkg).unwrap();
        let old = vec![ManagedEntry {
            path: "node_modules/@scope/gone".into(),
            mode: "local".into(),
            identity,
        }];
        let outcome = reconcile_project_view(project.path(), &old, &desired(&[])).unwrap();
        assert_eq!(outcome.removed, 1);
        assert!(
            !project.path().join("node_modules/@scope").exists(),
            "empty scope parent should be removed"
        );
    }

    #[test]
    fn infer_prior_ownership_claims_exact_relay_and_matching_local() {
        let volume_root = tempdir().unwrap();
        let project = tempdir().unwrap();
        make_volume(
            volume_root.path(),
            &[
                ("relay", r#"{"name":"relay"}"#),
                ("local", r#"{"name":"local"}"#),
            ],
        );
        let vol_ref = volume_ref(volume_root.path());

        // Relay entry: project symlink -> volume entry exactly.
        let proj_nm = project.path().join("node_modules");
        fs::create_dir_all(&proj_nm).unwrap();
        symlink(
            volume_root.path().join("node_modules").join("relay"),
            proj_nm.join("relay"),
        )
        .unwrap();
        // Local entry: hardlinked view (byte-identical to the volume entry).
        let local_outcome = attach_project_local(project.path(), &vol_ref).unwrap();
        // attach_project_local also attached "relay" as a dir, which clobbered
        // the symlink above; re-create a clean project to isolate the two.
        let project2 = tempdir().unwrap();
        let proj_nm2 = project2.path().join("node_modules");
        fs::create_dir_all(&proj_nm2).unwrap();
        symlink(
            volume_root.path().join("node_modules").join("relay"),
            proj_nm2.join("relay"),
        )
        .unwrap();
        let _ = local_outcome;
        // Local dir identical to volume.
        let local_src = volume_root.path().join("node_modules").join("local");
        let local_dst = proj_nm2.join("local");
        crate::materializer::hardlink_tree(&local_src, &local_dst).unwrap();

        let inferred = infer_prior_ownership(project2.path(), volume_root.path());
        let paths: Vec<String> = inferred.iter().map(|e| e.path.clone()).collect();
        assert_eq!(
            paths,
            vec![
                "node_modules/local".to_string(),
                "node_modules/relay".to_string()
            ]
        );
        for e in &inferred {
            assert!(!e.identity.is_empty());
            if e.mode == "relay" {
                assert!(e.identity.starts_with(IDENTITY_RELAY));
            } else {
                assert!(e.identity.starts_with(IDENTITY_TREE));
            }
        }
    }

    #[test]
    fn infer_prior_ownership_skips_a_replaced_local_directory() {
        let volume_root = tempdir().unwrap();
        let project = tempdir().unwrap();
        make_volume(volume_root.path(), &[("foo", r#"{"name":"foo"}"#)]);
        let vol_ref = volume_ref(volume_root.path());
        // Attach a local view, then replace the project dir so it no longer
        // matches the prior volume entry fingerprint.
        let _ = attach_project_local(project.path(), &vol_ref).unwrap();
        fs::write(
            project
                .path()
                .join("node_modules")
                .join("foo")
                .join("extra.txt"),
            b"user-added",
        )
        .unwrap();
        let inferred = infer_prior_ownership(project.path(), volume_root.path());
        assert!(
            inferred.is_empty(),
            "a user-modified local dir must not be claimed for deletion"
        );
    }
}
