//! Local dependency mutation: `bpm install foo`, `bpm add foo`, and
//! `bpm remove foo` / `bpm uninstall foo`.
//!
//! Mutation is a single staged transaction. Every target is parsed and the
//! full graph is resolved *before* any project file is written. The manifest
//! is edited losslessly through [`bpm::manifest_edit`], reparsed into a typed
//! [`bpm::manifest::PackageManifest`] for resolution, and published alongside
//! its lock through the crash-bounded two-file publisher. A failure in
//! parsing, target resolution, graph resolution, export, or publishing leaves
//! the source files byte-identical; a later download/materialization/lifecycle
//! failure may leave the already-resolved manifest+lock in place, matching
//! package-manager retry semantics.
//!
//! This first slice supports registry specs only. Git, URL/tarball, `file:`,
//! `link:`, workspace, patch, optional, and peer mutation require separate
//! compatibility work and are rejected before any file is touched.

use std::env;
use std::path::{Path, PathBuf};

use bpm::http::HttpClient;
use bpm::lockfile::{Lockfile, LockfileError, BPM_LOCK_FILE};
use bpm::manifest_edit::{self, DependencySection, ManifestDocument, PublishPlan};
use bpm::npm_lock;
use bpm::project_lock::{find_project_lock, ProjectLockKind};
use bpm::registry::{self, PackageSpec, RegistryError};

use super::fetch::{open_registry_client, store_root};
use super::install;

/// Options for `bpm remove` / `bpm uninstall` / `bpm rm` / `bpm un`.
pub(super) struct UninstallOptions {
    pub names: Vec<String>,
    pub registry: Option<String>,
    pub store: Option<PathBuf>,
    pub concurrency: usize,
    pub json_metrics: Option<PathBuf>,
    pub ignore_scripts: bool,
    pub derived_store: bool,
    pub git_prepare: bool,
    pub legacy_peer_deps: bool,
    pub cache_mode: bpm::metadata_cache::CacheMode,
    /// Optional verified read-through cache endpoint.
    pub remote_cache: Option<String>,
    pub global: bool,
}

