//! Deterministic value contracts shared by resolver stages.
//!
//! All collections use ordered standard-library types. A package identity
//! includes its source and bound peer providers, so instances that cannot be
//! safely deduplicated never compare equal.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// The semantic reason a dependency edge exists.
///
/// Declaration order is the canonical processing order used by the resolver.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum DependencyKind {
    Prod,
    Dev,
    Optional,
    Peer,
    PeerOptional,
    Workspace,
}

/// Where package contents originate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "kebab-case", tag = "type")]
pub enum PackageSource {
    /// An npm-compatible registry base URL.
    Registry { registry: String },
    /// A configured workspace, represented by a normalized project-relative path.
    Workspace { relative_path: String },
}

/// The npm platform names used during package compatibility checks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetPlatform {
    pub os: String,
    pub cpu: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub libc: Option<String>,
}

/// Normalized package platform declarations.
///
/// Values are sorted and deduplicated at the metadata boundary before this
/// contract is constructed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlatformConstraints {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub os: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub cpu: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub libc: BTreeSet<String>,
}

/// Stable identity of a package that provides a peer dependency.
///
/// A provider deliberately excludes its own peer context, preventing a
/// recursively-sized identity while retaining all facts needed to distinguish
/// visible providers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderIdentity {
    pub name: String,
    pub version: String,
    pub source: PackageSource,
}

/// Peer name to the exact provider visible to a package instance.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct PeerContext(pub BTreeMap<String, ProviderIdentity>);

/// Full logical identity of a resolved package instance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageIdentity {
    pub name: String,
    pub version: String,
    pub source: PackageSource,
    #[serde(default, skip_serializing_if = "PeerContext::is_empty")]
    pub peer_context: PeerContext,
}

impl PeerContext {
    /// Whether this instance has no bound peer providers.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Installation metadata retained independently of dependency edges.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tarball: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bin: BTreeMap<String, String>,
    #[serde(default)]
    pub platform: PlatformConstraints,
    #[serde(default)]
    pub has_install_script: bool,
}

/// A directed dependency from one package instance to another.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DependencyEdge {
    pub kind: DependencyKind,
    pub name: String,
    /// The effective request after workspace and root-override processing.
    pub spec: String,
    pub target: PackageIdentity,
}

/// One immutable node in the logical resolution graph.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageInstance {
    pub identity: PackageIdentity,
    pub metadata: PackageMetadata,
    /// Ordered edges make traversal and serialization independent of discovery order.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub edges: BTreeSet<DependencyEdge>,
}

/// A non-fatal resolver outcome retained with the completed graph.
///
/// The stable code is the primary ordering key. Package and message break ties,
/// making a set of diagnostics independent of traversal and metadata response
/// order while preserving distinct explanations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResolutionDiagnostic {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    pub message: String,
}

impl ResolutionDiagnostic {
    /// Create a graph diagnostic with a stable machine-readable code.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            package: None,
            message: message.into(),
        }
    }

    /// Associate the diagnostic with a package name.
    pub fn with_package(mut self, package: impl Into<String>) -> Self {
        self.package = Some(package.into());
        self
    }
}

