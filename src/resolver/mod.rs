//! Deterministic registry dependency graph resolution.

pub mod model;
pub mod overrides;
pub mod peer;
pub mod platform;
pub mod workspaces;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine;
use semver::Version;
use sha2::Digest;
use thiserror::Error;

use crate::lockfile::{
    LockDependency, LockSource, Lockfile, PackageEntry, PackageResolution, RootEntry,
    RootResolution,
};
use crate::manifest::PackageManifest;
use crate::registry::{
    parse_spec, resolve_packument, RegistryClient, RegistryError, VersionMetadata,
};

pub use model::*;

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("registry resolution failed for {package}@{spec}: {source}")]
    Registry {
        package: String,
        spec: String,
        #[source]
        source: RegistryError,
    },
    #[error("package {package}@{version} is incompatible with the current platform")]
    Platform { package: String, version: String },
    #[error("invalid dependency range {package}@{spec}: {reason}")]
    InvalidRange {
        package: String,
        spec: String,
        reason: String,
    },
    #[error("root override validation failed: {0}")]
    Override(String),
    #[error("peer dependency conflict: {0}")]
    Peer(String),
    #[error("non-registry source resolution failed for {package}@{spec}: {reason}")]
    Source {
        package: String,
        spec: String,
        reason: String,
    },
    #[error("dependency placement conflict at {path}: {package}@{requested} cannot replace selected {selected}")]
    PlacementConflict {
        path: String,
        package: String,
        requested: String,
        selected: String,
    },
}

#[derive(Clone)]
struct Node {
    path: String,
    placement_name: String,
    metadata: VersionMetadata,
    resolved: String,
    integrity: String,
    dependencies: BTreeMap<String, String>,
    targets: BTreeMap<String, String>,
    optional: bool,
    dev: bool,
    peer_context: BTreeMap<String, crate::lockfile::PeerProvider>,
    source: LockSource,
    link: bool,
    workspace_target: Option<String>,
    source_dir: Option<PathBuf>,
}

/// Resolve a manifest into the canonical BPM lockfile.
pub fn resolve_manifest(
    manifest: &PackageManifest,
    registry: &RegistryClient,
    generator: &str,
) -> Result<Lockfile, ResolveError> {
    resolve_manifest_with_workspaces(manifest, registry, generator, None)
}

pub fn resolve_manifest_with_workspaces(
    manifest: &PackageManifest,
    registry: &RegistryClient,
    generator: &str,
    workspace: Option<&crate::resolver::workspaces::WorkspaceIndex>,
) -> Result<Lockfile, ResolveError> {
    resolve_manifest_with_options(
        manifest,
        registry,
        generator,
        workspace,
        crate::resolver::peer::PeerMode::Strict,
    )
}

/// Workspace-aware variant of [`resolve_manifest_with_target`].
pub fn resolve_manifest_with_workspaces_and_target(
    manifest: &PackageManifest,
    registry: &RegistryClient,
    generator: &str,
    workspace: Option<&crate::resolver::workspaces::WorkspaceIndex>,
    target: TargetPlatform,
) -> Result<Lockfile, ResolveError> {
    resolve_manifest_with_options_and_target(
        manifest,
        registry,
        generator,
        workspace,
        crate::resolver::peer::PeerMode::Strict,
        target,
    )
}

pub fn resolve_manifest_with_options(
    manifest: &PackageManifest,
    registry: &RegistryClient,
    generator: &str,
    workspace: Option<&crate::resolver::workspaces::WorkspaceIndex>,
    peer_mode: crate::resolver::peer::PeerMode,
) -> Result<Lockfile, ResolveError> {
    resolve_manifest_with_options_and_target(
        manifest,
        registry,
        generator,
        workspace,
        peer_mode,
        current_target_platform(),
    )
}

/// Resolve a manifest for an explicit npm target platform.
///
/// Keeping the target in the resolver (rather than consulting the host from
/// deep in the traversal) makes cross-platform lock generation deterministic
/// and lets callers use the same graph on CI and build machines.
pub fn resolve_manifest_with_target(
    manifest: &PackageManifest,
    registry: &RegistryClient,
    generator: &str,
    target: TargetPlatform,
) -> Result<Lockfile, ResolveError> {
    resolve_manifest_with_options_and_target(
        manifest,
        registry,
        generator,
        None,
        crate::resolver::peer::PeerMode::Strict,
        target,
    )
}