/// Run `bpm add`/`bpm install <targets>`: edit package.json, resolve the whole
/// edited graph, write the selected lock, and install.
pub(super) fn run_add(options: &install::Options) -> anyhow::Result<()> {
    let cwd = env::current_dir()?;
    let (project_root, lock_kind) = project_root_and_lock_kind(&cwd)?;

    let manifest_path = project_root.join("package.json");
    if !manifest_path.is_file() {
        anyhow::bail!(
            "no package.json found at {} to edit; `bpm add` mutates a local manifest",
            manifest_path.display()
        );
    }

    // 1. Parse and validate every target as a supported registry spec before
    //    any network or file mutation.
    let mut requests: Vec<TargetRequest> = Vec::with_capacity(options.targets.len());
    for target in &options.targets {
        requests.push(parse_registry_target(target)?);
    }
    reject_duplicate_targets(&requests)?;

    // 2. Load effective npm config and one shared registry client.
    let store_root_path = store_root(options.store.clone())?;
    let config = install::effective_npm_config(&project_root, options.registry.as_deref())?;
    let http = HttpClient::new(config.clone());
    let client = open_registry_client(&store_root_path, config, http.clone(), options.cache_mode)?;

    // 3. Resolve each target to a name/version and compute its save spec.
    let mut additions: Vec<Addition> = Vec::with_capacity(requests.len());
    for request in &requests {
        let resolved = client.resolve(&request.spec).map_err(|error| {
            anyhow::anyhow!(
                "failed to resolve '{target}': {error}",
                target = request.raw
            )
        })?;
        additions.push(Addition {
            name: resolved.name.clone(),
            version: resolved.version.to_string(),
            save_spec: save_spec(request, &resolved.version, options.save_exact),
        });
    }
    reject_duplicate_additions(&additions)?;

    // 4. Edit all requested dependency entries in memory.
    let mut document = ManifestDocument::from_path(&manifest_path)?;
    let section = if options.save_dev {
        DependencySection::Dev
    } else {
        DependencySection::Production
    };
    for addition in &additions {
        document.add_dependency(section, &addition.name, &addition.save_spec)?;
    }
    if !document.changed() {
        eprintln!(
            "nothing to change; package.json already declares {}",
            additions
                .iter()
                .map(|addition| format!("{} {}", addition.name, addition.save_spec))
                .collect::<Vec<_>>()
                .join(", ")
        );
        return Ok(());
    }

    // 5. Reparse the rendered document and resolve the entire edited manifest.
    let manifest = document
        .to_manifest()
        .map_err(|error| anyhow::anyhow!("edited package.json is invalid: {error}"))?;
    let manifest_bytes = document.render();

    let workspace_layout = bpm::workspace::discover(&project_root);
    let workspace_index = bpm::resolver::workspaces::WorkspaceIndex::from_project_root(
        &project_root,
        &workspace_layout,
    )
    .map_err(|error| anyhow::anyhow!("workspace resolution failed: {error}"))?;
    let peer_mode = if options.legacy_peer_deps {
        bpm::resolver::peer::PeerMode::LegacyIgnore
    } else {
        bpm::resolver::peer::PeerMode::Strict
    };
    let lockfile = bpm::resolver::resolve_manifest_with_options(
        &manifest,
        &client,
        "bpm",
        Some(&workspace_index),
        peer_mode,
    )
    .map_err(|error| anyhow::anyhow!("dependency resolution failed: {error}"))?;

    // 6. Serialize the lock according to authority.
    let (lock_path, lock_bytes, resolved_kind) =
        serialize_lock(&project_root, lock_kind, &lockfile, &manifest)?;

    // 7. Publish manifest + lock through the crash-bounded publisher. A
    //    failure here restores both files to their pre-publish state.
    let plan = PublishPlan {
        manifest_path: manifest_path.clone(),
        manifest_bytes,
        lock_path: lock_path.clone(),
        lock_bytes,
    };
    manifest_edit::publish(&plan).map_err(|error| {
        anyhow::anyhow!("failed to publish manifest and lock; both files were restored: {error}")
    })?;
    eprintln!(
        "added {} package(s) and wrote {}",
        additions.len(),
        lock_path.display()
    );

    // 8. Install the in-memory graph through the shared install path. A
    //    failure here may leave the successfully published manifest+lock in
    //    place; the help text below records that boundary.
    let install_options = install::Options {
        targets: Vec::new(),
        frozen: false,
        registry: options.registry.clone(),
        store: options.store.clone(),
        concurrency: options.concurrency,
        json_metrics: options.json_metrics.clone(),
        global: false,
        ignore_scripts: options.ignore_scripts,
        derived_store: options.derived_store,
        git_prepare: options.git_prepare,
        legacy_peer_deps: options.legacy_peer_deps,
        cache_mode: options.cache_mode,
        remote_cache: options.remote_cache.clone(),
        save_dev: false,
        save_exact: false,
    };
    install::install_resolved_lockfile(
        &project_root,
        &lock_path,
        lockfile,
        resolved_kind,
        &install_options,
        &store_root_path,
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "package.json and lock were updated, but installation failed: {error}\n\
             re-run `bpm install` to retry; the manifest and lock are already written"
        )
    })
}