/// A complete immutable logical dependency graph.
///
/// Root edges identify the project's direct requests. Every resolved package is
/// identified by its full identity, including source and peer context. Ordered
/// collections make equality, traversal, and serialized bytes independent of
/// discovery order.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedGraph {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub root: BTreeSet<DependencyEdge>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub instances: BTreeSet<PackageInstance>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub diagnostics: BTreeSet<ResolutionDiagnostic>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> PackageSource {
        PackageSource::Registry {
            registry: "https://registry.npmjs.org/".into(),
        }
    }

    fn provider(version: &str) -> ProviderIdentity {
        ProviderIdentity {
            name: "react".into(),
            version: version.into(),
            source: registry(),
        }
    }

    fn identity(peer_context: PeerContext) -> PackageIdentity {
        PackageIdentity {
            name: "plugin".into(),
            version: "1.0.0".into(),
            source: registry(),
            peer_context,
        }
    }

    #[test]
    fn peer_bindings_are_part_of_canonical_identity() {
        let mut react_18 = BTreeMap::new();
        react_18.insert("react".into(), provider("18.3.0"));
        let mut react_19 = BTreeMap::new();
        react_19.insert("react".into(), provider("19.1.0"));

        let first = identity(PeerContext(react_18));
        let second = identity(PeerContext(react_19));

        assert_ne!(first, second);
        assert!(first < second);
    }

    #[test]
    fn edge_order_and_serialization_do_not_depend_on_insertion_order() {
        let target = identity(PeerContext::default());
        let prod = DependencyEdge {
            kind: DependencyKind::Prod,
            name: "alpha".into(),
            spec: "^1.0.0".into(),
            target: target.clone(),
        };
        let optional = DependencyEdge {
            kind: DependencyKind::Optional,
            name: "zeta".into(),
            spec: "2".into(),
            target: target.clone(),
        };

        let left = PackageInstance {
            identity: target.clone(),
            metadata: PackageMetadata::default(),
            edges: [optional.clone(), prod.clone()].into_iter().collect(),
        };
        let right = PackageInstance {
            identity: target,
            metadata: PackageMetadata::default(),
            edges: [prod, optional].into_iter().collect(),
        };

        assert_eq!(left, right);
        assert_eq!(
            serde_json::to_vec(&left).unwrap(),
            serde_json::to_vec(&right).unwrap()
        );
    }

    #[test]
    fn source_is_part_of_canonical_identity() {
        let registry_identity = identity(PeerContext::default());
        let workspace_identity = PackageIdentity {
            source: PackageSource::Workspace {
                relative_path: "packages/plugin".into(),
            },
            ..registry_identity.clone()
        };

        assert_ne!(registry_identity, workspace_identity);
    }

    fn edge(name: &str, target: PackageIdentity) -> DependencyEdge {
        DependencyEdge {
            kind: DependencyKind::Prod,
            name: name.into(),
            spec: "1".into(),
            target,
        }
    }

    fn instance(identity: PackageIdentity) -> PackageInstance {
        PackageInstance {
            identity,
            metadata: PackageMetadata::default(),
            edges: BTreeSet::new(),
        }
    }

    #[test]
    fn resolved_graph_is_constructible() {
        let package = identity(PeerContext::default());
        let graph = ResolvedGraph {
            root: [edge("plugin", package.clone())].into_iter().collect(),
            instances: [instance(package)].into_iter().collect(),
            diagnostics: [
                ResolutionDiagnostic::new("OPTIONAL_SKIPPED", "not supported")
                    .with_package("plugin"),
            ]
            .into_iter()
            .collect(),
        };

        assert_eq!(graph.root.len(), 1);
        assert_eq!(graph.instances.len(), 1);
        assert_eq!(graph.diagnostics.len(), 1);
    }

    #[test]
    fn graph_order_and_serialization_do_not_depend_on_insertion_order() {
        let alpha = PackageIdentity {
            name: "alpha".into(),
            ..identity(PeerContext::default())
        };
        let zeta = PackageIdentity {
            name: "zeta".into(),
            ..identity(PeerContext::default())
        };
        let alpha_edge = edge("alpha", alpha.clone());
        let zeta_edge = edge("zeta", zeta.clone());
        let alpha_diagnostic = ResolutionDiagnostic::new("A_INFO", "alpha note");
        let zeta_diagnostic = ResolutionDiagnostic::new("Z_INFO", "zeta note");

        let left = ResolvedGraph {
            root: [zeta_edge.clone(), alpha_edge.clone()]
                .into_iter()
                .collect(),
            instances: [instance(zeta.clone()), instance(alpha.clone())]
                .into_iter()
                .collect(),
            diagnostics: [zeta_diagnostic.clone(), alpha_diagnostic.clone()]
                .into_iter()
                .collect(),
        };
        let right = ResolvedGraph {
            root: [alpha_edge, zeta_edge].into_iter().collect(),
            instances: [instance(alpha), instance(zeta)].into_iter().collect(),
            diagnostics: [alpha_diagnostic, zeta_diagnostic].into_iter().collect(),
        };

        assert_eq!(left, right);
        assert_eq!(left.root.iter().next().unwrap().name, "alpha");
        assert_eq!(left.instances.iter().next().unwrap().identity.name, "alpha");
        assert_eq!(left.diagnostics.iter().next().unwrap().code, "A_INFO");
        assert_eq!(
            serde_json::to_vec(&left).unwrap(),
            serde_json::to_vec(&right).unwrap()
        );
        assert_eq!(
            serde_json::from_slice::<ResolvedGraph>(&serde_json::to_vec(&left).unwrap()).unwrap(),
            left
        );
    }
}