pub fn resolve_manifest_with_options_and_target(
    manifest: &PackageManifest,
    registry: &RegistryClient,
    generator: &str,
    workspace: Option<&crate::resolver::workspaces::WorkspaceIndex>,
    peer_mode: crate::resolver::peer::PeerMode,
    target: TargetPlatform,
) -> Result<Lockfile, ResolveError> {
    let mut root_deps = manifest.dependencies.clone();
    // npm's optionalDependencies take precedence over dependencies with the
    // same name. Peer dependencies are root requests too: npm installs a
    // missing root peer so that packages below the root can bind to it.
    for (name, spec) in &manifest.optional_dependencies {
        root_deps.insert(name.clone(), spec.clone());
    }
    for (name, spec) in &manifest.peer_dependencies {
        root_deps
            .entry(name.clone())
            .or_insert_with(|| spec.clone());
    }
    for (name, spec) in &manifest.dev_dependencies {
        root_deps
            .entry(name.clone())
            .or_insert_with(|| spec.clone());
    }
    let overrides = crate::resolver::overrides::OverrideSet::from_manifest(
        &manifest.overrides,
        &root_deps,
        crate::resolver::overrides::OverrideOrigin::Root,
    )
    .map_err(|error| ResolveError::Override(error.to_string()))?;
    let normalized_overrides = overrides.as_map().clone();

    let mut resolver = GraphResolver {
        registry,
        overrides,
        nodes: BTreeMap::new(),
        diagnostics: Vec::new(),
        workspace,
        root_dir: manifest.source_dir.clone(),
        target: target.clone(),
    };
    let mut root_targets = BTreeMap::new();
    for (name, spec) in &root_deps {
        let optional = manifest.optional_dependencies.contains_key(name);
        let dev = manifest.dev_dependencies.contains_key(name)
            && !manifest.dependencies.contains_key(name)
            && !manifest.optional_dependencies.contains_key(name);
        if let Some(path) = resolver.resolve_dependency("", name, spec, optional, dev)? {
            root_targets.insert(name.clone(), (spec.clone(), path));
        }
    }

    let mut lock = Lockfile::new(generator);
    lock.root = RootEntry {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        dependencies: root_deps,
    };
    lock.resolution.root = RootResolution {
        dev_dependencies: manifest.dev_dependencies.clone(),
        optional_dependencies: manifest.optional_dependencies.clone(),
        overrides: normalized_overrides,
        target: Some(crate::lockfile::LockTarget {
            os: target.os,
            cpu: target.cpu,
            libc: target.libc,
        }),
        ..RootResolution::default()
    };
    lock.resolution.root.peer_mode = match peer_mode {
        crate::resolver::peer::PeerMode::Strict => crate::lockfile::PeerMode::Strict,
        crate::resolver::peer::PeerMode::LegacyIgnore => crate::lockfile::PeerMode::LegacyIgnore,
    };
    // Peer providers may be declared after their consumers in the manifest.
    // Validate only after the complete placement graph is known; this gives
    // the resolver a deterministic backtracking point instead of rejecting a
    // temporarily-incomplete parent context during depth-first traversal.
    let node_paths: Vec<String> = resolver.nodes.keys().cloned().collect();
    for path in node_paths {
        let (metadata, parent) = {
            let node = resolver.nodes.get(&path).expect("node path exists");
            (node.metadata.clone(), parent_path(&path))
        };
        let providers = resolver.visible_providers(&parent);
        let visible =
            crate::resolver::peer::VisibleProviders::new(std::iter::once(path.clone()), providers);
        let context = crate::resolver::peer::bind_peer_context(&metadata, &visible, peer_mode)
            .map_err(|error| ResolveError::Peer(error.to_string()))?;
        let peer_context = context
            .0
            .into_iter()
            .map(|(name, provider)| {
                let provider_name = provider.name.clone();
                let provider_path = resolver
                    .find_visible_any(&parent, &provider_name)
                    .unwrap_or_default();
                let source = resolver
                    .nodes
                    .get(&provider_path)
                    .map(|node| node.source.clone())
                    .unwrap_or_else(|| crate::lockfile::LockSource::Registry {
                        registry: registry.registry_for_package(&provider_name).to_owned(),
                    });
                (
                    name,
                    crate::lockfile::PeerProvider {
                        name: provider.name,
                        version: provider.version,
                        source,
                        path: provider_path,
                    },
                )
            })
            .collect();
        if let Some(node) = resolver.nodes.get_mut(&path) {
            node.peer_context = peer_context;
        }
    }

    for node in resolver.nodes.values() {
        lock.packages.push(PackageEntry {
            path: node.path.clone(),
            name: node.placement_name.clone(),
            version: node.metadata.version.to_string(),
            resolved: node.resolved.clone(),
            workspace_target: node.workspace_target.clone(),
            integrity: Some(node.integrity.clone()),
            link: node.link,
            dev: node.dev,
            optional: node.optional,
            os: node.metadata.os.clone(),
            cpu: node.metadata.cpu.clone(),
            bin: node.metadata.bin.clone(),
            dependencies: node.dependencies.clone(),
        });
    }
    for node in resolver.nodes.values() {
        let mut dependencies = BTreeMap::new();
        for (name, spec) in &node.dependencies {
            if let Some(target) = node.targets.get(name) {
                dependencies.insert(
                    name.clone(),
                    LockDependency {
                        spec: spec.clone(),
                        target: target.clone(),
                    },
                );
            }
        }
        lock.resolution.packages.insert(
            node.path.clone(),
            PackageResolution {
                source: node.source.clone(),
                dev_optional: node.dev || node.optional,
                dependencies,
                has_install_script: node.metadata.has_install_script,
                peer: !node.metadata.peer_dependencies.is_empty(),
                libc: Vec::new(),
                optional_dependencies: node
                    .metadata
                    .optional_dependencies
                    .iter()
                    .filter_map(|(name, spec)| {
                        node.targets.get(name).map(|target| {
                            (
                                name.clone(),
                                LockDependency {
                                    spec: spec.clone(),
                                    target: target.clone(),
                                },
                            )
                        })
                    })
                    .collect(),
                peer_dependencies: node
                    .metadata
                    .peer_dependencies
                    .iter()
                    .filter_map(|(name, spec)| {
                        node.peer_context.get(name).map(|provider| {
                            (
                                name.clone(),
                                LockDependency {
                                    spec: spec.clone(),
                                    target: provider.path.clone(),
                                },
                            )
                        })
                    })
                    .collect(),
                optional_peers: node
                    .metadata
                    .peer_dependencies_meta
                    .iter()
                    .filter(|(_, meta)| meta.optional)
                    .map(|(name, _)| name.clone())
                    .collect(),
                peer_context: node.peer_context.clone(),
                workspace_target: node.workspace_target.clone(),
            },
        );
    }
    lock.sort_packages();
    let _ = root_targets;
    Ok(lock)
}