/// Run `bpm remove`/`bpm uninstall`: drop names from every dependency section,
/// resolve, write the selected lock, and install.
pub(super) fn run_uninstall(options: UninstallOptions) -> anyhow::Result<()> {
    let remote_cache = options.remote_cache.clone().or_else(|| {
        env::var_os("BPM_REMOTE_CACHE").map(|path| path.to_string_lossy().into_owned())
    });
    if options.global {
        anyhow::bail!(
            "`bpm remove --global` is not supported: BPM does not yet track which \
             global bin shims it owns, so deleting by filename would be unsafe"
        );
    }
    let cwd = env::current_dir()?;
    let (project_root, lock_kind) = project_root_and_lock_kind(&cwd)?;

    let manifest_path = project_root.join("package.json");
    if !manifest_path.is_file() {
        anyhow::bail!(
            "no package.json found at {} to edit",
            manifest_path.display()
        );
    }

    // Validate every name as an npm package name before editing.
    for name in &options.names {
        if !bpm::manifest::is_valid_package_name(name) {
            anyhow::bail!("'{name}' is not a valid npm package name");
        }
    }

    let mut document = ManifestDocument::from_path(&manifest_path)?;
    let mut removed_any = false;
    for name in &options.names {
        removed_any |= document.remove_dependency(name);
    }
    if !removed_any {
        eprintln!(
            "nothing to remove; package.json does not declare {}",
            options.names.join(", ")
        );
        return Ok(());
    }

    let manifest = document
        .to_manifest()
        .map_err(|error| anyhow::anyhow!("edited package.json is invalid: {error}"))?;
    let manifest_bytes = document.render();

    let store_root_path = store_root(options.store.clone())?;
    let config = install::effective_npm_config(&project_root, options.registry.as_deref())?;
    let http = HttpClient::new(config.clone());
    let client = open_registry_client(&store_root_path, config, http.clone(), options.cache_mode)?;

    let workspace_layout = bpm::workspace::discover(&project_root);
    let workspace_index = bpm::resolver::workspaces::WorkspaceIndex::from_project_root(
        &project_root,
        &workspace_layout,
    )
    .map_err(|error| anyhow::anyhow!("workspace resolution failed: {error}"))?;
    let peer_mode = if options.legacy_peer_deps {
        bpm::resolver::peer::PeerMode::LegacyIgnore
    } else {
        bpm::resolver::peer::PeerMode::Strict
    };
    let lockfile = bpm::resolver::resolve_manifest_with_options(
        &manifest,
        &client,
        "bpm",
        Some(&workspace_index),
        peer_mode,
    )
    .map_err(|error| anyhow::anyhow!("dependency resolution failed: {error}"))?;

    let (lock_path, lock_bytes, resolved_kind) =
        serialize_lock(&project_root, lock_kind, &lockfile, &manifest)?;

    let plan = PublishPlan {
        manifest_path: manifest_path.clone(),
        manifest_bytes,
        lock_path: lock_path.clone(),
        lock_bytes,
    };
    manifest_edit::publish(&plan).map_err(|error| {
        anyhow::anyhow!("failed to publish manifest and lock; both files were restored: {error}")
    })?;
    eprintln!("removed {} package(s)", options.names.len());

    let install_options = install::Options {
        targets: Vec::new(),
        frozen: false,
        registry: options.registry.clone(),
        store: options.store.clone(),
        concurrency: options.concurrency,
        json_metrics: options.json_metrics.clone(),
        global: false,
        ignore_scripts: options.ignore_scripts,
        derived_store: options.derived_store,
        git_prepare: options.git_prepare,
        legacy_peer_deps: options.legacy_peer_deps,
        cache_mode: options.cache_mode,
        remote_cache,
        save_dev: false,
        save_exact: false,
    };
    install::install_resolved_lockfile(
        &project_root,
        &lock_path,
        lockfile,
        resolved_kind,
        &install_options,
        &store_root_path,
    )
    .map_err(|error| {
        anyhow::anyhow!(
            "package.json and lock were updated, but installation failed: {error}\n\
             re-run `bpm install` to retry; the manifest and lock are already written"
        )
    })
}

/// Locate the project root and the selected lock kind (Plan 002 precedence) for
/// a mutation. Returns `(root, kind)` where `kind` is `None` when no lock
/// exists yet (the mutation creates a `bpm.lock`).
fn project_root_and_lock_kind(cwd: &Path) -> anyhow::Result<(PathBuf, Option<ProjectLockKind>)> {
    match find_project_lock(cwd)? {
        Some(lock) => Ok((lock.project_root.clone(), Some(lock.kind))),
        None => Ok((
            bpm::project::find_project_root(cwd).unwrap_or_else(|_| cwd.to_path_buf()),
            None,
        )),
    }
}

/// Serialize the resolved lockfile according to the project's lock authority.
/// A `bpm.lock` project (or a project with no lock yet) gets canonical BPM
/// bytes; a `package-lock.json` v3 project gets strict npm v3 export bytes.
fn serialize_lock(
    project_root: &Path,
    lock_kind: Option<ProjectLockKind>,
    lockfile: &Lockfile,
    manifest: &bpm::manifest::PackageManifest,
) -> anyhow::Result<(PathBuf, Vec<u8>, ProjectLockKind)> {
    match lock_kind {
        Some(ProjectLockKind::NpmV3) => {
            let path = project_root.join(bpm::project_lock::NPM_PACKAGE_LOCK_FILE);
            let bytes = npm_lock::export_v3(lockfile, manifest)
                .map_err(|error| anyhow::anyhow!("cannot export package-lock.json v3: {error}"))?;
            Ok((path, bytes, ProjectLockKind::NpmV3))
        }
        Some(ProjectLockKind::Bpm) | None => {
            let path = project_root.join(BPM_LOCK_FILE);
            let json = lockfile.to_json().map_err(|error| {
                anyhow::anyhow!("cannot serialize bpm.lock: {}", lock_error(&error))
            })?;
            Ok((path, json.into_bytes(), ProjectLockKind::Bpm))
        }
    }
}

