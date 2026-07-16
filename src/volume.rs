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

use crate::graph::graph_id_with_prepared;
use crate::integrity::ArtifactId;
use crate::lockfile::Lockfile;
use crate::materializer::hardlink_tree;
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
    if stale {
        // Discard the stale projection so the rebuild contains no orphan
        // entries from the previous layout/graph. Removing hardlinked entries
        // only unlinks directory entries; the shared store images persist.
        let _ = fs::remove_dir_all(&volume_dir);
    }

    // Build: materialize the full node_modules projection inside the volume as
    // HARDLINKS (real directories sharing inodes with the immutable store
    // images) rather than symlinks into the store. A package's realpath then
    // lands inside the volume, where `node_modules/<self>` is reachable as a
    // sibling, so self-referential requires (e.g. `require('next/...')` issued
    // from within next's own code) resolve instead of escaping into the store
    // (which has no node_modules and breaks them).
    fs::create_dir_all(volume_dir.join("node_modules"))
        .map_err(|source| io_err(&volume_dir, source))?;
    let resolved: Vec<(_, ArtifactId)> = artifact_ids
        .iter()
        .zip(lockfile.packages.iter())
        .filter_map(|(maybe_id, pkg)| maybe_id.map(|id| (pkg, id)))
        .collect();
    let stats = materialize_with_backend(
        volume_dir.as_path(),
        store,
        &resolved,
        MaterializeBackend::Hardlink,
    )?;
    for package in lockfile.packages.iter().filter(|package| package.link) {
        let Some(source) = package.workspace_target.as_deref() else {
            continue;
        };
        let target = volume_dir.join(&package.path);
        overlay_prepared_image(Path::new(source), &target)?;
    }
    for (package_path, prepared_image) in prepared {
        let target = volume_dir.join(package_path);
        overlay_prepared_image(&prepared_image.image_path, &target)?;
    }

    let meta = VolumeMeta {
        graph_id_hex: graph_hex,
        layout_version: VOLUME_LAYOUT_VERSION,
        packages_materialized: stats.packages_materialized,
        bins_linked: stats.bins_linked,
    };
    let meta_bytes = serde_json::to_vec_pretty(&meta).unwrap_or_default();
    fs::write(volume_dir.join(META_FILE), meta_bytes)
        .map_err(|source| io_err(&volume_dir.join(META_FILE), source))?;

    metrics.record("graph_volume_build", std::time::Duration::ZERO);
    Ok(VolumeRef {
        path: volume_dir,
        cached: false,
        stats,
    })
}

/// Acquire an exclusive per-graph lock file at `<store>/locks/graph-<hex>.lock`.
fn acquire_graph_lock(store: &ArtifactStore, graph_hex: &str) -> Result<fs::File, VolumeError> {
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
        let target = fs::read_link(source).map_err(|error| io_err(source, error))?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| io_err(parent, error))?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, destination)
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

/// Attach a project to a graph volume via shallow top-level relays: for every
/// top-level entry in `<volume>/node_modules`, create `<project>/node_modules/<entry>`
/// as a symlink to `<volume>/node_modules/<entry>` (created or confirmed;
/// a wrong target is replaced). Gated by `#[cfg(unix)]` since it needs symlinks.
#[cfg(unix)]
pub fn attach_project(project_root: &Path, volume: &VolumeRef) -> Result<AttachStats, VolumeError> {
    use std::os::unix::fs::symlink;
    let vol_nm = volume.path.join("node_modules");
    let proj_nm = project_root.join("node_modules");
    fs::create_dir_all(&proj_nm).map_err(|source| io_err(&proj_nm, source))?;

    let mut stats = AttachStats::default();
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
    }
    Ok(stats)
}

#[cfg(windows)]
pub fn attach_project(project_root: &Path, volume: &VolumeRef) -> Result<AttachStats, VolumeError> {
    // Correctness-first Windows view: hardlinks where possible, copies as a
    // cross-volume fallback. No junctions or privileged symlinks are needed.
    attach_project_local(project_root, volume)
}

