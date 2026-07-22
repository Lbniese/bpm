//! I/O-agnostic dependency graph placement core.
//!
//! This module contains the deterministic placement algorithm
//! (`GraphResolver::resolve_dependency`) and all its helpers.  It is
//! parameterised over a [`PackumentSource`] so the same placement logic can be
//! used by both the blocking and async resolver paths.
//!
//! ## Determinism contract
//!
//! Every traversal collection is `BTreeMap`/`BTreeSet`, the peer backtracking
//! loop calls `versions.sort()`, and `sort_packages()` canonicalises final
//! ordering.  A placement step must never depend on which fetch completed first.
//! See `docs/m7-concurrent-resolution-design.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use semver::Version;

use crate::integrity::Integrity;
use crate::lockfile::{LockSource, PeerProvider};
use crate::registry::{self, parse_spec, resolve_packument, VersionMetadata};
use crate::resolver::fetch::PackumentSource;
use crate::resolver::model::*;
use crate::resolver::overrides::OverrideSet;

use crate::resolver::platform::{self, check_package_platform, PackageReachability};
use crate::resolver::sources::{
    read_source_bytes, source_from_tarball_bytes, write_patched_tarball, DependencySource,
    SourceResolution,
};
use crate::resolver::workspaces::WorkspaceIndex;
use crate::resolver::{
    merged_dependencies, parent_path, registry_request, request_matches, version_request_to_string,
    workspace_metadata, ResolveError, ResolveSink, ResolvedDownloadUnit,
};

// ── Node ─────────────────────────────────────────────────────────────────

/// A single placed dependency in the resolution graph.
#[derive(Clone)]
pub(crate) struct Node {
    pub(crate) path: String,
    pub(crate) placement_name: String,
    pub(crate) metadata: VersionMetadata,
    pub(crate) resolved: String,
    pub(crate) integrity: String,
    pub(crate) dependencies: BTreeMap<String, String>,
    pub(crate) targets: BTreeMap<String, String>,
    pub(crate) optional: bool,
    pub(crate) dev: bool,
    pub(crate) peer_context: BTreeMap<String, PeerProvider>,
    pub(crate) source: LockSource,
    pub(crate) link: bool,
    pub(crate) workspace_target: Option<String>,
    pub(crate) source_dir: Option<PathBuf>,
}

// ── GraphResolver ────────────────────────────────────────────────────────

/// Synchronous, deterministic dependency graph resolver.
///
/// Generic over `S: PackumentSource` so the same placement logic serves both
/// the blocking resolver (via `RegistrySource`) and the async resolver (via an
/// adapter that drives async fetches to completion).
pub(crate) struct GraphResolver<'a, S: PackumentSource> {
    pub(crate) source: S,
    pub(crate) overrides: OverrideSet,
    pub(crate) nodes: BTreeMap<String, Node>,
    pub(crate) diagnostics: Vec<String>,
    pub(crate) workspace: Option<&'a WorkspaceIndex>,
    pub(crate) root_dir: Option<PathBuf>,
    pub(crate) target: TargetPlatform,
    pub(crate) sink: Option<&'a dyn ResolveSink>,
}

impl<'a, S: PackumentSource> GraphResolver<'a, S> {
    /// Create a new resolver.
    pub(crate) fn new(
        source: S,
        overrides: OverrideSet,
        workspace: Option<&'a WorkspaceIndex>,
        root_dir: Option<PathBuf>,
        target: TargetPlatform,
        sink: Option<&'a dyn ResolveSink>,
    ) -> Self {
        Self {
            source,
            overrides,
            nodes: BTreeMap::new(),
            diagnostics: Vec::new(),
            workspace,
            root_dir,
            target,
            sink,
        }
    }