fn lock_error(error: &LockfileError) -> String {
    match error {
        LockfileError::Write { source, .. } => source.to_string(),
        other => other.to_string(),
    }
}

struct TargetRequest {
    raw: String,
    spec: PackageSpec,
    /// The user's literal version-request substring, when present. `None` for
    /// a bare name or `@latest`.
    raw_request: Option<String>,
}

/// Parse and validate one `add` target as a registry-only spec.
fn parse_registry_target(target: &str) -> anyhow::Result<TargetRequest> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        anyhow::bail!("cannot add an empty package spec");
    }
    if looks_non_registry(trimmed) {
        anyhow::bail!(
            "only registry package specs are supported for local add in this slice; \
             '{target}' looks like a URL, path, Git, file, link, or workspace reference"
        );
    }
    let spec = registry::parse_spec(trimmed).map_err(|error| {
        anyhow::anyhow!(
            "cannot add '{target}': only registry package specs are supported in this slice ({})",
            registry_error(&error)
        )
    })?;
    let raw_request = match trimmed.rfind('@') {
        Some(0) | None => None,
        Some(index) => {
            let request = &trimmed[index + 1..];
            if request.is_empty() || request == "latest" {
                None
            } else {
                Some(request.to_string())
            }
        }
    };
    Ok(TargetRequest {
        raw: target.to_string(),
        spec,
        raw_request,
    })
}

fn looks_non_registry(target: &str) -> bool {
    target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("file:")
        || target.starts_with("git+")
        || target.starts_with("git:")
        || target.starts_with("link:")
        || target.starts_with("workspace:")
        || target.starts_with("./")
        || target.starts_with("../")
        || target.starts_with('/')
        || target.contains("://")
}

/// Reject duplicate package names with conflicting requests before resolution.
fn reject_duplicate_targets(requests: &[TargetRequest]) -> anyhow::Result<()> {
    use std::collections::BTreeMap;
    let mut by_name: BTreeMap<&str, &TargetRequest> = BTreeMap::new();
    for request in requests {
        if let Some(existing) = by_name.get(request.spec.name.as_str()) {
            if existing.raw_request != request.raw_request {
                anyhow::bail!(
                    "conflicting requests for package '{}': '{}' vs '{}'",
                    request.spec.name,
                    existing.raw,
                    request.raw
                );
            }
        } else {
            by_name.insert(request.spec.name.as_str(), request);
        }
    }
    Ok(())
}

/// Reject two `add` targets that resolved to the same name with different
/// resolved versions.
fn reject_duplicate_additions(additions: &[Addition]) -> anyhow::Result<()> {
    use std::collections::BTreeMap;
    let mut by_name: BTreeMap<&str, &Addition> = BTreeMap::new();
    for addition in additions {
        if let Some(existing) = by_name.get(addition.name.as_str()) {
            if existing.version != addition.version {
                anyhow::bail!(
                    "package '{}' resolved to two different versions ({} vs {})",
                    addition.name,
                    existing.version,
                    addition.version
                );
            }
        } else {
            by_name.insert(addition.name.as_str(), addition);
        }
    }
    Ok(())
}

struct Addition {
    name: String,
    version: String,
    save_spec: String,
}

/// Apply the plan's save-spec rules to one resolved target:
///   - `--save-exact` → `X.Y.Z`;
///   - explicit supported range (`^`, `~`, `>`, `<`, `=`, `*`) → preserve;
///   - bare name, `@latest`, or exact version without `--save-exact` → `^X.Y.Z`.
fn save_spec(request: &TargetRequest, version: &semver::Version, save_exact: bool) -> String {
    if save_exact {
        return version.to_string();
    }
    if let Some(raw) = &request.raw_request {
        if raw.starts_with(['^', '~', '>', '<', '=', '*']) {
            return raw.clone();
        }
    }
    format!("^{}", version)
}

fn registry_error(error: &RegistryError) -> String {
    error.to_string()
}