struct GraphResolver<'a> {
    registry: &'a RegistryClient,
    overrides: crate::resolver::overrides::OverrideSet,
    nodes: BTreeMap<String, Node>,
    diagnostics: Vec<String>,
    workspace: Option<&'a crate::resolver::workspaces::WorkspaceIndex>,
    root_dir: Option<PathBuf>,
    target: TargetPlatform,
}

impl<'a> GraphResolver<'a> {
    fn resolve_dependency(
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
                    crate::resolver::model::PackageSource::Workspace { relative_path } => {
                        relative_path
                    }
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
                for (child, child_spec) in dependencies {
                    let child_optional = self.nodes.get(&path).is_some_and(|node| {
                        optional || node.metadata.optional_dependencies.contains_key(&child)
                    });
                    if let Some(target) =
                        self.resolve_dependency(&path, &child, &child_spec, child_optional, dev)?
                    {
                        if let Some(node) = self.nodes.get_mut(&path) {
                            node.targets.insert(child, target);
                        }
                    }
                }
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
        let registry_base = self
            .registry
            .registry_for_package(&registry_name)
            .to_owned();
        let packument =
            self.registry
                .packument(&registry_name)
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
                let exact = crate::registry::PackageSpec {
                    name: registry_name.clone(),
                    req: crate::registry::VersionRequest::Exact(version),
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
        for (child, child_spec) in dependencies {
            let child_optional = self.nodes.get(&path).is_some_and(|node| {
                optional || node.metadata.optional_dependencies.contains_key(&child)
            });
            if let Some(target) =
                self.resolve_dependency(&path, &child, &child_spec, child_optional, dev)?
            {
                if let Some(node) = self.nodes.get_mut(&path) {
                    node.targets.insert(child, target);
                }
            }
        }
        Ok(Some(path))
    }

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
        let base_dir = self.base_dir_for(parent);
        let resolved = match source {
            DependencySource::Patch { inner, patch } => self
                .resolve_patch_dependency(name, &inner, &patch, &base_dir)
                .map_err(|reason| ResolveError::Source {
                    package: name.to_owned(),
                    spec: spec.to_owned(),
                    reason,
                })?,
            source => source
                .resolve(&base_dir)
                .map_err(|reason| ResolveError::Source {
                    package: name.to_owned(),
                    spec: spec.to_owned(),
                    reason,
                })?,
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
        for (child, child_spec) in dependencies {
            let child_optional = self.nodes.get(&path).is_some_and(|node| {
                optional || node.metadata.optional_dependencies.contains_key(&child)
            });
            if let Some(target) =
                self.resolve_dependency(&path, &child, &child_spec, child_optional, dev)?
            {
                if let Some(node) = self.nodes.get_mut(&path) {
                    node.targets.insert(child, target);
                }
            }
        }
        Ok(Some(path))
    }

    fn resolve_patch_dependency(
        &self,
        name: &str,
        inner: &str,
        patch: &Path,
        base_dir: &Path,
    ) -> Result<SourceResolution, String> {
        let patch_path = if patch.is_absolute() {
            patch.to_path_buf()
        } else {
            base_dir.join(patch)
        };
        let patch_text = fs::read_to_string(&patch_path)
            .map_err(|error| format!("cannot read patch {}: {error}", patch_path.display()))?;
        let (source_resolution, source_bytes) = self.resolve_patch_inner(name, inner, base_dir)?;
        if source_resolution.link {
            return Err("patch: currently supports tarball, registry, and git sources, not linked directories".into());
        }
        let patched = crate::patch::apply_unified_patch_to_tgz(&source_bytes, &patch_text)
            .map_err(|error| error.to_string())?;
        let url = write_patched_tarball(base_dir, &patched)?;
        let mut resolved = source_from_tarball_bytes(
            &url,
            patched,
            LockSource::Patch {
                source: Box::new(source_resolution.source),
                patch: patch_path.display().to_string(),
            },
        )?;
        resolved.resolved = url;
        Ok(resolved)
    }

    fn resolve_patch_inner(
        &self,
        name: &str,
        inner: &str,
        base_dir: &Path,
    ) -> Result<(SourceResolution, Vec<u8>), String> {
        if let Some(source) = DependencySource::parse(inner) {
            if matches!(source, DependencySource::Patch { .. }) {
                return Err("nested patch: sources are not supported".into());
            }
            let resolution = source.resolve(base_dir)?;
            let bytes = read_source_bytes(self.registry.http(), &resolution.resolved)?;
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
                        format!(
                            "invalid patched registry source {registry_name}@{registry_spec}: {error}"
                        )
                    },
                )?;
                (registry_name, registry_spec, parsed)
            }
        };
        let registry_base = self
            .registry
            .registry_for_package(&registry_name)
            .to_owned();
        let packument = self
            .registry
            .packument(&registry_name)
            .map_err(|error| error.to_string())?;
        let resolved = resolve_packument(&parsed, &packument, &registry_base)
            .map_err(|error| error.to_string())?;
        let bytes = read_source_bytes(self.registry.http(), &resolved.tarball_url)?;
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
        let constraints = crate::resolver::model::PlatformConstraints {
            os: metadata.os.iter().cloned().collect::<BTreeSet<_>>(),
            cpu: metadata.cpu.iter().cloned().collect::<BTreeSet<_>>(),
            libc: metadata.libc.iter().cloned().collect::<BTreeSet<_>>(),
        };
        match crate::resolver::platform::check_package_platform(
            &format!("{}@{}", name, metadata.version),
            &constraints,
            &self.target,
            if optional {
                crate::resolver::platform::PackageReachability::OptionalOnly
            } else {
                crate::resolver::platform::PackageReachability::Required
            },
        ) {
            Ok(crate::resolver::platform::PlatformDisposition::Compatible) => Ok(true),
            Ok(crate::resolver::platform::PlatformDisposition::SkipOptional(diagnostic)) => {
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

    fn find_visible_any(&self, parent: &str, name: &str) -> Option<String> {
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

    fn visible_providers(
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
                            identity: crate::resolver::model::ProviderIdentity {
                                name: node.metadata.name.clone(),
                                version: node.metadata.version.to_string(),
                                source: package_source_for_node(
                                    node,
                                    self.registry.registry_for_package(&node.metadata.name),
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

#[derive(Debug, Clone)]
enum DependencySource {
    File(PathBuf),
    Tarball(String),
    Git {
        url: String,
        reference: Option<String>,
    },
    Patch {
        inner: String,
        patch: PathBuf,
    },
}

#[derive(Debug, Clone)]
struct SourceResolution {
    metadata: VersionMetadata,
    resolved: String,
    integrity: Option<String>,
    source: LockSource,
    link: bool,
    workspace_target: Option<String>,
    source_dir: Option<PathBuf>,
}

impl DependencySource {
    fn parse(spec: &str) -> Option<Self> {
        let lower = spec.to_ascii_lowercase();
        if let Some(payload) = spec.strip_prefix("patch:") {
            let (inner, patch) = payload.rsplit_once('#')?;
            return Some(Self::Patch {
                inner: inner.to_owned(),
                patch: PathBuf::from(patch),
            });
        }
        if let Some(path) = spec
            .strip_prefix("file:")
            .or_else(|| spec.strip_prefix("link:"))
        {
            return Some(Self::File(PathBuf::from(path)));
        }
        if spec.starts_with("./") || spec.starts_with("../") || spec.starts_with('/') {
            return Some(Self::File(PathBuf::from(spec)));
        }
        if (lower.starts_with("http://") || lower.starts_with("https://"))
            && (lower.ends_with(".tgz") || lower.contains(".tgz?"))
        {
            return Some(Self::Tarball(spec.to_owned()));
        }
        if lower.starts_with("git+")
            || lower.starts_with("git://")
            || lower.starts_with("ssh://")
            || lower.starts_with("git@")
            || lower.starts_with("github:")
            || lower.starts_with("gitlab:")
            || lower.starts_with("bitbucket:")
            || looks_like_hosted_git(spec)
        {
            let (url, reference) = split_git_reference(spec);
            return Some(Self::Git { url, reference });
        }
        None
    }

    fn resolve(self, base_dir: &Path) -> Result<SourceResolution, String> {
        match self {
            Self::File(path) => resolve_file_source(base_dir, &path),
            Self::Tarball(url) => resolve_tarball_source(&url),
            Self::Git { url, reference } => resolve_git_source(&url, reference.as_deref()),
            Self::Patch { .. } => Err("patch sources require resolver context".into()),
        }
    }
}

fn resolve_file_source(base_dir: &Path, path: &Path) -> Result<SourceResolution, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    };
    let absolute = absolute
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", absolute.display()))?;
    if absolute.is_dir() {
        let manifest = PackageManifest::from_path(&absolute.join("package.json"))
            .map_err(|error| error.to_string())?;
        let name = manifest.name.clone().unwrap_or_else(|| {
            absolute
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("package")
                .to_owned()
        });
        let version = manifest.version.clone().unwrap_or_else(|| "0.0.0".into());
        return Ok(SourceResolution {
            metadata: workspace_metadata(&name, &version, Some(&manifest)),
            resolved: String::new(),
            integrity: None,
            source: LockSource::File {
                path: absolute.display().to_string(),
            },
            link: true,
            workspace_target: Some(absolute.display().to_string()),
            source_dir: Some(absolute),
        });
    }
    resolve_tarball_file(&absolute)
}

fn resolve_tarball_file(path: &Path) -> Result<SourceResolution, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let url = format!("file://{}", path.display());
    source_from_tarball_bytes(&url, bytes, LockSource::Tarball { url: url.clone() })
}

fn resolve_tarball_source(url: &str) -> Result<SourceResolution, String> {
    let http = crate::http::HttpClient::new(crate::config::NpmConfig::default());
    let bytes = read_source_bytes(&http, url)?;
    source_from_tarball_bytes(
        url,
        bytes,
        LockSource::Tarball {
            url: url.to_owned(),
        },
    )
}

fn read_source_bytes(http: &crate::http::HttpClient, url: &str) -> Result<Vec<u8>, String> {
    if let Some(path) = url.strip_prefix("file://") {
        return fs::read(path).map_err(|error| format!("cannot read {path}: {error}"));
    }
    if !url.contains("://") {
        return fs::read(url).map_err(|error| format!("cannot read {url}: {error}"));
    }
    let mut response = http.stream(url).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    response
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read tarball response: {error}"))?;
    Ok(bytes)
}

fn write_patched_tarball(base_dir: &Path, bytes: &[u8]) -> Result<String, String> {
    let mut hasher = sha2::Sha512::new();
    hasher.update(bytes);
    let hex = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let root = if base_dir.is_dir() {
        base_dir.to_path_buf()
    } else {
        std::env::temp_dir()
    };
    let dir = root.join(".bpm").join("patches");
    fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create patch cache {}: {error}", dir.display()))?;
    let path = dir.join(format!("{hex}.tgz"));
    if !path.exists() {
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, bytes)
            .map_err(|error| format!("cannot write patched tarball {}: {error}", tmp.display()))?;
        fs::rename(&tmp, &path).map_err(|error| {
            format!(
                "cannot publish patched tarball {} -> {}: {error}",
                tmp.display(),
                path.display()
            )
        })?;
    }
    Ok(format!("file://{}", path.display()))
}

fn resolve_git_source(url: &str, reference: Option<&str>) -> Result<SourceResolution, String> {
    if let Some(tarball_url) = hosted_git_tarball_url(url, reference) {
        return resolve_tarball_source(&tarball_url).map(|mut resolution| {
            resolution.source = LockSource::Git {
                url: url.to_owned(),
                reference: reference.map(str::to_owned),
            };
            resolution
        });
    }
    let tarball = git_archive_tarball(url, reference.unwrap_or("HEAD"))?;
    let cache_url = format!("file://{}", tarball.display());
    let bytes = fs::read(&tarball)
        .map_err(|error| format!("cannot read git archive {}: {error}", tarball.display()))?;
    source_from_tarball_bytes(
        &cache_url,
        bytes,
        LockSource::Git {
            url: url.to_owned(),
            reference: reference.map(str::to_owned),
        },
    )
}

fn git_archive_tarball(url: &str, reference: &str) -> Result<PathBuf, String> {
    let mut key_hasher = sha2::Sha512::new();
    key_hasher.update(url.as_bytes());
    key_hasher.update([0]);
    key_hasher.update(reference.as_bytes());
    let key = key_hasher
        .finalize()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let cache_dir = std::env::temp_dir().join("bpm-git-archives");
    fs::create_dir_all(&cache_dir).map_err(|error| {
        format!(
            "cannot create git archive cache {}: {error}",
            cache_dir.display()
        )
    })?;
    let dest = cache_dir.join(format!("{key}.tgz"));
    if dest.is_file() {
        return Ok(dest);
    }
    let output = Command::new("git")
        .args([
            "archive",
            "--format=tar",
            &format!("--remote={url}"),
            reference,
        ])
        .output()
        .map_err(|error| format!("cannot execute git archive for {url}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git archive failed for {url}#{reference}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let tmp = dest.with_extension("tmp");
    {
        let file = fs::File::create(&tmp)
            .map_err(|error| format!("cannot create {}: {error}", tmp.display()))?;
        let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        std::io::copy(&mut Cursor::new(output.stdout), &mut encoder)
            .map_err(|error| format!("cannot gzip git archive: {error}"))?;
        encoder
            .finish()
            .map_err(|error| format!("cannot finish git archive: {error}"))?;
    }
    fs::rename(&tmp, &dest).map_err(|error| {
        format!(
            "cannot publish git archive {} -> {}: {error}",
            tmp.display(),
            dest.display()
        )
    })?;
    Ok(dest)
}

fn source_from_tarball_bytes(
    url: &str,
    bytes: Vec<u8>,
    source: LockSource,
) -> Result<SourceResolution, String> {
    let mut hasher = sha2::Sha512::new();
    sha2::Digest::update(&mut hasher, &bytes);
    let integrity = format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
    );
    let manifest = manifest_from_tarball(&bytes)?;
    let name = manifest
        .name
        .clone()
        .ok_or_else(|| format!("tarball {url} package.json has no name"))?;
    let version = manifest
        .version
        .clone()
        .ok_or_else(|| format!("tarball {url} package.json has no version"))?;
    Ok(SourceResolution {
        metadata: workspace_metadata(&name, &version, Some(&manifest)),
        resolved: url.to_owned(),
        integrity: Some(integrity),
        source,
        link: false,
        workspace_target: None,
        source_dir: None,
    })
}

fn manifest_from_tarball(bytes: &[u8]) -> Result<PackageManifest, String> {
    let gz = flate2::read::GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(gz);
    let entries = archive
        .entries()
        .map_err(|error| format!("cannot enumerate tarball: {error}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| format!("corrupt tar entry: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("invalid tar entry path: {error}"))?
            .into_owned();
        if path
            .components()
            .next_back()
            .is_some_and(|component| component.as_os_str() == "package.json")
        {
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .map_err(|error| format!("cannot read package.json from tarball: {error}"))?;
            return PackageManifest::from_json(&text, Path::new("package.json"))
                .map_err(|error| error.to_string());
        }
    }
    Err("tarball does not contain package.json".into())
}

fn split_git_reference(spec: &str) -> (String, Option<String>) {
    let stripped = spec.strip_prefix("git+").unwrap_or(spec);
    match stripped.split_once('#') {
        Some((url, reference)) => (url.to_owned(), Some(reference.to_owned())),
        None => (stripped.to_owned(), None),
    }
}

fn looks_like_hosted_git(spec: &str) -> bool {
    let mut parts = spec.split('/');
    matches!((parts.next(), parts.next(), parts.next()), (Some(owner), Some(repo), None) if !owner.is_empty() && !repo.is_empty() && !owner.contains(':') && !repo.contains(':'))
}

fn hosted_git_tarball_url(spec: &str, reference: Option<&str>) -> Option<String> {
    let reference = reference.unwrap_or("HEAD");
    if let Some(rest) = spec
        .strip_prefix("github:")
        .or_else(|| spec.strip_prefix("github.com/"))
    {
        return hosted_tarball("https://codeload.github.com", rest, "tar.gz", reference);
    }
    if let Some(rest) = spec
        .strip_prefix("gitlab:")
        .or_else(|| spec.strip_prefix("gitlab.com/"))
    {
        let (owner, repo) = rest.split_once('/')?;
        return Some(format!(
            "https://gitlab.com/{}/{}/-/archive/{}/{}-{}.tar.gz",
            owner,
            repo,
            reference,
            repo.trim_end_matches(".git"),
            reference
        ));
    }
    if let Some(rest) = spec
        .strip_prefix("bitbucket:")
        .or_else(|| spec.strip_prefix("bitbucket.org/"))
    {
        let (owner, repo) = rest.split_once('/')?;
        return Some(format!(
            "https://bitbucket.org/{}/{}/get/{}.tar.gz",
            owner, repo, reference
        ));
    }
    if looks_like_hosted_git(spec) {
        return hosted_tarball("https://codeload.github.com", spec, "tar.gz", reference);
    }
    None
}

fn hosted_tarball(base: &str, rest: &str, suffix: &str, reference: &str) -> Option<String> {
    let (owner, repo) = rest.split_once('/')?;
    Some(format!(
        "{}/{}/{}/{}/{}",
        base,
        owner,
        repo.trim_end_matches(".git"),
        suffix,
        reference
    ))
}

fn parent_path(path: &str) -> String {
    path.rsplit_once("/node_modules/")
        .map(|(parent, _)| parent.to_owned())
        .unwrap_or_default()
}

fn package_source_for_node(node: &Node, registry: &str) -> crate::resolver::model::PackageSource {
    match &node.source {
        LockSource::Workspace { relative_path } => {
            crate::resolver::model::PackageSource::Workspace {
                relative_path: relative_path.clone(),
            }
        }
        LockSource::Registry { .. }
        | LockSource::File { .. }
        | LockSource::Tarball { .. }
        | LockSource::Git { .. }
        | LockSource::Patch { .. } => crate::resolver::model::PackageSource::Registry {
            registry: registry.to_owned(),
        },
    }
}

fn merged_dependencies(metadata: &VersionMetadata) -> BTreeMap<String, String> {
    let mut dependencies = metadata.dependencies.clone();
    for (name, spec) in &metadata.optional_dependencies {
        dependencies.insert(name.clone(), spec.clone());
    }
    dependencies
}

fn workspace_metadata(
    name: &str,
    version: &str,
    manifest: Option<&PackageManifest>,
) -> VersionMetadata {
    let parsed_version =
        Version::parse(version).expect("workspace versions are validated by the index");
    let Some(manifest) = manifest else {
        return VersionMetadata {
            name: name.to_owned(),
            version: parsed_version,
            deprecated: None,
            dependencies: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
            peer_dependencies_meta: BTreeMap::new(),
            bin: BTreeMap::new(),
            dist: crate::registry::Dist::default(),
            engines: BTreeMap::new(),
            os: Vec::new(),
            cpu: Vec::new(),
            libc: Vec::new(),
            has_install_script: false,
            has_shrinkwrap: false,
        };
    };
    VersionMetadata {
        name: manifest.name.clone().unwrap_or_else(|| name.to_owned()),
        version: parsed_version,
        deprecated: None,
        dependencies: manifest.dependencies.clone(),
        optional_dependencies: manifest.optional_dependencies.clone(),
        peer_dependencies: manifest.peer_dependencies.clone(),
        peer_dependencies_meta: manifest
            .peer_dependencies_meta
            .iter()
            .map(|(name, meta)| {
                (
                    name.clone(),
                    crate::registry::PeerMeta {
                        optional: meta.optional,
                    },
                )
            })
            .collect(),
        bin: manifest_bin(manifest, name),
        dist: crate::registry::Dist::default(),
        engines: manifest.engines.clone(),
        os: manifest.os.clone(),
        cpu: manifest.cpu.clone(),
        libc: manifest.libc.clone(),
        has_install_script: manifest.scripts.keys().any(|script| {
            matches!(
                script.as_str(),
                "preinstall" | "install" | "postinstall" | "prepare"
            )
        }),
        has_shrinkwrap: false,
    }
}

fn manifest_bin(manifest: &PackageManifest, fallback_name: &str) -> BTreeMap<String, String> {
    match &manifest.bin {
        Some(crate::manifest::BinField::Map(entries)) => entries.clone(),
        Some(crate::manifest::BinField::One(path)) => BTreeMap::from([(
            manifest
                .name
                .clone()
                .unwrap_or_else(|| fallback_name.to_owned()),
            path.clone(),
        )]),
        None => BTreeMap::new(),
    }
}

fn request_matches(spec: &str, version: &Version) -> bool {
    let Ok(parsed) = parse_spec(&format!("pkg@{spec}")) else {
        return false;
    };
    match parsed.req {
        crate::registry::VersionRequest::Latest => true,
        crate::registry::VersionRequest::Exact(expected) => expected == *version,
        crate::registry::VersionRequest::Range(range) => range.matches(version),
    }
}

/// Split npm's `npm:target@range` alias syntax while retaining the requested
/// dependency name for physical placement (`node_modules/alias`).
fn registry_request(name: &str, spec: &str) -> (String, String) {
    let Some(alias) = spec.strip_prefix("npm:") else {
        return (name.to_owned(), spec.to_owned());
    };
    match parse_spec(alias) {
        Ok(parsed) => (parsed.name, version_request_to_string(&parsed.req)),
        Err(_) => (name.to_owned(), spec.to_owned()),
    }
}

fn version_request_to_string(request: &crate::registry::VersionRequest) -> String {
    match request {
        crate::registry::VersionRequest::Latest => "latest".to_owned(),
        crate::registry::VersionRequest::Exact(version) => version.to_string(),
        crate::registry::VersionRequest::Range(range) => range.to_string(),
    }
}

/// Return the host as npm's canonical target names.
///
/// This is intentionally small and stable: it is also the default used by
/// the compatibility resolver APIs. Cross-platform callers should pass an
/// explicit target to [`resolve_manifest_with_target`].
pub fn current_target_platform() -> TargetPlatform {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        value => value,
    };
    let cpu = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "x86" => "ia32",
        "aarch64" => "arm64",
        "powerpc64" => "ppc64",
        value => value,
    };
    let libc = if os == "linux" {
        Some(if cfg!(target_env = "musl") {
            "musl".to_owned()
        } else {
            "glibc".to_owned()
        })
    } else {
        None
    };
    TargetPlatform {
        os: os.to_owned(),
        cpu: cpu.to_owned(),
        libc,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn aliases_resolve_target_but_keep_alias_placement() {
        assert_eq!(
            registry_request("alias", "npm:real@^1.2.0"),
            ("real".into(), "^1.2.0".into())
        );
        assert_eq!(
            registry_request("real", "^1.2.0"),
            ("real".into(), "^1.2.0".into())
        );
    }

    #[test]
    fn resolves_transitive_registry_graph_deterministically() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 2048];
                let length = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..length]);
                let body = if request.starts_with("GET /a ") {
                    r#"{"name":"a","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"a","version":"1.0.0","dependencies":{"b":"^1.0.0"},"dist":{"tarball":"/a.tgz","integrity":"sha512-a"}}}}"#
                } else {
                    r#"{"name":"b","dist-tags":{"latest":"1.2.0"},"versions":{"1.2.0":{"name":"b","version":"1.2.0","dist":{"tarball":"/b.tgz","integrity":"sha512-b"}}}}"#
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        let config = crate::config::NpmConfig::default()
            .with_registry_override(&format!("http://{}", address))
            .unwrap();
        let client = RegistryClient::new(config);
        let manifest = PackageManifest::from_json(
            r#"{"name":"app","version":"1.0.0","dependencies":{"a":"*"}}"#,
            std::path::Path::new("package.json"),
        )
        .unwrap();
        let lock = resolve_manifest(&manifest, &client, "test").unwrap();
        server.join().unwrap();
        assert_eq!(
            lock.packages
                .iter()
                .map(|package| package.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(
            lock.resolution.packages["node_modules/a"].dependencies["b"].target,
            "node_modules/a/node_modules/b"
        );
        assert!(lock.to_json().unwrap().contains("\"lockfileVersion\": 2"));
    }

    #[test]
    fn workspace_manifest_dependencies_are_resolved_recursively() {
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("package.json"),
            r#"{"name":"root","version":"1.0.0","workspaces":["packages/*"],"dependencies":{"a":"workspace:*"}}"#,
        )
        .unwrap();
        fs::create_dir_all(project.path().join("packages/a")).unwrap();
        fs::write(
            project.path().join("packages/a/package.json"),
            r#"{"name":"a","version":"1.0.0","dependencies":{"b":"^1.0.0"},"bin":{"a":"cli.js"}}"#,
        )
        .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let length = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..length]);
            assert!(request.starts_with("GET /b "), "{request}");
            let body = r#"{"name":"b","dist-tags":{"latest":"1.2.0"},"versions":{"1.2.0":{"name":"b","version":"1.2.0","dist":{"tarball":"/b.tgz","integrity":"sha512-b"}}}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let config = crate::config::NpmConfig::default()
            .with_registry_override(&format!("http://{}", address))
            .unwrap();
        let client = RegistryClient::new(config);
        let manifest = PackageManifest::from_path(&project.path().join("package.json")).unwrap();
        let layout = crate::workspace::discover(project.path());
        let workspace_index =
            crate::resolver::workspaces::WorkspaceIndex::from_project_root(project.path(), &layout)
                .unwrap();

        let lock =
            resolve_manifest_with_workspaces(&manifest, &client, "test", Some(&workspace_index))
                .unwrap();
        server.join().unwrap();

        assert_eq!(lock.packages.len(), 2);
        let workspace = lock
            .packages
            .iter()
            .find(|package| package.name == "a")
            .unwrap();
        assert!(workspace.link);
        assert_eq!(workspace.dependencies["b"], "^1.0.0");
        assert_eq!(workspace.bin["a"], "cli.js");
        assert_eq!(
            lock.resolution.packages["node_modules/a"].dependencies["b"].target,
            "node_modules/a/node_modules/b"
        );
    }

    #[test]
    fn file_directory_dependency_links_and_traverses_manifest_dependencies() {
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("package.json"),
            r#"{"name":"root","version":"1.0.0","dependencies":{"local":"file:./local"}}"#,
        )
        .unwrap();
        fs::create_dir(project.path().join("local")).unwrap();
        fs::write(
            project.path().join("local/package.json"),
            r#"{"name":"local","version":"1.0.0","dependencies":{"b":"^1.0.0"}}"#,
        )
        .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let length = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..length]);
            assert!(request.starts_with("GET /b "), "{request}");
            let body = r#"{"name":"b","dist-tags":{"latest":"1.2.0"},"versions":{"1.2.0":{"name":"b","version":"1.2.0","dist":{"tarball":"/b.tgz","integrity":"sha512-b"}}}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let config = crate::config::NpmConfig::default()
            .with_registry_override(&format!("http://{}", address))
            .unwrap();
        let client = RegistryClient::new(config);
        let manifest = PackageManifest::from_path(&project.path().join("package.json")).unwrap();
        let lock = resolve_manifest(&manifest, &client, "test").unwrap();
        server.join().unwrap();

        let local = lock
            .packages
            .iter()
            .find(|package| package.name == "local")
            .unwrap();
        assert!(local.link);
        assert_eq!(local.dependencies["b"], "^1.0.0");
        assert!(matches!(
            &lock.resolution.packages["node_modules/local"].source,
            LockSource::File { .. }
        ));
    }

    #[test]
    fn file_tarball_dependency_reads_package_metadata_and_integrity() {
        let project = tempfile::tempdir().unwrap();
        let tarball = project.path().join("pkg.tgz");
        write_test_tgz(
            &tarball,
            r#"{"name":"local-tar","version":"2.0.0","bin":{"lt":"cli.js"}}"#,
        );
        let manifest = serde_json::json!({
            "name": "root",
            "version": "1.0.0",
            "dependencies": {"local-tar": format!("file:{}", tarball.display())},
        });
        fs::write(
            project.path().join("package.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let client = RegistryClient::new(crate::config::NpmConfig::default());
        let manifest = PackageManifest::from_path(&project.path().join("package.json")).unwrap();
        let lock = resolve_manifest(&manifest, &client, "test").unwrap();

        let package = &lock.packages[0];
        assert_eq!(package.name, "local-tar");
        assert_eq!(package.version, "2.0.0");
        assert!(!package.link);
        assert!(package.resolved.starts_with("file://"));
        assert!(package.integrity.as_deref().unwrap().starts_with("sha512-"));
        assert_eq!(package.bin["lt"], "cli.js");
    }

    #[test]
    fn patch_protocol_applies_unified_diff_to_tarball_dependency() {
        let project = tempfile::tempdir().unwrap();
        let tarball = project.path().join("pkg.tgz");
        write_test_tgz(
            &tarball,
            r#"{"name":"local-tar","version":"2.0.0","bin":{"lt":"cli.js"}}"#,
        );
        fs::write(
            project.path().join("fix.patch"),
            "--- a/cli.js\n+++ b/cli.js\n@@ -1 +1 @@\n-console.log(1);\n+console.log(2);\n",
        )
        .unwrap();
        let manifest = serde_json::json!({
            "name": "root",
            "version": "1.0.0",
            "dependencies": {
                "local-tar": format!("patch:file:{}#./fix.patch", tarball.display()),
            },
        });
        fs::write(
            project.path().join("package.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let client = RegistryClient::new(crate::config::NpmConfig::default());
        let manifest = PackageManifest::from_path(&project.path().join("package.json")).unwrap();
        let lock = resolve_manifest(&manifest, &client, "test").unwrap();

        let package = &lock.packages[0];
        assert_eq!(package.name, "local-tar");
        assert!(package.resolved.starts_with("file://"));
        assert!(package.resolved.contains(".bpm"));
        assert!(package.resolved.contains("patches"));
        assert!(matches!(
            &lock.resolution.packages["node_modules/local-tar"].source,
            LockSource::Patch { .. }
        ));
        assert_eq!(
            read_tgz_file(&package.resolved, "package/cli.js"),
            "console.log(2);\n"
        );
    }

    fn write_test_tgz(path: &std::path::Path, package_json: &str) {
        let file = fs::File::create(path).unwrap();
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);
        let mut header = tar::Header::new_gnu();
        header.set_path("package/package.json").unwrap();
        header.set_size(package_json.len() as u64);
        header.set_cksum();
        tar.append(&header, package_json.as_bytes()).unwrap();
        let bytes = b"console.log(1);\n";
        let mut header = tar::Header::new_gnu();
        header.set_path("package/cli.js").unwrap();
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        tar.append(&header, &bytes[..]).unwrap();
        tar.finish().unwrap();
    }

    fn read_tgz_file(url: &str, wanted: &str) -> String {
        let path = url.strip_prefix("file://").unwrap_or(url);
        let file = fs::File::open(path).unwrap();
        let gz = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(gz);
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap().to_string_lossy() == wanted {
                let mut text = String::new();
                entry.read_to_string(&mut text).unwrap();
                return text;
            }
        }
        panic!("missing {wanted} in {path}");
    }
}
