//! Deterministic workspace matching for the native resolver.
//!
//! Configured workspaces are indexed by normalized project-relative path and
//! declared package name. Ordinary semver requests prefer a matching local
//! package and otherwise remain registry requests. The `workspace:` protocol
//! is a BPM extension: it always requires a matching local package and never
//! falls back to a registry.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use semver::Version;
use thiserror::Error;

use crate::manifest::{PackageManifest, WorkspaceSpec};
use crate::resolver::model::{
    DependencyEdge, DependencyKind, PackageIdentity, PackageSource, PeerContext,
};
use crate::workspace::WorkspaceLayout;

/// One validated local package available to resolver workspace matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedWorkspace {
    pub name: String,
    pub version: Version,
    pub relative_path: String,
    /// Parsed workspace package manifest, when the index was built from a
    /// project root. Synthetic test/layout indexes may omit it.
    pub manifest: Option<PackageManifest>,
}

/// A deterministic workspace index keyed by both package name and path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceIndex {
    by_name: BTreeMap<String, IndexedWorkspace>,
    by_path: BTreeMap<String, String>,
}

/// Result of considering a dependency request for local workspace binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceResolution {
    /// Bind the dependency to the configured local package.
    Link(DependencyEdge),
    /// Continue through normal registry resolution with the unchanged request.
    Registry { spec: String },
}

/// Invalid or unsatisfied workspace configuration/request.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WorkspaceError {
    #[error("workspace path `{path}` must be a normalized project-relative path")]
    InvalidPath { path: String },
    #[error("workspace path `{path}` is configured more than once")]
    DuplicatePath { path: String },
    #[error("workspace at `{path}` has no declared package name")]
    MissingName { path: String },
    #[error("workspace `{name}` at `{path}` has no declared version")]
    MissingVersion { name: String, path: String },
    #[error("workspace `{name}` at `{path}` has invalid version `{version}`: {reason}")]
    InvalidVersion {
        name: String,
        path: String,
        version: String,
        reason: String,
    },
    #[error(
        "workspace package name `{name}` is ambiguous between `{first_path}` and `{second_path}`"
    )]
    DuplicateName {
        name: String,
        first_path: String,
        second_path: String,
    },
    #[error("dependency `{name}` requires a local workspace, but none is configured")]
    MissingWorkspace { name: String },
    #[error("dependency `{name}` has unsupported workspace request `{spec}`: {reason}")]
    InvalidWorkspaceSpec {
        name: String,
        spec: String,
        reason: String,
    },
    #[error(
        "dependency `{name}` requires `{spec}`, but local workspace version `{version}` does not satisfy it; explicit workspace requests never fall back to the registry"
    )]
    VersionMismatch {
        name: String,
        spec: String,
        version: String,
    },
}

impl WorkspaceIndex {
    /// Validate and index a discovered workspace layout.
    pub fn from_layout(layout: &WorkspaceLayout) -> Result<Self, WorkspaceError> {
        Self::from_layout_with_manifest_loader(layout, |_| None)
    }

    /// Validate and index a discovered workspace layout, loading each
    /// workspace's full manifest from `project_root`. Native resolution uses
    /// this richer index so dependencies declared by local workspaces are
    /// traversed just like registry package dependencies.
    pub fn from_project_root(
        project_root: &Path,
        layout: &WorkspaceLayout,
    ) -> Result<Self, WorkspaceError> {
        Self::from_layout_with_manifest_loader(layout, |relative_path| {
            PackageManifest::from_path(&project_root.join(relative_path).join("package.json")).ok()
        })
    }

    fn from_layout_with_manifest_loader<F>(
        layout: &WorkspaceLayout,
        mut load_manifest: F,
    ) -> Result<Self, WorkspaceError>
    where
        F: FnMut(&str) -> Option<PackageManifest>,
    {
        let mut index = Self::default();
        let mut packages = layout
            .packages
            .iter()
            .map(|package| normalize_relative_path(&package.dir).map(|path| (path, package)))
            .collect::<Result<Vec<_>, _>>()?;
        packages.sort_by(|(left, _), (right, _)| left.cmp(right));

        for (relative_path, package) in packages {
            if index.by_path.contains_key(&relative_path) {
                return Err(WorkspaceError::DuplicatePath {
                    path: relative_path,
                });
            }

            let manifest = load_manifest(&relative_path);
            let name = manifest
                .as_ref()
                .and_then(|manifest| manifest.name.clone())
                .or_else(|| package.name.clone())
                .filter(|name| !name.is_empty())
                .ok_or_else(|| WorkspaceError::MissingName {
                    path: relative_path.clone(),
                })?;
            let version_text = manifest
                .as_ref()
                .and_then(|manifest| manifest.version.clone())
                .or_else(|| package.version.clone())
                .ok_or_else(|| WorkspaceError::MissingVersion {
                    name: name.clone(),
                    path: relative_path.clone(),
                })?;
            let version =
                Version::parse(&version_text).map_err(|error| WorkspaceError::InvalidVersion {
                    name: name.clone(),
                    path: relative_path.clone(),
                    version: version_text.clone(),
                    reason: error.to_string(),
                })?;

            if let Some(existing) = index.by_name.get(&name) {
                return Err(WorkspaceError::DuplicateName {
                    name,
                    first_path: existing.relative_path.clone(),
                    second_path: relative_path,
                });
            }

            index.by_path.insert(relative_path.clone(), name.clone());
            index.by_name.insert(
                name.clone(),
                IndexedWorkspace {
                    name,
                    version,
                    relative_path,
                    manifest,
                },
            );
        }

        Ok(index)
    }

