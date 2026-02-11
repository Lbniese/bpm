//! BPM shared library.
//!
//! Submodules:
//! - `manifest`, `project`, `diagnostic`, `doctor`: project inspection.
//! - `integrity`, `download`, `archive`, `store`, `metrics`: the immutable
//!   artifact store.
//! - `lockfile`, `npm_lock`: package-lock v3 import and the canonical
//!   `bpm.lock`.
//! - `materializer`: project-local `node_modules` materialization for the
//!   frozen installer.
//! - `bench`: benchmark harness and tool runner.
//!
//! Resolver/registry-metadata/graph/lifecycle work is intentionally absent and
//! arrives in later milestones.

pub mod archive;
pub mod bench;
pub mod diagnostic;
pub mod doctor;
pub mod download;
pub mod graph;
pub mod integrity;
pub mod lifecycle;
pub mod lockfile;
pub mod manifest;
pub mod materializer;
pub mod metrics;
pub mod npm_lock;
pub mod project;
pub mod store;
pub mod volume;
pub mod workspace;

pub use diagnostic::{Diagnostic, Severity};
pub use doctor::{DoctorReport, ManifestSummary};
pub use integrity::{ArtifactId, Integrity, IntegrityError, Sha512Digest};
pub use lockfile::{Lockfile, LockfileError, PackageEntry, RootEntry};
pub use manifest::{ManifestError, PackageManifest};
pub use materializer::{materialize, MaterializeError, MaterializeStats};
pub use npm_lock::{ImportReport, NpmLockError};
pub use project::{find_project_root, find_repository_root, ProjectError};
pub use store::{ArtifactRef, ArtifactStore, ImageRef, StoreError};
