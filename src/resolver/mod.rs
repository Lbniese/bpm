//! Deterministic registry dependency graph resolution.

pub mod model;
pub mod overrides;
pub mod peer;
pub mod platform;
pub mod prepare_graph;
pub use prepare_graph::{build_prepare_closure, PreparedClosure};
pub mod sources;
#[cfg(test)]
pub(crate) use sources::{
    git_clone_url, hosted_git_tarball_url, is_full_git_commit, looks_like_hosted_git,
    reject_git_option_value, resolve_git_source,
};
pub mod workspaces;
pub(crate) use sources::DependencySource;
pub(crate) mod fetch;
pub(crate) mod placement;

use std::collections::BTreeMap;

use semver::Version;
use thiserror::Error;

use crate::integrity::Integrity;
use crate::lockfile::{
    LockDependency, Lockfile, PackageEntry, PackageResolution, RootEntry, RootResolution,
};
use crate::manifest::PackageManifest;
use crate::registry::{parse_spec, RegistryClient, RegistryError, VersionMetadata};

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

/// A resolved package available for download, emitted by the resolver the
/// moment its node is placed during graph expansion.
///
/// Keyed by the resolved install `path`, which is unique and stable across the
/// complete lockfile, so a streaming caller can map results back onto lockfile
/// position after resolution finishes. Best-effort: a download started from a
/// unit is integrity-keyed and idempotent, so it never changes the resolved
/// graph.
#[derive(Clone, Debug)]
pub struct ResolvedDownloadUnit {
    pub path: String,
    pub name: String,
    pub url: String,
    pub integrity: Option<Integrity>,
}