    /// Return a validated workspace by package name.
    pub fn get(&self, name: &str) -> Option<&IndexedWorkspace> {
        self.by_name.get(name)
    }

    /// Resolve a request to a local link when workspace semantics permit it.
    ///
    /// Invalid or non-semver ordinary requests remain registry requests; the
    /// registry resolver owns tags and its wider npm range grammar.
    pub fn resolve(&self, name: &str, spec: &str) -> Result<WorkspaceResolution, WorkspaceError> {
        if let Some(workspace_spec) = WorkspaceSpec::parse(spec) {
            return self.resolve_explicit(name, spec, workspace_spec);
        }

        let Some(workspace) = self.by_name.get(name) else {
            return Ok(registry(spec));
        };
        let Ok(requirement) = crate::registry::VersionRange::parse(spec) else {
            return Ok(registry(spec));
        };
        if requirement.matches(&workspace.version) {
            Ok(WorkspaceResolution::Link(link_edge(workspace, spec)))
        } else {
            Ok(registry(spec))
        }
    }

    /// Deterministic top-level links for every configured workspace.
    pub fn top_level_links(&self) -> BTreeSet<DependencyEdge> {
        self.by_name
            .values()
            .map(|workspace| link_edge(workspace, &workspace.version.to_string()))
            .collect()
    }

    fn resolve_explicit(
        &self,
        name: &str,
        original_spec: &str,
        spec: WorkspaceSpec,
    ) -> Result<WorkspaceResolution, WorkspaceError> {
        let workspace = self
            .by_name
            .get(name)
            .ok_or_else(|| WorkspaceError::MissingWorkspace {
                name: name.to_owned(),
            })?;

        let effective_spec = match spec {
            WorkspaceSpec::Any => "*".to_owned(),
            WorkspaceSpec::Caret => format!("^{}", workspace.version),
            WorkspaceSpec::Tilde => format!("~{}", workspace.version),
            WorkspaceSpec::Range(range) => {
                if range.is_empty() {
                    return Err(invalid_workspace_spec(
                        name,
                        original_spec,
                        "expected `*`, `^`, `~`, or a semver range",
                    ));
                }
                let requirement =
                    crate::registry::VersionRange::parse(&range).map_err(|error| {
                        invalid_workspace_spec(name, original_spec, &error.to_string())
                    })?;
                if !requirement.matches(&workspace.version) {
                    return Err(WorkspaceError::VersionMismatch {
                        name: name.to_owned(),
                        spec: original_spec.to_owned(),
                        version: workspace.version.to_string(),
                    });
                }
                range
            }
        };

        Ok(WorkspaceResolution::Link(link_edge(
            workspace,
            &effective_spec,
        )))
    }
}

fn registry(spec: &str) -> WorkspaceResolution {
    WorkspaceResolution::Registry {
        spec: spec.to_owned(),
    }
}

fn link_edge(workspace: &IndexedWorkspace, effective_spec: &str) -> DependencyEdge {
    DependencyEdge {
        kind: DependencyKind::Workspace,
        name: workspace.name.clone(),
        spec: effective_spec.to_owned(),
        target: PackageIdentity {
            name: workspace.name.clone(),
            version: workspace.version.to_string(),
            source: PackageSource::Workspace {
                relative_path: workspace.relative_path.clone(),
            },
            peer_context: PeerContext(Default::default()),
        },
    }
}

fn invalid_workspace_spec(name: &str, spec: &str, reason: &str) -> WorkspaceError {
    WorkspaceError::InvalidWorkspaceSpec {
        name: name.to_owned(),
        spec: spec.to_owned(),
        reason: reason.to_owned(),
    }
}

