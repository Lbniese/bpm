//! I/O abstraction for the placement core.
//!
//! [`PackumentSource`] is the single method a resolver needs from a registry
//! client: given a `PackageSpec`, return the full `Packument`.  The blocking
//! adapter wraps `RegistryClient`; the async adapter wraps `AsyncRegistryClient`
//! and drives async fetches to completion.
//!
//! Placement itself never touches a registry client directly — it calls
//! `PackumentSource` methods.  This makes the placement core I/O-agnostic.

use crate::http::HttpClient;
use crate::registry::{PackageSpec, Packument, RegistryClient, RegistryError};

/// Trait that provides packument data to the placement core.
///
/// The placement core is synchronous and deterministic: it reads packuments
/// from this source and never performs I/O itself.  A blocking implementation
/// delegates directly to `RegistryClient`; an async implementation must bridge
/// the sync↔async boundary (e.g. by pre-filling a cache or by driving async
/// fetches to completion in a way that does not re-enter a tokio runtime).
pub(crate) trait PackumentSource {
    /// Fetch the full packument for a given package+range spec.
    fn packument_for(&self, spec: &PackageSpec) -> Result<Packument, RegistryError>;

    /// Return the registry base URL for a package name.
    fn registry_for_package(&self, name: &str) -> &str;

    /// Best-effort hint that a packument will be needed soon.
    fn prefetch_packument(&self, name: &str, version_spec: Option<&str>);

    /// Optional HTTP client reference (needed for patch-resolution downloads).
    fn http(&self) -> Option<&HttpClient> {
        None
    }
}

// ── Blocking adapter ─────────────────────────────────────────────────────

/// Adapter that wraps `&RegistryClient` as a `PackumentSource`.
pub(crate) struct RegistrySource<'a> {
    pub(crate) client: &'a RegistryClient,
}

impl PackumentSource for RegistrySource<'_> {
    fn packument_for(&self, spec: &PackageSpec) -> Result<Packument, RegistryError> {
        self.client.packument_for(spec)
    }

    fn registry_for_package(&self, name: &str) -> &str {
        self.client.registry_for_package(name)
    }

    fn prefetch_packument(&self, name: &str, version_spec: Option<&str>) {
        self.client.prefetch_packument(name, version_spec);
    }

    fn http(&self) -> Option<&HttpClient> {
        Some(self.client.http())
    }
}
