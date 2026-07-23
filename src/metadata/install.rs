//! Production integration between install/publication and the rebuildable
//! metadata index.
//!
//! An [`InstallSession`] records publication of every store object an install
//! reads, holds one renewable lease over them (so a concurrent `bpm gc` cannot
//! reclaim them mid-install), records the graph's complete inventory edges, and
//! atomically publishes the durable project registration + SQLite graph
//! reference after attachment succeeds. Dropping the session releases the
//! lease.
//!
//! The lease (durable SQLite rows, renewed by a heartbeat) — not an OS lock —
//! provides ongoing protection across the long install; the per-object advisory
//! locks shared with GC coordinate only the brief publication/lease-record and
//! delete windows.

use std::path::{Path, PathBuf};

use crate::integrity::ArtifactId;
use crate::metadata::{
    LeaseOptions, MetadataError, MetadataRepository, ObjectKey, ProjectRegistration, Timestamp,
};

/// One install's metadata session: leases, graph inventory, and durable
/// project ownership.
pub struct InstallSession {
    repository: MetadataRepository,
    store_root: PathBuf,
    lease: Option<crate::metadata::LeaseGuard>,
}

impl InstallSession {
    /// Open the rebuildable index for `store_root`. Cheap; the heavy work
    /// happens on the first lease/record call.
    pub fn open(store_root: &Path) -> Result<Self, MetadataError> {
        let repository = MetadataRepository::open(store_root)?;
        Ok(Self {
            repository,
            store_root: store_root.to_path_buf(),
            lease: None,
        })
    }

    /// Borrow the underlying repository for direct metadata calls.
    pub fn repository(&self) -> &MetadataRepository {
        &self.repository
    }

    /// The store root this session protects.
    pub fn store_root(&self) -> &Path {
        &self.store_root
    }

    /// Acquire one renewable lease over the artifacts (and their extracted
    /// images) plus prepared-derived objects an install is about to read,
    /// recording each object's publication first. Must be called after the
    /// objects are published to the store and before materialization/lifecycle
    /// reads them, so a concurrent collector cannot reclaim them.
    ///
    /// `artifacts` are the resolved registry artifact ids; each is assumed to
    /// have an extracted store image (the materializer extracts every fetched
    /// artifact). `derived` are prepared-image BLAKE3 hex ids.
    pub fn lease_objects(
        &mut self,
        artifacts: &[ArtifactId],
        derived: &[String],
    ) -> Result<(), MetadataError> {
        let now = Timestamp::now()?;
        let mut keys: Vec<ObjectKey> = Vec::new();
        for id in artifacts {
            let hex = id.to_hex();
            let artifact = ObjectKey::artifact(hex.clone())?;
            self.repository.record_published_object(&artifact)?;
            keys.push(artifact);
            // Every fetched artifact is extracted into a store image.
            let image = ObjectKey::image(hex)?;
            self.repository.record_published_object(&image)?;
            keys.push(image);
        }
        for derived_id in derived {
            let key = ObjectKey::derived(derived_id.clone())?;
            self.repository.record_published_object(&key)?;
            keys.push(key);
        }
        if !keys.is_empty() {
            self.repository.record_access(&keys, now)?;
            self.lease = Some(
                self.repository
                    .acquire_lease(&keys, LeaseOptions::default())?,
            );
        }
        Ok(())
    }

    /// Record a newly published graph and extend the install lease to cover
    /// it (and any derived objects in its inventory). When a complete
    /// inventory is supplied, the graph's artifact/derived edges are recorded
    /// (marking it `complete`); when `None` (a legacy/incomplete volume) the
    /// graph object is still recorded and leased, staying `complete=0`
    /// (protected). Call after the graph volume is durably published and
    /// before it is attached.
    pub fn record_graph(
        &mut self,
        graph_hex: &str,
        inventory: Option<&crate::volume::GraphInventory>,
    ) -> Result<(), MetadataError> {
        let graph_key = ObjectKey::graph(graph_hex)?;
        match inventory {
            Some(inventory) => {
                self.repository
                    .record_graph_with_inventory(graph_hex, inventory)?;
            }
            None => {
                // Legacy/incomplete volume: record the object only so it is
                // indexed; it stays complete=0 (protected, no guessed edges).
                self.repository.record_published_object(&graph_key)?;
            }
        }
        let mut extend_keys = vec![graph_key];
        if let Some(inventory) = inventory {
            for derived_id in &inventory.derived {
                extend_keys.push(ObjectKey::derived(derived_id.clone())?);
            }
        }
        if let Some(lease) = self.lease.as_mut() {
            lease.extend(&extend_keys)?;
        } else if !extend_keys.is_empty() {
            self.lease = Some(
                self.repository
                    .acquire_lease(&extend_keys, LeaseOptions::default())?,
            );
        }
        Ok(())
    }