/// Receives each resolved registry-typed package as it is placed during graph
/// expansion, so a caller can overlap downloads with the rest of resolution.
///
/// `emit` may block (bounded consumers apply backpressure) but must not panic:
/// the resolver calls it inline on the resolution thread. A send failure (the
/// consumer dropped its receiver) must be ignored — the resolver continues and
/// still returns the complete, deterministic lockfile; the consumer reports its
/// own errors.
pub trait ResolveSink {
    fn emit(&self, unit: ResolvedDownloadUnit);
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

/// Streaming variant of [`resolve_manifest_with_options`] for the current
/// target platform. See [`resolve_manifest_with_options_and_target_sink`].
pub fn resolve_manifest_with_options_sink(
    manifest: &PackageManifest,
    registry: &RegistryClient,
    generator: &str,
    workspace: Option<&crate::resolver::workspaces::WorkspaceIndex>,
    peer_mode: crate::resolver::peer::PeerMode,
    sink: Option<&dyn ResolveSink>,
) -> Result<Lockfile, ResolveError> {
    resolve_manifest_with_options_and_target_sink(
        manifest,
        registry,
        generator,
        workspace,
        peer_mode,
        current_target_platform(),
        sink,
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
    resolve_manifest_with_options_and_target_sink(
        manifest, registry, generator, workspace, peer_mode, target, None,
    )
}

/// Streaming variant of [`resolve_manifest_with_options_and_target`].
///
/// Each resolved registry-typed package is pushed to `sink` the instant its
/// node is placed, so a caller can begin downloading before the full graph is
/// known. The returned `Lockfile` is byte-for-byte identical to the non-sink
/// variant: streaming only overlaps the *download* of early packages with the
/// *resolution* of later ones, and downloads are integrity-keyed and idempotent.
/// Pass `None` to disable streaming.
pub fn resolve_manifest_with_options_and_target_sink(
    manifest: &PackageManifest,
    registry: &RegistryClient,
    generator: &str,
    workspace: Option<&crate::resolver::workspaces::WorkspaceIndex>,
    peer_mode: crate::resolver::peer::PeerMode,
    target: TargetPlatform,
    sink: Option<&dyn ResolveSink>,
) -> Result<Lockfile, ResolveError> {
    // npm's optionalDependencies take precedence over dependencies with the
    // same name. Peer dependencies are root requests too: npm installs a
    // missing root peer so that packages below the root can bind to it. Keep
    // this merge in one manifest helper so frozen validation and imported
    // lockfiles use exactly the same declaration set.
    let root_deps = manifest.root_dependency_declarations();
    let overrides = crate::resolver::overrides::OverrideSet::from_manifest(
        &manifest.overrides,
        &root_deps,
        crate::resolver::overrides::OverrideOrigin::Root,
    )
    .map_err(|error| ResolveError::Override(error.to_string()))?;
    let normalized_overrides = overrides.as_map().clone();

    let mut resolver = placement::GraphResolver::new(
        fetch::RegistrySource { client: registry },
        overrides,
        workspace,
        manifest.source_dir.clone(),
        target.clone(),
        sink,
    );
    let mut root_targets = BTreeMap::new();

    // ── Batch-prefetch dependency closure before DFS ───────────────────
    // Scan the root manifest for all registry-typed dependency names and
    // prefetch their packuments up to 3 BFS levels.  This separates metadata
    // fetch from graph traversal (pnpm's approach): after this call the
    // in-memory packument cache is warm for the first several levels of the
    // dependency tree, so the resolver's DFS mostly hits cache instead of
    // blocking on the network for each new level's packuments.  The existing
    // per-node prefetch_children calls continue to work for deeper levels.
    //
    // We ignore the returned count here; the batch is purely a warmup.
    let _ = registry.prefetch_batch_closure(&root_deps, 3);

    // Prefetch the first wave of registry packuments so the root requests
    // overlap instead of running strictly depth-first.
    resolver.prefetch_children(&root_deps);
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
pub(crate) fn parent_path(path: &str) -> String {
    path.rsplit_once("/node_modules/")
        .map(|(parent, _)| parent.to_owned())
        .unwrap_or_default()
}

pub(crate) fn merged_dependencies(metadata: &VersionMetadata) -> BTreeMap<String, String> {
    let mut dependencies = metadata.dependencies.clone();
    for (name, spec) in &metadata.optional_dependencies {
        dependencies.insert(name.clone(), spec.clone());
    }
    dependencies
}

pub(crate) fn workspace_metadata(
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

pub(crate) fn request_matches(spec: &str, version: &Version) -> bool {
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
pub(crate) fn registry_request(name: &str, spec: &str) -> (String, String) {
    let Some(alias) = spec.strip_prefix("npm:") else {
        return (name.to_owned(), spec.to_owned());
    };
    match parse_spec(alias) {
        Ok(parsed) => (parsed.name, version_request_to_string(&parsed.req)),
        Err(_) => (name.to_owned(), spec.to_owned()),
    }
}

/// Heuristic: true when `spec` looks like a registry version/range.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn looks_like_registry_spec(spec: &str) -> bool {
    let lower = spec.to_ascii_lowercase();
    !(spec.starts_with("patch:")
        || spec.starts_with("file:")
        || spec.starts_with("link:")
        || lower.starts_with("git+")
        || lower.starts_with("git:")
        || lower.starts_with("github:")
        || lower.starts_with("http:")
        || lower.starts_with("https:")
        || lower.starts_with("npm:")
        || spec.starts_with("workspace:")
        || spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with('/'))
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
    fn git_commit_validation_and_shortcuts_are_deterministic() {
        assert!(is_full_git_commit(
            "0123456789abcdef0123456789abcdef01234567"
        ));
        assert!(!is_full_git_commit("0123456789abcdef"));
        assert!(!is_full_git_commit(
            "0123456789abcdef0123456789abcdef0123456g"
        ));
        assert_eq!(
            git_clone_url("github:owner/repo.git"),
            "https://github.com/owner/repo"
        );
        assert_eq!(
            git_clone_url("file:///tmp/repo.git"),
            "file:///tmp/repo.git"
        );
        assert!(looks_like_hosted_git("https://github.com/owner/repo.git"));
        assert_eq!(
            hosted_git_tarball_url("https://github.com/owner/repo.git", Some("abc123")),
            Some("https://codeload.github.com/owner/repo/tar.gz/abc123".into())
        );
        assert_eq!(
            hosted_git_tarball_url("https://gitlab.com/owner/repo.git", Some("abc123")),
            Some("https://gitlab.com/owner/repo.git/-/archive/abc123/repo-abc123.tar.gz".into())
        );
    }

    #[test]
    fn git_option_values_are_rejected() {
        // References beginning with '-' are rejected — they would be parsed as
        // git options (argument injection). 40-hex SHAs and normal ref names pass.
        assert!(reject_git_option_value("reference", "main").is_ok());
        assert!(reject_git_option_value("reference", "v1.0.0").is_ok());
        assert!(reject_git_option_value("reference", "HEAD").is_ok());
        assert!(reject_git_option_value("reference", "-.upload-pack evil").is_err());
        assert!(reject_git_option_value("reference", "-anything").is_err());
        assert!(reject_git_option_value("reference", "--anything").is_err());

        // Urls are subject to the same rule.
        assert!(reject_git_option_value("url", "https://example/x.git").is_ok());
        assert!(reject_git_option_value("url", "file:///tmp/repo").is_ok());
        assert!(reject_git_option_value("url", "-Otouch").is_err());

        // A `git+-O...` spec reaches `resolve_git_source` with url="-O...".
        // Confirm the entry-point rejection without invoking git.
        assert!(resolve_git_source("-Ofoo", None).is_err());
        assert!(resolve_git_source("https://example/x.git", Some("-UploadPack")).is_err());
    }

    #[test]
    fn git_40_hex_ref_path_is_still_accepted() {
        // The fast path that bypasses ls-remote must keep working for valid SHAs.
        // (This is a regression guard: step 3 must not have rejected the safe path.)
        let sha = "0123456789abcdef0123456789abcdef01234567";
        // resolve_git_source will try network for the hosted case; only assert that
        // the leading-dash guard did not fire for a legitimate 40-hex ref.
        assert!(reject_git_option_value("reference", sha).is_ok());
    }

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
                    r#"{"name":"a","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"a","version":"1.0.0","dependencies":{"b":"^1.0.0"},"dist":{"tarball":"/a.tgz","integrity":"sha512-61000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#
                } else {
                    r#"{"name":"b","dist-tags":{"latest":"1.2.0"},"versions":{"1.2.0":{"name":"b","version":"1.2.0","dist":{"tarball":"/b.tgz","integrity":"sha512-62000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#
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
    fn prefetch_does_not_change_the_resolved_lockfile() {
        // A small fan-out graph (root -> {a, c}; a -> {b, d}; c -> {b, d}) so
        // the resolver's prefetch trigger fires for several registry siblings
        // at once. Resolving it with prefetch disabled must yield a lockfile
        // byte-identical to resolving it with prefetch enabled, run twice to
        // also rule out nondeterminism within the prefetch path itself.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_shutdown = std::sync::Arc::clone(&shutdown);
        let server = thread::spawn(move || {
            // Non-blocking accept: prefetch makes the exact request count and
            // arrival order unpredictable, so the server runs until the test
            // sets the shutdown flag instead of accepting a fixed count.
            listener.set_nonblocking(true).unwrap();
            while !server_shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    Err(_) => break,
                };
                let mut request = [0_u8; 2048];
                let length = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..length]);
                let path = request.lines().next().and_then(|line| {
                    line.strip_prefix("GET /")
                        .and_then(|rest| rest.split(' ').next())
                });
                let body = match path {
                    Some("a") => {
                        r#"{"name":"a","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"a","version":"1.0.0","dependencies":{"b":"^1.0.0","d":"^1.0.0"},"dist":{"tarball":"/a.tgz","integrity":"sha512-61000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#
                    }
                    Some("b") => {
                        r#"{"name":"b","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"b","version":"1.0.0","dist":{"tarball":"/b.tgz","integrity":"sha512-62000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#
                    }
                    Some("c") => {
                        r#"{"name":"c","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"c","version":"1.0.0","dependencies":{"b":"^1.0.0","d":"^1.0.0"},"dist":{"tarball":"/c.tgz","integrity":"sha512-63000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#
                    }
                    Some("d") => {
                        r#"{"name":"d","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"d","version":"1.0.0","dist":{"tarball":"/d.tgz","integrity":"sha512-64000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#
                    }
                    _ => continue,
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
        let manifest = PackageManifest::from_json(
            r#"{"name":"app","version":"1.0.0","dependencies":{"a":"*","c":"*"}}"#,
            std::path::Path::new("package.json"),
        )
        .unwrap();

        let baseline = resolve_manifest(&manifest, &RegistryClient::new(config.clone()), "test")
            .unwrap()
            .to_json()
            .unwrap();
        let with_prefetch_a = resolve_manifest(
            &manifest,
            &RegistryClient::new(config.clone()).with_prefetch(4),
            "test",
        )
        .unwrap()
        .to_json()
        .unwrap();
        let with_prefetch_b = resolve_manifest(
            &manifest,
            &RegistryClient::new(config.clone()).with_prefetch(4),
            "test",
        )
        .unwrap()
        .to_json()
        .unwrap();

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().unwrap();

        assert_eq!(
            baseline, with_prefetch_a,
            "enabling prefetch changed the resolved lockfile"
        );
        assert_eq!(
            with_prefetch_a, with_prefetch_b,
            "prefetch resolution was nondeterministic across runs"
        );
    }

    #[test]
    fn batch_closure_does_not_change_the_resolved_lockfile() {
        // A 3-level-deep graph (root -> a -> b -> c) so the batch-prefetch
        // closure exercises multiple BFS levels.  Resolution with the batch
        // closure enabled must be byte-identical to resolution without it
        // (the closure is purely a cache warmup and must not affect placement
        // or target selection).  We also verify the batch counter is
        // populated, proving the closure actually ran.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_shutdown = std::sync::Arc::clone(&shutdown);
        let server = thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            while !server_shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    Err(_) => break,
                };
                let mut request = [0_u8; 2048];
                let length = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..length]);
                let path = request.lines().next().and_then(|line| {
                    line.strip_prefix("GET /")
                        .and_then(|rest| rest.split(' ').next())
                });
                let body = match path {
                    Some("a") => {
                        r#"{"name":"a","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"a","version":"1.0.0","dependencies":{"b":"^1.0.0"},"dist":{"tarball":"/a.tgz","integrity":"sha512-41000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#
                    }
                    Some("b") => {
                        r#"{"name":"b","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"b","version":"1.0.0","dependencies":{"c":"^1.0.0"},"dist":{"tarball":"/b.tgz","integrity":"sha512-42000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#
                    }
                    Some("c") => {
                        r#"{"name":"c","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"c","version":"1.0.0","dist":{"tarball":"/c.tgz","integrity":"sha512-43000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#
                    }
                    _ => continue,
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
        let manifest = PackageManifest::from_json(
            r#"{"name":"app","version":"1.0.0","dependencies":{"a":"*"}}"#,
            std::path::Path::new("package.json"),
        )
        .unwrap();

        // Baseline: no prefetch, no batch.
        let baseline = resolve_manifest(&manifest, &RegistryClient::new(config.clone()), "test")
            .unwrap()
            .to_json()
            .unwrap();

        // With prefetch enabled — triggers batch closure at 3 BFS levels.
        let client_a = RegistryClient::new(config.clone()).with_prefetch(4);
        let (with_batch_a, batch_a): (String, u64) = {
            let _ = client_a.take_diagnostics();
            let lock = resolve_manifest(&manifest, &client_a, "test")
                .unwrap()
                .to_json()
                .unwrap();
            let diag = client_a.take_diagnostics();
            (lock, diag.batch_prefetch_fetches)
        };
        let client_b = RegistryClient::new(config.clone()).with_prefetch(4);
        let (with_batch_b, batch_b): (String, u64) = {
            let _ = client_b.take_diagnostics();
            let lock = resolve_manifest(&manifest, &client_b, "test")
                .unwrap()
                .to_json()
                .unwrap();
            let diag = client_b.take_diagnostics();
            (lock, diag.batch_prefetch_fetches)
        };

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().unwrap();

        assert_eq!(
            baseline, with_batch_a,
            "batch closure changed the resolved lockfile"
        );
        assert_eq!(
            with_batch_a, with_batch_b,
            "batch closure resolution was nondeterministic across runs"
        );
        // The batch must have fetched all 3 unique packages (a, b, c)
        // across 3 BFS levels.
        assert!(
            batch_a >= 3,
            "batch closure should fetch >=3 packuments, got {batch_a}"
        );
        assert!(
            batch_b >= 3,
            "second batch closure should also fetch >=3 packuments, got {batch_b}"
        );
    }

    #[test]
    fn streaming_sink_emits_every_downloadable_node_and_keeps_the_lockfile_identical() {
        // The sink variant must (a) produce a lockfile byte-identical to the
        // non-sink resolver, and (b) announce exactly the registry-typed nodes
        // that carry a tarball URL, keyed by their resolved install path. Same
        // fan-out graph as the prefetch determinism test.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_shutdown = std::sync::Arc::clone(&shutdown);
        let server = thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            while !server_shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    Err(_) => break,
                };
                let mut request = [0_u8; 2048];
                let length = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..length]);
                let path = request.lines().next().and_then(|line| {
                    line.strip_prefix("GET /")
                        .and_then(|rest| rest.split(' ').next())
                });
                let body = match path {
                    Some("a") => {
                        r#"{"name":"a","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"a","version":"1.0.0","dependencies":{"b":"^1.0.0","d":"^1.0.0"},"dist":{"tarball":"/a.tgz","integrity":"sha512-61000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#
                    }
                    Some("b") => {
                        r#"{"name":"b","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"b","version":"1.0.0","dist":{"tarball":"/b.tgz","integrity":"sha512-62000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#
                    }
                    Some("c") => {
                        r#"{"name":"c","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"c","version":"1.0.0","dependencies":{"b":"^1.0.0","d":"^1.0.0"},"dist":{"tarball":"/c.tgz","integrity":"sha512-63000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#
                    }
                    Some("d") => {
                        r#"{"name":"d","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"d","version":"1.0.0","dist":{"tarball":"/d.tgz","integrity":"sha512-64000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#
                    }
                    _ => continue,
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
        let manifest = PackageManifest::from_json(
            r#"{"name":"app","version":"1.0.0","dependencies":{"a":"*","c":"*"}}"#,
            std::path::Path::new("package.json"),
        )
        .unwrap();

        let baseline =
            resolve_manifest(&manifest, &RegistryClient::new(config.clone()), "test").unwrap();

        struct RecordingSink(std::sync::Mutex<Vec<ResolvedDownloadUnit>>);
        impl ResolveSink for RecordingSink {
            fn emit(&self, unit: ResolvedDownloadUnit) {
                self.0.lock().unwrap().push(unit);
            }
        }
        let sink = RecordingSink(std::sync::Mutex::new(Vec::new()));
        let streamed = resolve_manifest_with_options_sink(
            &manifest,
            &RegistryClient::new(config.clone()).with_prefetch(2),
            "test",
            None,
            crate::resolver::peer::PeerMode::Strict,
            Some(&sink),
        )
        .unwrap();

        shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
        server.join().unwrap();

        assert_eq!(
            baseline.to_json().unwrap(),
            streamed.to_json().unwrap(),
            "streaming sink changed the resolved lockfile"
        );
        // The sink must announce exactly the lockfile's downloadable packages
        // (registry-typed, non-link, with a tarball url), keyed by install path
        // — one announce per physical placement, including deduplicated siblings
        // placed under multiple parents.
        // Compare sink announcements to lockfile packages.  Integrity values
        // stored in the lockfile may be in hex or base64; we canonicalize
        // through Integrity::parse + to_npm_string so both sides use the same
        // canonical base64 form, even when the lockfile was populated from
        // packument fixtures with hex-style values.
        let mut expected: Vec<(String, String, String, Option<String>)> = streamed
            .packages
            .iter()
            .filter(|package| !package.link && !package.resolved.is_empty())
            .map(|package| {
                (
                    package.path.clone(),
                    package.name.clone(),
                    package.resolved.clone(),
                    package
                        .integrity
                        .as_deref()
                        .and_then(|v| Integrity::parse(v).ok())
                        .map(|i| i.to_npm_string()),
                )
            })
            .collect();
        expected.sort();
        let mut emitted: Vec<(String, String, String, Option<String>)> = sink
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|unit| {
                (
                    unit.path.clone(),
                    unit.name.clone(),
                    unit.url.clone(),
                    unit.integrity.as_ref().map(|i| i.to_npm_string()),
                )
            })
            .collect();
        emitted.sort();
        assert_eq!(
            emitted, expected,
            "sink must announce exactly the lockfile's downloadable packages"
        );
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
            let body = r#"{"name":"b","dist-tags":{"latest":"1.2.0"},"versions":{"1.2.0":{"name":"b","version":"1.2.0","dist":{"tarball":"/b.tgz","integrity":"sha512-62000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#;
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
            let body = r#"{"name":"b","dist-tags":{"latest":"1.2.0"},"versions":{"1.2.0":{"name":"b","version":"1.2.0","dist":{"tarball":"/b.tgz","integrity":"sha512-62000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"}}}}"#;
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
            crate::lockfile::LockSource::File { .. }
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
            crate::lockfile::LockSource::Patch { .. }
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