    /// Resolve a single dependency specification into a node path.
    pub(crate) fn resolve_dependency(
        &mut self,
        parent: &str,
        name: &str,
        requested: &str,
        optional: bool,
        dev: bool,
    ) -> Result<Option<String>, ResolveError> {
        let ancestors = self.ancestor_chain(parent);
        let spec = self
            .overrides
            .effective_spec_for(name, requested, &ancestors)
            .to_owned();
        if let Some(workspace) = self.workspace {
            if let crate::resolver::workspaces::WorkspaceResolution::Link(edge) = workspace
                .resolve(name, &spec)
                .map_err(|error| ResolveError::Peer(error.to_string()))?
            {
                let relative_path = match edge.target.source {
                    PackageSource::Workspace { relative_path } => relative_path,
                    _ => unreachable!(),
                };
                let path = format!("node_modules/{name}");
                if self.nodes.contains_key(&path) {
                    self.upgrade_reachability(&path, optional, dev);
                    return Ok(Some(path));
                }

                let metadata = workspace_metadata(
                    name,
                    &edge.target.version,
                    workspace
                        .get(name)
                        .and_then(|workspace| workspace.manifest.as_ref()),
                );
                if !self.platform_allows(name, &metadata, optional)? {
                    return Ok(None);
                }
                let dependencies = merged_dependencies(&metadata);
                self.nodes.insert(
                    path.clone(),
                    Node {
                        path: path.clone(),
                        placement_name: name.to_owned(),
                        metadata,
                        resolved: String::new(),
                        integrity: String::new(),
                        dependencies: dependencies.clone(),
                        targets: BTreeMap::new(),
                        optional,
                        dev,
                        peer_context: BTreeMap::new(),
                        source: LockSource::Workspace {
                            relative_path: relative_path.clone(),
                        },
                        link: true,
                        workspace_target: Some(relative_path.clone()),
                        source_dir: workspace
                            .get(name)
                            .and_then(|workspace| workspace.manifest.as_ref())
                            .and_then(|manifest| manifest.source_dir.clone()),
                    },
                );
                self.resolve_children(&path, &dependencies, optional, dev)?;
                return Ok(Some(path));
            }
        }
        if let Some(source) = DependencySource::parse(&spec) {
            return self.resolve_source_dependency(parent, name, &spec, source, optional, dev);
        }
        let (_, visible_spec) = registry_request(name, &spec);
        if let Some(path) = self.find_visible(parent, name, &visible_spec) {
            self.upgrade_reachability(&path, optional, dev);
            return Ok(Some(path));
        }

        let path = if parent.is_empty() {
            format!("node_modules/{name}")
        } else {
            format!("{parent}/node_modules/{name}")
        };
        if self.nodes.contains_key(&path) {
            let selected = self.nodes.get(&path).expect("checked above");
            if request_matches(&visible_spec, &selected.metadata.version) {
                return Ok(Some(path));
            }
            return Err(ResolveError::PlacementConflict {
                path,
                package: name.to_owned(),
                requested: spec,
                selected: selected.metadata.version.to_string(),
            });
        }
        let (registry_name, registry_spec) = registry_request(name, &spec);
        let parsed = parse_spec(&format!("{registry_name}@{registry_spec}")).map_err(|source| {
            ResolveError::Registry {
                package: name.to_owned(),
                spec: spec.clone(),
                source,
            }
        })?;
        let registry_base = self.source.registry_for_package(&registry_name).to_owned();
        let packument =
            self.source
                .packument_for(&parsed)
                .map_err(|source| ResolveError::Registry {
                    package: name.to_owned(),
                    spec: spec.clone(),
                    source,
                })?;
        let mut resolved =
            resolve_packument(&parsed, &packument, &registry_base).map_err(|source| {
                ResolveError::Registry {
                    package: name.to_owned(),
                    spec: spec.clone(),
                    source,
                }
            })?;
        // If a visible provider already exists, try lower published versions
        // before accepting a peer-incompatible highest version. This is the
        // bounded backtracking point: candidates are deterministic semver
        // versions from one packument and no network request is repeated.
        if !self.peer_candidate_matches(&resolved.metadata, parent) {
            let mut versions: Vec<Version> = packument
                .versions
                .keys()
                .filter_map(|version| Version::parse(version).ok())
                .collect();
            versions.sort();
            versions.reverse();
            for version in versions {
                let exact = registry::PackageSpec {
                    name: registry_name.clone(),
                    req: registry::VersionRequest::Exact(version),
                };
                let candidate =
                    resolve_packument(&exact, &packument, &registry_base).map_err(|source| {
                        ResolveError::Registry {
                            package: name.to_owned(),
                            spec: spec.clone(),
                            source,
                        }
                    })?;
                if self.peer_candidate_matches(&candidate.metadata, parent) {
                    resolved = candidate;
                    break;
                }
            }
        }
        if !self.platform_allows(name, &resolved.metadata, optional)? {
            return Ok(None);
        }
        let dependencies = merged_dependencies(&resolved.metadata);
        self.nodes.insert(
            path.clone(),
            Node {
                path: path.clone(),
                placement_name: name.to_owned(),
                metadata: resolved.metadata,
                resolved: resolved.tarball_url,
                integrity: resolved.integrity,
                dependencies: dependencies.clone(),
                targets: BTreeMap::new(),
                optional,
                dev,
                peer_context: BTreeMap::new(),
                source: LockSource::Registry {
                    registry: registry_base,
                },
                link: false,
                workspace_target: None,
                source_dir: None,
            },
        );
        self.announce(&path);
        // Submit prefetches for this node's registry-typed children so sibling
        // packument fetches overlap while depth-first placement proceeds.
        self.prefetch_children(&dependencies);
        self.resolve_children(&path, &dependencies, optional, dev)?;
        Ok(Some(path))
    }