fn normalize_relative_path(path: &str) -> Result<String, WorkspaceError> {
    let normalized = path.replace('\\', "/");
    let invalid = normalized.is_empty()
        || normalized.starts_with('/')
        || normalized.ends_with('/')
        || normalized
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
        || normalized
            .split('/')
            .next()
            .is_some_and(|component| component.ends_with(':'));
    if invalid {
        return Err(WorkspaceError::InvalidPath {
            path: path.to_owned(),
        });
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::WorkspacePackage;

    fn package(dir: &str, name: &str, version: &str) -> WorkspacePackage {
        WorkspacePackage {
            dir: dir.into(),
            name: Some(name.into()),
            version: Some(version.into()),
        }
    }

    fn index(packages: Vec<WorkspacePackage>) -> WorkspaceIndex {
        WorkspaceIndex::from_layout(&WorkspaceLayout {
            patterns: vec!["packages/*".into()],
            packages,
        })
        .unwrap()
    }

    #[test]
    fn ordinary_matching_range_links_and_mismatch_falls_back() {
        let index = index(vec![package("packages/a", "a", "1.2.3")]);

        assert!(matches!(
            index.resolve("a", "^1.0.0").unwrap(),
            WorkspaceResolution::Link(_)
        ));
        assert_eq!(
            index.resolve("a", "^2.0.0").unwrap(),
            WorkspaceResolution::Registry {
                spec: "^2.0.0".into()
            }
        );
        assert_eq!(
            index.resolve("missing", "latest").unwrap(),
            WorkspaceResolution::Registry {
                spec: "latest".into()
            }
        );
    }

    #[test]
    fn workspace_protocol_links_and_canonicalizes_publication_ranges() {
        let index = index(vec![package("packages/a", "a", "1.2.3")]);

        for (request, effective) in [
            ("workspace:*", "*"),
            ("workspace:^", "^1.2.3"),
            ("workspace:~", "~1.2.3"),
            ("workspace:>=1.2, <2", ">=1.2, <2"),
        ] {
            let WorkspaceResolution::Link(edge) = index.resolve("a", request).unwrap() else {
                panic!("explicit workspace request must link");
            };
            assert_eq!(edge.kind, DependencyKind::Workspace);
            assert_eq!(edge.spec, effective);
            assert_eq!(edge.target.version, "1.2.3");
            assert_eq!(
                edge.target.source,
                PackageSource::Workspace {
                    relative_path: "packages/a".into()
                }
            );
        }
    }

    #[test]
    fn explicit_workspace_requests_never_fall_back() {
        let index = index(vec![package("packages/a", "a", "1.2.3")]);

        assert!(matches!(
            index.resolve("missing", "workspace:*").unwrap_err(),
            WorkspaceError::MissingWorkspace { .. }
        ));
        assert!(matches!(
            index.resolve("a", "workspace:^2.0.0").unwrap_err(),
            WorkspaceError::VersionMismatch { .. }
        ));
        assert!(matches!(
            index.resolve("a", "workspace:file:../a").unwrap_err(),
            WorkspaceError::InvalidWorkspaceSpec { .. }
        ));
    }

    #[test]
    fn index_rejects_ambiguous_or_unsafe_entries() {
        let duplicate = WorkspaceLayout {
            patterns: vec![],
            packages: vec![
                package("packages/a", "same", "1.0.0"),
                package("packages/b", "same", "1.0.0"),
            ],
        };
        assert!(matches!(
            WorkspaceIndex::from_layout(&duplicate).unwrap_err(),
            WorkspaceError::DuplicateName { .. }
        ));

        let escaping = WorkspaceLayout {
            patterns: vec![],
            packages: vec![package("../outside", "outside", "1.0.0")],
        };
        assert!(matches!(
            WorkspaceIndex::from_layout(&escaping).unwrap_err(),
            WorkspaceError::InvalidPath { .. }
        ));
    }

    #[test]
    fn links_are_independent_of_discovery_order() {
        let left = index(vec![
            package("packages/z", "z", "2.0.0"),
            package("packages/a", "a", "1.0.0"),
        ]);
        let right = index(vec![
            package("packages/a", "a", "1.0.0"),
            package("packages/z", "z", "2.0.0"),
        ]);

        assert_eq!(left, right);
        assert_eq!(left.top_level_links(), right.top_level_links());
        assert_eq!(
            left.top_level_links()
                .iter()
                .map(|edge| edge.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "z"]
        );
    }

    #[test]
    fn duplicate_diagnostics_are_independent_of_discovery_order() {
        let packages = vec![
            package("packages/z", "same", "1.0.0"),
            package("packages/a", "same", "1.0.0"),
        ];
        let reversed = packages.iter().cloned().rev().collect();

        let first = WorkspaceIndex::from_layout(&WorkspaceLayout {
            patterns: vec![],
            packages,
        })
        .unwrap_err();
        let second = WorkspaceIndex::from_layout(&WorkspaceLayout {
            patterns: vec![],
            packages: reversed,
        })
        .unwrap_err();

        assert_eq!(first, second);
        assert_eq!(
            first,
            WorkspaceError::DuplicateName {
                name: "same".into(),
                first_path: "packages/a".into(),
                second_path: "packages/z".into(),
            }
        );
    }
}
