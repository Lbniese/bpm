//! BPM shared library.
//!
//! Submodules:
//! - `manifest`, `project`, `diagnostic`, `doctor`: project inspection.
//! - `config`, `http`: npm configuration and shared HTTP transport.
//! - `integrity`, `download`, `archive`, `store`, `metrics`: the immutable
//!   artifact store.
//! - `derived`: lifecycle output cache keyed by build-visible inputs.
//! - `lockfile`, `npm_lock`, `project_lock`: package-lock v3 import,
//!   selected-lock discovery, and the canonical `bpm.lock`.
//! - `materializer`: project-local `node_modules` materialization for the
//!   frozen installer.
//! - `bench`: benchmark harness and tool runner.
//! - `registry`: npm-registry packument resolution (name/spec -> tarball).
//!
//! Registry graph resolution is exposed through [`resolver`]; peer and
//! workspace edge cases remain incremental hardening work.

pub mod alternate_lock;
pub mod archive;
pub mod async_resolver;
pub mod bench;
pub mod config;
pub mod derived;
pub mod diagnostic;
pub mod doctor;
pub mod download;
pub mod gc;
pub mod graph;
pub mod http;
pub mod integrity;
pub mod lifecycle;
pub mod lockfile;
pub mod manifest;
pub mod manifest_edit;
pub mod materializer;
pub mod metadata;
pub mod metadata_cache;
pub mod metrics;
pub mod npm_lock;
pub mod package_image;
pub mod patch;
pub mod path_safety;
pub mod platform;
pub mod project;
pub mod project_lock;
pub mod registry;
pub mod remote_cache;
pub mod resolver;
pub mod store;
pub mod volume;
pub mod workspace;

pub use diagnostic::{Diagnostic, Severity};
pub use doctor::{DoctorReport, ManifestSummary};
pub use http::redact_url;
pub use integrity::{ArtifactId, Integrity, IntegrityError, Sha512Digest};
pub use lockfile::{Lockfile, LockfileError, PackageEntry, RootEntry};
pub use manifest::{ManifestError, PackageManifest};
pub use materializer::{materialize, MaterializeError, MaterializeStats};
pub use npm_lock::{ImportReport, NpmLockError};
pub use project::{find_project_root, find_repository_root, ProjectError};
pub use project_lock::{ProjectLock, ProjectLockError, ProjectLockKind};
pub use store::{ArtifactRef, ArtifactStore, ImageRef, StoreError};
