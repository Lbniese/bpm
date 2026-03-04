//! Deterministic peer-dependency binding.
//!
//! Strict mode is the default: every visible provider must satisfy its declared
//! range, required missing peers are returned as actionable requests, and a
//! package's exact provider bindings become its [`PeerContext`]. The explicit
//! legacy mode ignores all peer edges and emits one stable graph diagnostic.

use std::collections::{BTreeMap, BTreeSet};

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::registry::VersionMetadata;

use super::model::{PeerContext, ProviderIdentity, ResolutionDiagnostic};

/// Peer resolution policy. Strict behavior is deliberately the safe default.
#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(rename_all = "kebab-case")]
pub enum PeerMode {
    #[default]
    Strict,
    LegacyIgnore,
}

/// One provider visible from a peer consumer's parent context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleProvider {
    pub identity: ProviderIdentity,
    /// Canonical project-relative placement used in conflict diagnostics.
    pub path: String,
    /// The earlier requester that selected this provider, when known.
    pub competing_requester: Option<String>,
}

/// Canonically ordered providers and the dependency chain of their consumer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VisibleProviders {
    consumer_chain: Vec<String>,
    providers: BTreeMap<String, VisibleProvider>,
}

impl VisibleProviders {
    /// Construct a visibility snapshot. Provider names are sorted by the map,
    /// while the chain retains root-to-consumer order for actionable errors.
    pub fn new<C, S, I, K>(consumer_chain: C, providers: I) -> Self
    where
        C: IntoIterator<Item = S>,
        S: Into<String>,
        I: IntoIterator<Item = (K, VisibleProvider)>,
        K: Into<String>,
    {
        Self {
            consumer_chain: consumer_chain.into_iter().map(Into::into).collect(),
            providers: providers
                .into_iter()
                .map(|(name, provider)| (name.into(), provider))
                .collect(),
        }
    }

    /// Return the nearest already-selected provider for `name`.
    pub fn get(&self, name: &str) -> Option<&VisibleProvider> {
        self.providers.get(name)
    }

    /// Root-to-consumer chain retained for diagnostics and enqueue placement.
    pub fn consumer_chain(&self) -> &[String] {
        &self.consumer_chain
    }
}

/// A required absent peer that traversal must enqueue at the consumer's parent.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerRequest {
    pub consumer: String,
    pub peer: String,
    pub range: String,
    pub consumer_chain: Vec<String>,
}

/// Detailed provider mismatch kept behind a box so the result error remains
/// inexpensive while retaining every actionable conflict field.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
#[error(
    "peer {peer}@{range} for {consumer}@{consumer_version} is not satisfied by {provider_name}@{provider_version} at {provider_path} (chain: {consumer_chain:?}, competing requester: {competing_requester:?})"
)]
pub struct UnsatisfiedPeer {
    pub consumer: String,
    pub consumer_version: String,
    pub peer: String,
    pub range: String,
    pub provider_name: String,
    pub provider_version: String,
    pub provider_path: String,
    pub competing_requester: Option<String>,
    pub consumer_chain: Vec<String>,
}

/// Strict peer binding failure with stable, complete conflict context.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PeerConflict {
    #[error(
        "required peer {peer}@{range} is missing for {consumer}@{consumer_version} (chain: {consumer_chain:?})"
    )]
    MissingRequired {
        consumer: String,
        consumer_version: String,
        peer: String,
        range: String,
        consumer_chain: Vec<String>,
    },
    #[error(
        "invalid peer range {peer}@{range} declared by {consumer}@{consumer_version}: {reason}"
    )]
    InvalidRange {
        consumer: String,
        consumer_version: String,
        peer: String,
        range: String,
        reason: String,
    },
    #[error("{details}")]
    Unsatisfied { details: Box<UnsatisfiedPeer> },
}

impl PeerConflict {
    /// Convert a strict failure to the stable graph diagnostic representation.
    pub fn diagnostic(&self) -> ResolutionDiagnostic {
        let (code, package) = match self {
            Self::MissingRequired { consumer, .. } => ("PEER_MISSING", consumer),
            Self::InvalidRange { consumer, .. } => ("PEER_INVALID_RANGE", consumer),
            Self::Unsatisfied { details } => ("PEER_CONFLICT", &details.consumer),
        };
        ResolutionDiagnostic::new(code, self.to_string()).with_package(package.clone())
    }