    /// Resolve all children of a node.
    fn resolve_children(
        &mut self,
        parent_path: &str,
        dependencies: &BTreeMap<String, String>,
        optional: bool,
        dev: bool,
    ) -> Result<(), ResolveError> {
        for (child, child_spec) in dependencies {
            let child_optional = self.nodes.get(parent_path).is_some_and(|node| {
                optional || node.metadata.optional_dependencies.contains_key(child)
            });
            if let Some(target) =
                self.resolve_dependency(parent_path, child, child_spec, child_optional, dev)?
            {
                if let Some(node) = self.nodes.get_mut(parent_path) {
                    node.targets.insert(child.clone(), target);
                }
            }
        }
        Ok(())
    }

    /// If a download sink is attached, announce a just-placed node so a caller
    /// can start fetching its tarball before the rest of the graph is resolved.
    /// No-op for linked (workspace) nodes and nodes without a download URL.
    fn announce(&self, path: &str) {
        let Some(sink) = self.sink else {
            return;
        };
        let unit = {
            let Some(node) = self.nodes.get(path) else {
                return;
            };
            if node.link || node.resolved.is_empty() {
                return;
            }
            ResolvedDownloadUnit {
                path: node.path.clone(),
                name: node.placement_name.clone(),
                url: node.resolved.clone(),
                integrity: if node.integrity.is_empty() {
                    None
                } else {
                    Some(Integrity::parse(&node.integrity).unwrap_or_else(|e| {
                        panic!(
                            "invalid integrity in resolved node for {}: {}: {}",
                            node.placement_name, node.integrity, e
                        )
                    }))
                },
            }
        };
        sink.emit(unit);
    }

    /// Submit best-effort packument prefetches for registry-typed children.
    pub(crate) fn prefetch_children(&self, dependencies: &BTreeMap<String, String>) {
        for (child, child_spec) in dependencies {
            if DependencySource::parse(child_spec).is_none()
                && self
                    .workspace
                    .and_then(|workspace| workspace.get(child))
                    .is_none()
            {
                self.source.prefetch_packument(child, Some(child_spec));
            }
        }
    }

