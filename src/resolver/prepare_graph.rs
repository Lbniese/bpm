//! Transient dependency closure used while preparing a Git-sourced package.
//!
//! The closure is deliberately separate from the consumer's lockfile: Git
//! preparation needs dev tooling, while the final installed graph must not.

use crate::graph::canonical_graph_bytes;
use crate::lockfile::Lockfile;
use crate::manifest::PackageManifest;
use crate::registry::RegistryClient;

use super::model::TargetPlatform;
use super::peer::PeerMode;
use super::{resolve_manifest_with_options_and_target, ResolveError};

/// A disposable graph for a package's build/preparation lifecycle.
#[derive(Debug, Clone)]
pub struct PreparedClosure {
    pub lockfile: Lockfile,
    pub digest: [u8; 32],
}

impl PreparedClosure {
    /// Construct the closure digest from the canonical graph encoding.
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

/// Resolve regular, optional, peer, and dev dependencies for a Git package.
///
/// No workspace is supplied: a Git package is prepared as a standalone root,
/// and the returned lockfile is never merged into the consumer graph.
pub fn build_prepare_closure(
    manifest: &PackageManifest,
    registry: &RegistryClient,
    generator: &str,
    target: TargetPlatform,
) -> Result<PreparedClosure, ResolveError> {
    let lockfile = resolve_manifest_with_options_and_target(
        manifest,
        registry,
        generator,
        None,
        PeerMode::Strict,
        target,
    )?;
    let digest = *blake3::hash(&canonical_graph_bytes(&lockfile)).as_bytes();
    Ok(PreparedClosure { lockfile, digest })
}