    /// Return the deterministic parent-context request for an absent required
    /// peer. Other conflicts cannot be repaired by auto-installation.
    pub fn required_request(&self) -> Option<PeerRequest> {
        match self {
            Self::MissingRequired {
                consumer,
                peer,
                range,
                consumer_chain,
                ..
            } => Some(PeerRequest {
                consumer: consumer.clone(),
                peer: peer.clone(),
                range: range.clone(),
                consumer_chain: consumer_chain.clone(),
            }),
            Self::InvalidRange { .. } | Self::Unsatisfied { .. } => None,
        }
    }
}

/// Bind every declared peer to its exact visible provider.
///
/// Missing optional peers are omitted. A present optional peer is validated in
/// exactly the same way as a required peer. In explicit legacy mode all peer
/// declarations are ignored and the returned context is empty.
pub fn bind_peer_context(
    candidate: &VersionMetadata,
    visible: &VisibleProviders,
    mode: PeerMode,
) -> Result<PeerContext, PeerConflict> {
    if mode == PeerMode::LegacyIgnore {
        return Ok(PeerContext::default());
    }

    let mut context = BTreeMap::new();
    for (peer, range) in &candidate.peer_dependencies {
        let optional = candidate
            .peer_dependencies_meta
            .get(peer)
            .is_some_and(|metadata| metadata.optional);
        let Some(provider) = visible.get(peer) else {
            if optional {
                continue;
            }
            return Err(PeerConflict::MissingRequired {
                consumer: candidate.name.clone(),
                consumer_version: candidate.version.to_string(),
                peer: peer.clone(),
                range: range.clone(),
                consumer_chain: visible.consumer_chain.clone(),
            });
        };

        let requirement = VersionReq::parse(range).map_err(|error| PeerConflict::InvalidRange {
            consumer: candidate.name.clone(),
            consumer_version: candidate.version.to_string(),
            peer: peer.clone(),
            range: range.clone(),
            reason: error.to_string(),
        })?;
        let provider_version = Version::parse(&provider.identity.version).ok();
        if provider_version
            .as_ref()
            .is_none_or(|version| !requirement.matches(version))
        {
            return Err(PeerConflict::Unsatisfied {
                details: Box::new(UnsatisfiedPeer {
                    consumer: candidate.name.clone(),
                    consumer_version: candidate.version.to_string(),
                    peer: peer.clone(),
                    range: range.clone(),
                    provider_name: provider.identity.name.clone(),
                    provider_version: provider.identity.version.clone(),
                    provider_path: provider.path.clone(),
                    competing_requester: provider.competing_requester.clone(),
                    consumer_chain: visible.consumer_chain.clone(),
                }),
            });
        }
        context.insert(peer.clone(), provider.identity.clone());
    }
    Ok(PeerContext(context))
}