    /// Verify the install lease is still held (heartbeat alive, token valid).
    pub fn check(&self) -> Result<(), MetadataError> {
        match &self.lease {
            Some(guard) => guard.check(),
            None => Ok(()),
        }
    }

    /// Publish durable project ownership after the graph has been attached and
    /// `.bpm-state` written: checks the lease is still valid, writes the
    /// durable registration file, and atomically replaces the SQLite project
    /// graph reference. A failure here must fail the install rather than leave
    /// an unprotected graph reported as installed.
    pub fn finalize_project(
        &self,
        project_root: &Path,
        graph_hex: &str,
    ) -> Result<(), MetadataError> {
        self.check()?;
        self.repository
            .write_durable_registration(project_root, graph_hex)?;
        self.repository.replace_project_graph_ref(
            &ProjectRegistration {
                root: project_root.to_path_buf(),
                graph_id: graph_hex.to_owned(),
            },
            Timestamp::now()?,
        )?;
        // Final confirmation that the lease survived registration.
        self.check()
    }

    /// Refresh ownership for a plan-cache-hit install (no fetch/materialization
    /// performed): read the graph's durable inventory, lease the objects the
    /// cached graph depends on, record access, and refresh the durable project
    /// registration + SQLite reference.
    pub fn refresh_cached_graph(
        &mut self,
        project_root: &Path,
        graph_hex: &str,
    ) -> Result<(), MetadataError> {
        let volume_path = self
            .store_root
            .join("graphs/blake3")
            .join(graph_hex.get(..2).unwrap_or(""))
            .join(graph_hex);
        let Some(inventory) = crate::volume::read_graph_inventory(&volume_path) else {
            // Legacy/incomplete graph volume: take the normal rebuild path
            // rather than falsely marking it complete. Fail so the caller
            // rebuilds instead of trusting an unprotected cache hit.
            return Err(MetadataError::MissingObject {
                kind: "graph inventory",
                id: graph_hex.to_owned(),
            });
        };
        let artifact_ids: Vec<ArtifactId> = inventory
            .artifacts
            .iter()
            .filter_map(|(hex, _)| parse_artifact_hex(hex))
            .collect();
        self.lease_objects(&artifact_ids, &inventory.derived)?;
        self.record_graph(graph_hex, Some(&inventory))?;
        self.finalize_project(project_root, graph_hex)
    }
}

fn parse_artifact_hex(hex: &str) -> Option<ArtifactId> {
    ArtifactId::from_hex(hex).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_objects_records_publication_and_acquire_lease() {
        let temp = tempfile::tempdir().unwrap();
        let store_root = temp.path();
        // Publish one artifact + its image by hand.
        let id = ArtifactId::from_bytes([7; 64]);
        let hex = id.to_hex();
        let artifact_path = store_root
            .join("artifacts/sha512")
            .join(&hex[..2])
            .join(format!("{hex}.tgz"));
        std::fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
        std::fs::write(&artifact_path, b"tarball").unwrap();
        let image_path = store_root.join("images/sha512").join(&hex[..2]).join(&hex);
        std::fs::create_dir_all(&image_path).unwrap();
        std::fs::write(image_path.join("package.json"), b"{}").unwrap();

        let mut session = InstallSession::open(store_root).unwrap();
        session.lease_objects(&[id], &[]).unwrap();
        assert!(session.lease.is_some());
        // The lease is active; check passes.
        session.check().unwrap();
        // Dropping releases the lease without error.
        drop(session);
    }
}