#[cfg(all(not(unix), not(windows)))]
pub fn attach_project(
    _project_root: &Path,
    _volume: &VolumeRef,
) -> Result<AttachStats, VolumeError> {
    Err(VolumeError::Materialize(
        MaterializeError::SymlinksUnsupported,
    ))
}

/// Remove stale BPM-owned top-level entries from the project's `node_modules`
/// that are not present in the new volume.  Only removes entries that are
/// symlinks (relay) or directories (local view) at the expected paths.
pub fn reconcile_project_view(
    project_root: &Path,
    volume: &VolumeRef,
    previous_top_level: &[String],
) -> Result<usize, VolumeError> {
    let proj_nm = project_root.join("node_modules");
    if !proj_nm.exists() {
        return Ok(0);
    }
    // Collect the new set of top-level entries from the volume.
    let vol_nm = volume.path.join("node_modules");
    let mut current: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if vol_nm.is_dir() {
        for entry in fs::read_dir(&vol_nm).map_err(|source| io_err(&vol_nm, source))? {
            let entry = entry.map_err(|source| io_err(&vol_nm, source))?;
            current.insert(entry.file_name().to_string_lossy().to_string());
        }
    }

    let mut removed = 0usize;
    for name in previous_top_level {
        if current.contains(name.as_str()) {
            continue; // still present in the new volume
        }
        let proj_entry = proj_nm.join(name);
        if !proj_entry.exists() {
            continue;
        }
        // Only remove entries that look like BPM-owned relay symlinks or
        // local directories.  Never remove arbitrary files.
        let meta = fs::symlink_metadata(&proj_entry)
            .map_err(|source| io_err(&proj_entry, source))?;
        if meta.file_type().is_symlink() {
            // Relay: only remove if the target is inside the old volume path.
            if let Ok(target) = fs::read_link(&proj_entry) {
                if target.starts_with(&volume.path) || target.starts_with(
                    volume.path.parent().unwrap_or(Path::new("")),
                ) {
                    fs::remove_file(&proj_entry)
                        .map_err(|source| io_err(&proj_entry, source))?;
                    removed += 1;
                }
            }
        } else if meta.is_dir() {
            // Local directory: assume BPM-owned (conservative, safe).
            fs::remove_dir_all(&proj_entry)
                .map_err(|source| io_err(&proj_entry, source))?;
            removed += 1;
        }
        // Regular files (not symlinks, not dirs) are left alone.
    }
    Ok(removed)
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
) -> Result<AttachStats, VolumeError> {
    let vol_nm = volume.path.join("node_modules");
    let proj_nm = project_root.join("node_modules");
    fs::create_dir_all(&proj_nm).map_err(|source| io_err(&proj_nm, source))?;

    let mut entries = fs::read_dir(&vol_nm)
        .map_err(|source| io_err(&vol_nm, source))?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());

    let mut stats = AttachStats::default();
    for entry in entries {
        let source = entry.path();
        let target = proj_nm.join(entry.file_name());
        hardlink_tree(&source, &target).map_err(VolumeError::Materialize)?;
        stats.relays_created += 1;
    }
    Ok(stats)
}

#[cfg(windows)]
pub fn attach_project_local(
    project_root: &Path,
    volume: &VolumeRef,
) -> Result<AttachStats, VolumeError> {
    let vol_nm = volume.path.join("node_modules");
    let proj_nm = project_root.join("node_modules");
    fs::create_dir_all(&proj_nm).map_err(|source| io_err(&proj_nm, source))?;
    let mut entries = fs::read_dir(&vol_nm)
        .map_err(|source| io_err(&vol_nm, source))?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    let mut stats = AttachStats::default();
    for entry in entries {
        let target = proj_nm.join(entry.file_name());
        bpm::materializer::hardlink_tree(&entry.path(), &target)
            .map_err(VolumeError::Materialize)?;
        stats.relays_created += 1;
    }
    Ok(stats)
}

#[cfg(all(not(unix), not(windows)))]
pub fn attach_project_local(
    _project_root: &Path,
    _volume: &VolumeRef,
) -> Result<AttachStats, VolumeError> {
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
        assert_eq!(stats.relays_created, 2);

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
}
