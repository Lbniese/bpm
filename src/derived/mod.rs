//! Lifecycle-derived image cache facade.
//!
//! Derived artifacts are immutable filesystem images produced from package
//! lifecycle scripts. The cache key is a canonical hash of all build-visible
//! inputs, and the store accepts a hit only after validating filesystem state;
//! metadata remains a repairable index rather than the source of truth.

mod key;
mod store;

pub use key::{derived_key, DerivedInputs, DerivedKey, RuntimeIdentity, TargetDescriptor};
pub use store::{
    DerivedError, DerivedMetadata, DerivedRecord, DerivedRef, DerivedStore, EnsureDerived,
    EnsureOptions, NullDerivedMetadata, SandboxFailure,
};