    /// Resolve a non-registry (source) dependency.
    fn resolve_source_dependency(
        &mut self,
        parent: &str,
        name: &str,
        spec: &str,
        source: DependencySource,
        optional: bool,
        dev: bool,
    ) -> Result<Option<String>, ResolveError> {
        let path = if parent.is_empty() {
            format!("node_modules/{name}")
        } else {
            format!("{parent}/node_modules/{name}")
        };
        if self.nodes.contains_key(&path) {
            self.upgrade_reachability(&path, optional, dev);
            return Ok(Some(path));
        }
        let (resolved, _dependencies) = match source {
            DependencySource::Patch { inner, patch } => {
                let (source_res, deps) = self.resolve_patch_dependency(name, &inner, &patch)?;
                (source_res, deps)
            }
            source => {
                let base_dir = self.base_dir_for(parent);
                let source_res =
                    source
                        .resolve(&base_dir)
                        .map_err(|reason| ResolveError::Source {
                            package: name.to_owned(),
                            spec: spec.to_owned(),
                            reason,
                        })?;
                let deps = merged_dependencies(&source_res.metadata);
                (source_res, deps)
            }
        };
        let metadata = resolved.metadata;
        if !self.platform_allows(name, &metadata, optional)? {
            return Ok(None);
        }
        let dependencies = merged_dependencies(&metadata);
        self.nodes.insert(
            path.clone(),
            Node {
                path: path.clone(),
                placement_name: name.to_owned(),
                metadata,
                resolved: resolved.resolved.clone(),
                integrity: resolved.integrity.clone().unwrap_or_default(),
                dependencies: dependencies.clone(),
                targets: BTreeMap::new(),
                optional,
                dev,
                peer_context: BTreeMap::new(),
                source: resolved.source,
                link: resolved.link,
                workspace_target: resolved.workspace_target,
                source_dir: resolved.source_dir,
            },
        );
        self.announce(&path);
        self.resolve_children(&path, &dependencies, optional, dev)?;
        Ok(Some(path))
    }

    /// Resolve a patch dependency.
    fn resolve_patch_dependency(
        &self,
        name: &str,
        inner: &str,
        patch: &PathBuf,
    ) -> Result<(SourceResolution, BTreeMap<String, String>), ResolveError> {
        let patch_path = if patch.is_absolute() {
            patch.clone()
        } else {
            let base_dir = self.base_dir_for("");
            base_dir.join(patch)
        };
        let patch_text = fs::read_to_string(&patch_path).map_err(|error| ResolveError::Source {
            package: name.to_owned(),
            spec: format!("patch:{inner}#{}", patch_path.display()),
            reason: format!("cannot read patch {}: {error}", patch_path.display()),
        })?;
        let (source_resolution, source_bytes) = self.resolve_patch_inner(name, inner)?;
        if source_resolution.link {
            return Err(ResolveError::Source {
                package: name.to_owned(),
                spec: format!("patch:{inner}#{}", patch_path.display()),
                reason: "patch: currently supports tarball, registry, and git sources, not linked directories".into(),
            });
        }
        let patched = crate::patch::apply_unified_patch_to_tgz(&source_bytes, &patch_text)
            .map_err(|error| ResolveError::Source {
                package: name.to_owned(),
                spec: format!("patch:{inner}#{}", patch_path.display()),
                reason: error.to_string(),
            })?;
        let url = write_patched_tarball(&self.base_dir_for(""), &patched).map_err(|error| {
            ResolveError::Source {
                package: name.to_owned(),
                spec: format!("patch:{inner}#{}", patch_path.display()),
                reason: error.to_string(),
            }
        })?;
        let mut resolved = source_from_tarball_bytes(
            &url,
            patched,
            LockSource::Patch {
                source: Box::new(source_resolution.source),
                patch: patch_path.display().to_string(),
            },
        )
        .map_err(|error| ResolveError::Source {
            package: name.to_owned(),
            spec: format!("patch:{inner}#{}", patch_path.display()),
            reason: error.to_string(),
        })?;
        resolved.resolved = url;
        let deps = merged_dependencies(&resolved.metadata);
        Ok((resolved, deps))
    }

