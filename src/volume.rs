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

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::graph::graph_id;
use crate::integrity::ArtifactId;
use crate::lockfile::Lockfile;
use crate::materializer::{materialize, MaterializeError, MaterializeStats};
use crate::metrics::Metrics;
use crate::store::ArtifactStore;

/// Marker file written at `<volume>/metadata.json` once the volume is complete.
/// Its presence with the right graph id is the reuse signal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct VolumeMeta {
    graph_id_hex: String,
    packages_materialized: usize,
    bins_linked: usize,
}

const META_FILE: &str = "metadata.json";

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
    let gid = graph_id(lockfile);
    let graph_hex = gid.to_hex();
    let volume_dir = store.graph_volume_path(&graph_hex);

    // Reuse: a complete marker means another process/project already built this
    // exact graph volume; attaching to it is the fast path.
    if let Ok(meta_bytes) = fs::read(volume_dir.join(META_FILE)) {
        if let Ok(meta) = serde_json::from_slice::<VolumeMeta>(&meta_bytes) {
            if meta.graph_id_hex == graph_hex {
                metrics.record("graph_volume_hit", std::time::Duration::ZERO);
                return Ok(VolumeRef {
                    path: volume_dir,
                    cached: true,
                    stats: MaterializeStats::default(),
                });
            }
        }
    }

    // Build: materialize the full node_modules projection inside the volume.
    fs::create_dir_all(volume_dir.join("node_modules"))
        .map_err(|source| io_err(&volume_dir, source))?;
    let resolved: Vec<(_, ArtifactId)> = artifact_ids
        .iter()
        .zip(lockfile.packages.iter())
        .filter_map(|(maybe_id, pkg)| maybe_id.map(|id| (pkg, id)))
        .collect();
    let stats = materialize(volume_dir.as_path(), store, &resolved)?;

    let meta = VolumeMeta {
        graph_id_hex: graph_hex,
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

#[cfg(not(unix))]
pub fn attach_project(
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
            _ => return false,
        }
    }
    seen > 0
}

#[cfg(not(unix))]
pub fn project_attached(_project_root: &Path, _volume_path: &Path) -> bool {
    false
}

fn io_err(path: &Path, source: std::io::Error) -> VolumeError {
    VolumeError::Io {
        path: path.display().to_string(),
        source,
    }
}