/// Stable warning emitted when explicit legacy behavior discards peer edges.
pub fn peer_mode_diagnostics(
    candidate: &VersionMetadata,
    mode: PeerMode,
) -> BTreeSet<ResolutionDiagnostic> {
    if mode != PeerMode::LegacyIgnore || candidate.peer_dependencies.is_empty() {
        return BTreeSet::new();
    }
    [ResolutionDiagnostic::new(
        "LEGACY_PEER_DEPS",
        format!(
            "ignored {} peer dependency declaration(s) for {}@{} because legacy peer mode was explicitly selected",
            candidate.peer_dependencies.len(), candidate.name, candidate.version
        ),
    )
    .with_package(candidate.name.clone())]
    .into_iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::super::model::PackageSource;
    use super::*;
    use crate::registry::{Dist, PeerMeta};

    fn candidate(range: &str, optional: bool) -> VersionMetadata {
        VersionMetadata {
            name: "plugin".into(),
            version: Version::new(1, 0, 0),
            deprecated: None,
            dependencies: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::from([("react".into(), range.into())]),
            peer_dependencies_meta: optional
                .then(|| ("react".into(), PeerMeta { optional: true }))
                .into_iter()
                .collect(),
            bin: BTreeMap::new(),
            dist: Dist::default(),
            engines: BTreeMap::new(),
            os: Vec::new(),
            cpu: Vec::new(),
            libc: Vec::new(),
            has_install_script: false,
            has_shrinkwrap: false,
        }
    }

    fn provider(version: &str) -> VisibleProvider {
        VisibleProvider {
            identity: ProviderIdentity {
                name: "react".into(),
                version: version.into(),
                source: PackageSource::Registry {
                    registry: "https://registry.npmjs.org/".into(),
                },
            },
            path: "node_modules/react".into(),
            competing_requester: Some("app".into()),
        }
    }

    fn visible(version: &str) -> VisibleProviders {
        VisibleProviders::new(
            ["app", "plugin"],
            BTreeMap::from([("react", provider(version))]),
        )
    }

    #[test]
    fn satisfying_provider_is_bound_into_identity_context() {
        let context = bind_peer_context(
            &candidate("^18.0.0", false),
            &visible("18.3.0"),
            PeerMode::Strict,
        )
        .unwrap();
        assert_eq!(context.0["react"].version, "18.3.0");
    }

    #[test]
    fn missing_required_peer_produces_parent_request_and_stable_diagnostic() {
        let empty = VisibleProviders::new(
            ["app", "plugin"],
            BTreeMap::<String, VisibleProvider>::new(),
        );
        let conflict =
            bind_peer_context(&candidate("^18", false), &empty, PeerMode::Strict).unwrap_err();
        assert_eq!(conflict.required_request().unwrap().peer, "react");
        assert_eq!(conflict.diagnostic().code, "PEER_MISSING");
    }

    #[test]
    fn missing_optional_is_ignored_but_present_optional_must_satisfy() {
        let empty = VisibleProviders::new(
            ["app", "plugin"],
            BTreeMap::<String, VisibleProvider>::new(),
        );
        assert!(
            bind_peer_context(&candidate("^18", true), &empty, PeerMode::Strict)
                .unwrap()
                .0
                .is_empty()
        );
        assert!(matches!(
            bind_peer_context(
                &candidate("^18", true),
                &visible("17.0.2"),
                PeerMode::Strict
            ),
            Err(PeerConflict::Unsatisfied { .. })
        ));
    }

    #[test]
    fn conflict_retains_chain_provider_path_and_competing_requester() {
        let conflict = bind_peer_context(
            &candidate("^18", false),
            &visible("17.0.2"),
            PeerMode::Strict,
        )
        .unwrap_err();
        match conflict {
            PeerConflict::Unsatisfied { details } => {
                assert_eq!(details.provider_path, "node_modules/react");
                assert_eq!(details.competing_requester.as_deref(), Some("app"));
                assert_eq!(details.consumer_chain, ["app", "plugin"]);
            }
            other => panic!("unexpected conflict: {other}"),
        }
    }

    #[test]
    fn provider_binding_changes_peer_context_identity() {
        let first = bind_peer_context(
            &candidate("^18", false),
            &visible("18.3.0"),
            PeerMode::Strict,
        )
        .unwrap();
        let second = bind_peer_context(
            &candidate("^18", false),
            &visible("18.4.0"),
            PeerMode::Strict,
        )
        .unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn legacy_mode_is_explicit_empty_and_warns_stably() {
        let candidate = candidate("^18", false);
        let empty = VisibleProviders::new(
            ["app", "plugin"],
            BTreeMap::<String, VisibleProvider>::new(),
        );
        assert_eq!(PeerMode::default(), PeerMode::Strict);
        assert!(
            bind_peer_context(&candidate, &empty, PeerMode::LegacyIgnore)
                .unwrap()
                .0
                .is_empty()
        );
        let diagnostics = peer_mode_diagnostics(&candidate, PeerMode::LegacyIgnore);
        assert_eq!(diagnostics.iter().next().unwrap().code, "LEGACY_PEER_DEPS");
    }
}