    /// Resolve the inner source of a patch dependency.
    fn resolve_patch_inner(
        &self,
        name: &str,
        inner: &str,
    ) -> Result<(SourceResolution, Vec<u8>), ResolveError> {
        if let Some(source) = DependencySource::parse(inner) {
            if matches!(source, DependencySource::Patch { .. }) {
                return Err(ResolveError::Source {
                    package: name.to_owned(),
                    spec: format!("patch:{}", inner),
                    reason: "nested patch: sources are not supported".into(),
                });
            }
            let base_dir = self.base_dir_for("");
            let resolution = source
                .resolve(&base_dir)
                .map_err(|reason| ResolveError::Source {
                    package: name.to_owned(),
                    spec: format!("patch:{}", inner),
                    reason,
                })?;
            let http = self.source.http().ok_or_else(|| ResolveError::Source {
                package: name.to_owned(),
                spec: format!("patch:{}", inner),
                reason: "HTTP client not available for patch resolution".into(),
            })?;
            let bytes = read_source_bytes(http, &resolution.resolved).map_err(|reason| {
                ResolveError::Source {
                    package: name.to_owned(),
                    spec: format!("patch:{}", inner),
                    reason,
                }
            })?;
            return Ok((resolution, bytes));
        }
        let requested = if inner.trim().is_empty() {
            "*"
        } else {
            inner.trim()
        };
        let (registry_name, _registry_spec, parsed) = match parse_spec(requested) {
            Ok(parsed) if parsed.name == name => {
                let request = version_request_to_string(&parsed.req);
                (parsed.name.clone(), request, parsed)
            }
            _ => {
                let (registry_name, registry_spec) = registry_request(name, requested);
                let parsed = parse_spec(&format!("{registry_name}@{registry_spec}")).map_err(
                    |error| {
                        ResolveError::Source {
                            package: name.to_owned(),
                            spec: format!("patch:{inner}"),
                            reason: format!(
                                "invalid patched registry source {registry_name}@{registry_spec}: {error}"
                            ),
                        }
                    },
                )?;
                (registry_name, registry_spec, parsed)
            }
        };
        let registry_base = self.source.registry_for_package(&registry_name).to_owned();
        let packument =
            self.source
                .packument_for(&parsed)
                .map_err(|error| ResolveError::Source {
                    package: name.to_owned(),
                    spec: format!("patch:{inner}"),
                    reason: error.to_string(),
                })?;
        let resolved = resolve_packument(&parsed, &packument, &registry_base).map_err(|error| {
            ResolveError::Source {
                package: name.to_owned(),
                spec: format!("patch:{inner}"),
                reason: error.to_string(),
            }
        })?;
        let http = self.source.http().ok_or_else(|| ResolveError::Source {
            package: name.to_owned(),
            spec: format!("patch:{inner}"),
            reason: "HTTP client not available for patch resolution".into(),
        })?;
        let bytes = read_source_bytes(http, &resolved.tarball_url).map_err(|reason| {
            ResolveError::Source {
                package: name.to_owned(),
                spec: format!("patch:{inner}"),
                reason,
            }
        })?;
        let resolution = SourceResolution {
            metadata: resolved.metadata,
            resolved: resolved.tarball_url,
            integrity: Some(resolved.integrity),
            source: LockSource::Registry {
                registry: registry_base,
            },
            link: false,
            workspace_target: None,
            source_dir: None,
        };
        Ok((resolution, bytes))
    }

    /// Build ancestor chain for override resolution.
    fn ancestor_chain(&self, parent: &str) -> Vec<(String, Version)> {
        if parent.is_empty() {
            return Vec::new();
        }
        let mut paths = Vec::new();
        let mut current = parent.to_owned();
        loop {
            paths.push(current.clone());
            let next = parent_path(&current);
            if next.is_empty() {
                break;
            }
            current = next;
        }
        paths.reverse();
        paths
            .into_iter()
            .filter_map(|path| {
                self.nodes
                    .get(&path)
                    .map(|node| (node.placement_name.clone(), node.metadata.version.clone()))
            })
            .collect()
    }

    fn base_dir_for(&self, parent: &str) -> PathBuf {
        if parent.is_empty() {
            return self.root_dir.clone().unwrap_or_else(|| PathBuf::from("."));
        }
        self.nodes
            .get(parent)
            .and_then(|node| node.source_dir.clone())
            .unwrap_or_else(|| self.root_dir.clone().unwrap_or_else(|| PathBuf::from(".")))
    }

    fn upgrade_reachability(&mut self, path: &str, optional: bool, dev: bool) {
        if let Some(node) = self.nodes.get_mut(path) {
            // A package is optional/dev only when every path reaching it has
            // that property. A later required or production edge therefore
            // upgrades an already-created placement in place.
            node.optional &= optional;
            node.dev &= dev;
        }
    }

