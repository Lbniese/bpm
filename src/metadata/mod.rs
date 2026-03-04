//! Rebuildable SQLite metadata index for the immutable store.
//!
//! The filesystem is authoritative. A missing or corrupted database must be
//! rebuildable from the filesystem without deleting valid store objects.
//! Database rows never authorize a path outside the store.

mod repository;
mod schema;

pub use repository::{
    GraphRecord, LeaseGuard, LeaseOptions, MetadataError, MetadataRepository, ObjectKey,
    ObjectKind, ObjectRecord, ProjectRegistration, RepairReport, Timestamp,
};