    fn platform_allows(
        &mut self,
        name: &str,
        metadata: &VersionMetadata,
        optional: bool,
    ) -> Result<bool, ResolveError> {
        let constraints = PlatformConstraints {
            os: metadata.os.iter().cloned().collect::<BTreeSet<_>>(),
            cpu: metadata.cpu.iter().cloned().collect::<BTreeSet<_>>(),
            libc: metadata.libc.iter().cloned().collect::<BTreeSet<_>>(),
        };
        match check_package_platform(
            &format!("{}@{}", name, metadata.version),
            &constraints,
            &self.target,
            if optional {
                PackageReachability::OptionalOnly
            } else {
                PackageReachability::Required
            },
        ) {
            Ok(platform::PlatformDisposition::Compatible) => Ok(true),
            Ok(platform::PlatformDisposition::SkipOptional(diagnostic)) => {
                self.diagnostics.push(diagnostic.message);
                Ok(false)
            }
            Err(_) => Err(ResolveError::Platform {
                package: name.to_owned(),
                version: metadata.version.to_string(),
            }),
        }
    }

    fn find_visible(&self, parent: &str, name: &str, spec: &str) -> Option<String> {
        let mut candidate = if parent.is_empty() {
            String::new()
        } else {
            parent.to_owned()
        };
        loop {
            let path = if candidate.is_empty() {
                format!("node_modules/{name}")
            } else {
                format!("{candidate}/node_modules/{name}")
            };
            if let Some(node) = self.nodes.get(&path) {
                if request_matches(spec, &node.metadata.version) {
                    return Some(path);
                }
            }
            if candidate.is_empty() {
                break;
            }
            candidate = candidate
                .rsplit_once("/node_modules/")
                .map(|(prefix, _)| prefix.to_owned())
                .unwrap_or_default();
        }
        None
    }

    pub(crate) fn find_visible_any(&self, parent: &str, name: &str) -> Option<String> {
        let mut candidate = parent.to_owned();
        loop {
            let path = if candidate.is_empty() {
                format!("node_modules/{name}")
            } else {
                format!("{candidate}/node_modules/{name}")
            };
            if self.nodes.contains_key(&path) {
                return Some(path);
            }
            if candidate.is_empty() {
                return None;
            }
            candidate = candidate
                .rsplit_once("/node_modules/")
                .map(|(prefix, _)| prefix.to_owned())
                .unwrap_or_default();
        }
    }

    fn peer_candidate_matches(&self, metadata: &VersionMetadata, parent: &str) -> bool {
        metadata.peer_dependencies.iter().all(|(name, range)| {
            let Some(path) = self.find_visible_any(parent, name) else {
                return metadata
                    .peer_dependencies_meta
                    .get(name)
                    .is_some_and(|meta| meta.optional);
            };
            self.nodes
                .get(&path)
                .is_some_and(|provider| request_matches(range, &provider.metadata.version))
        })
    }

    pub(crate) fn visible_providers(
        &self,
        parent: &str,
    ) -> BTreeMap<String, crate::resolver::peer::VisibleProvider> {
        let mut providers = BTreeMap::new();
        let mut candidate = parent.to_owned();
        loop {
            for node in self.nodes.values() {
                let expected = if candidate.is_empty() {
                    format!("node_modules/{}", node.placement_name)
                } else {
                    format!("{candidate}/node_modules/{}", node.placement_name)
                };
                if node.path == expected {
                    providers
                        .entry(node.metadata.name.clone())
                        .or_insert_with(|| crate::resolver::peer::VisibleProvider {
                            identity: ProviderIdentity {
                                name: node.metadata.name.clone(),
                                version: node.metadata.version.to_string(),
                                source: package_source_for_node(
                                    node,
                                    self.source.registry_for_package(&node.metadata.name),
                                ),
                            },
                            path: node.path.clone(),
                            competing_requester: None,
                        });
                }
            }
            if candidate.is_empty() {
                break;
            }
            candidate = candidate
                .rsplit_once("/node_modules/")
                .map(|(prefix, _)| prefix.to_owned())
                .unwrap_or_default();
        }
        providers
    }
}

// ── Standalone helpers ───────────────────────────────────────────────────

fn package_source_for_node(node: &Node, registry: &str) -> PackageSource {
    match &node.source {
        LockSource::Workspace { relative_path } => PackageSource::Workspace {
            relative_path: relative_path.clone(),
        },
        LockSource::Registry { .. }
        | LockSource::File { .. }
        | LockSource::Tarball { .. }
        | LockSource::Git { .. }
        | LockSource::Patch { .. } => PackageSource::Registry {
            registry: registry.to_owned(),
        },
    }
}
