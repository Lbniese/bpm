//! Project-local materialization of a resolved dependency graph.
//!
//! Given a project root, the artifact store, and the set of packages that have
//! been fetched+extracted (each paired with its [`ArtifactId`]), produce the
//! `node_modules` tree by symlinking each package's lockfile path to its store
//! image. This reproduces npm's v3 layout 1:1 ("compatible" mode): every
//! `node_modules/<path>` placement from `bpm.lock` becomes a symlink into the
//! global, immutable, content-addressed store.
//!
//! Executables are exposed under `node_modules/.bin/<name>` as *relative*
//! symlinks into the package directory, so the `.bin` tree is portable within
//! the project (the package symlinks themselves point at the absolute store
//! path, matching pnpm's model).
//!
//! Determinism (IMPLEMENTATION §6): iteration follows the caller-supplied
//! `resolved` slice, which the installer builds in `bpm.lock` package order
//! (already sorted by path). No hash-map iteration is involved, so two runs
//! over the same lockfile produce byte-identical `node_modules` trees.
//!
//! Idempotency: a correct existing symlink is left in place; a stale or
//! conflicting entry is replaced. Re-running `bpm install --frozen` is a no-op
//! on the filesystem when nothing changed.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::integrity::ArtifactId;
use crate::path_safety::{validate_bin_name, validate_bin_target, validate_package_path, validate_workspace_target};
use crate::lockfile::{Lockfile, PackageEntry};
use crate::store::ArtifactStore;

/// Default permission bits applied to a linked bin (owner rwx, group rx, other rx).
#[cfg(unix)]
const BIN_MODE: u32 = 0o755;

/// Counters returned by [`materialize`] for human + JSON reporting.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MaterializeStats {
    /// Non-link packages whose `node_modules` symlink was created/confirmed.
    pub packages_materialized: usize,
    /// Distinct `node_modules/.bin/<name>` links created/confirmed.
    pub bins_linked: usize,
    /// Bin names that collided with an already-linked bin and were skipped.
    pub bins_collisions: usize,
    /// Entries skipped because they are workspace/link entries or unresolved.
    pub links_skipped: usize,
}

/// Materialization visibility policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializeMode {
    /// Accept the resolver's npm-v3-compatible placement as authoritative.
    Compatible,
    /// Require every placement to be reachable through an explicit lock edge.
    Strict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializeBackend {
    Symlink,
    Hardlink,
    Auto,
}

#[derive(Debug, Error)]
pub enum MaterializeError {
    #[error("io error materializing {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("symlinks are required for node_modules materialization but are unsupported on this platform")]
    SymlinksUnsupported,
    #[error("unsafe package path \"{path}\": {detail}")]
    UnsafePackagePath {
        path: String,
        detail: String,
    },
    #[error("unsafe bin name for {package}: \"{name}\": {detail}")]
    UnsafeBinName {
        package: String,
        name: String,
        detail: String,
    },
    #[error("unsafe bin target for {package}: \"{target}\": {detail}")]
    UnsafeBinTarget {
        package: String,
        target: String,
        detail: String,
    },
}

/// Materialize `resolved` packages under `project_root`, symlinking each
/// package's lockfile path to its store image and linking declared bins.
///
/// `resolved` pairs a borrowed [`PackageEntry`] with the [`ArtifactId`] of its
/// already-extracted image. Entries are processed in the given order; the
/// installer passes them in `bpm.lock` (path-sorted) order for determinism.
///
/// - Entries with `link == true` or an empty `resolved` are skipped
///   (`links_skipped`), since they are workspace/file links or unresolved.
/// - For every other entry, `project_root / entry.path` becomes a symlink to
///   `store.image_path(id)` (created or confirmed; stale targets replaced).
/// - Each declared `bin` becomes a relative `node_modules/.bin/<name>` symlink
///   into the package directory; the first declarant wins on collision (later
///   ones are skipped with a warning via the returned stats).
pub fn materialize(
    project_root: &Path,
    store: &ArtifactStore,
    resolved: &[(&PackageEntry, ArtifactId)],
) -> Result<MaterializeStats, MaterializeError> {
    materialize_with_backend(project_root, store, resolved, MaterializeBackend::Auto)
}

fn preflight_resolved(resolved: &[(&PackageEntry, ArtifactId)]) -> Result<(), MaterializeError> {
    for (entry, _) in resolved {
        // Link/workspace packages may have paths outside node_modules.
        if !entry.link {
            validate_package_path(&entry.path).map_err(|e| {
                MaterializeError::UnsafePackagePath {
                    path: entry.path.clone(),
                    detail: e.to_string(),
                }
            })?;
        }
        if let Some(ref target) = entry.workspace_target {
            validate_workspace_target(target).map_err(|e| {
                MaterializeError::UnsafePackagePath {
                    path: entry.path.clone(),
                    detail: format!("workspace target {target:?}: {e}"),
                }
            })?;
        }
        for (name, target) in &entry.bin {
            validate_bin_name(name).map_err(|e| {
                MaterializeError::UnsafeBinName {
                    package: entry.path.clone(),
                    name: name.clone(),
                    detail: e.to_string(),
                }
            })?;
            validate_bin_target(target).map_err(|e| {
                MaterializeError::UnsafeBinTarget {
                    package: entry.path.clone(),
                    target: target.clone(),
                    detail: e.to_string(),
                }
            })?;
        }
    }
    Ok(())
}

pub fn materialize_with_backend(
    project_root: &Path,
    store: &ArtifactStore,
    resolved: &[(&PackageEntry, ArtifactId)],
    backend: MaterializeBackend,
) -> Result<MaterializeStats, MaterializeError> {
    preflight_resolved(resolved)?;
    let mut stats = MaterializeStats::default();
    // Names already linked into node_modules/.bin, for deterministic collision
    // reporting (first declarant wins).
    let mut linked_bins: BTreeSet<String> = BTreeSet::new();

    for (entry, id) in resolved {
        if entry.link {
            if let Some(relative) = entry.workspace_target.as_deref() {
                let target = project_root.join(relative);
                link_path(&project_root.join(&entry.path), &target)?;
            }
            stats.links_skipped += 1;
            continue;
        }
        if entry.resolved.is_empty() {
            stats.links_skipped += 1;
            continue;
        }

        let image_dir = store.image_path(id);
        let target = project_root.join(&entry.path);
        let symlink_bins = match backend {
            MaterializeBackend::Symlink => {
                link_path(&target, &image_dir)?;
                true
            }
            MaterializeBackend::Hardlink => {
                hardlink_tree(&image_dir, &target)?;
                // A hardlinked executable loses the package-relative path that
                // Node uses for relative requires (Next's bin is a concrete
                // example: it requires ../server/require-hook). Keep .bin
                // entries as relative symlinks into the hardlinked package
                // tree; only package files themselves are hardlinked.
                cfg!(unix)
            }
            MaterializeBackend::Auto => {
                if let Err(error) = link_path(&target, &image_dir) {
                    if !matches!(error, MaterializeError::SymlinksUnsupported) {
                        return Err(error);
                    }
                    hardlink_tree(&image_dir, &target)?;
                    false
                } else {
                    true
                }
            }
        };
        stats.packages_materialized += 1;

        if !entry.bin.is_empty() {
            link_bins(
                project_root,
                &entry.path,
                &image_dir,
                &entry.bin,
                &mut linked_bins,
                &mut stats,
                symlink_bins,
            )?;
        }
    }

    Ok(stats)
}

pub(crate) fn hardlink_tree(source: &Path, target: &Path) -> Result<(), MaterializeError> {
    if target.exists() || symlink_exists(target) {
        remove_any(target)?;
    }
    let index_path = source.with_extension("bpi");
    if index_path.is_file() {
        return hardlink_tree_from_index(source, target, &index_path);
    }
    hardlink_tree_by_walking_directory(source, target)
}

fn hardlink_tree_by_walking_directory(
    source: &Path,
    target: &Path,
) -> Result<(), MaterializeError> {
    fs::create_dir_all(target).map_err(|source| io_err(target, source))?;
    for item in fs::read_dir(source).map_err(|error| io_err(source, error))? {
        let item = item.map_err(|error| io_err(source, error))?;
        let from = item.path();
        let to = target.join(item.file_name());
        if item
            .file_type()
            .map_err(|source| io_err(&from, source))?
            .is_dir()
        {
            hardlink_tree_by_walking_directory(&from, &to)?;
        } else if item
            .file_type()
            .map_err(|source| io_err(&from, source))?
            .is_file()
        {
            hardlink_or_copy_file(&from, &to)?;
        } else if item
            .file_type()
            .map_err(|source| io_err(&from, source))?
            .is_symlink()
        {
            let link = fs::read_link(&from).map_err(|source| io_err(&from, source))?;
            make_symlink(&link, &to)?;
        }
    }
    Ok(())
}

fn hardlink_tree_from_index(
    source: &Path,
    target: &Path,
    index_path: &Path,
) -> Result<(), MaterializeError> {
    let bytes = fs::read(index_path).map_err(|source| io_err(index_path, source))?;
    let entries = crate::package_image::decode(&bytes).map_err(|error| MaterializeError::Io {
        path: index_path.display().to_string(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()),
    })?;
    fs::create_dir_all(target).map_err(|source| io_err(target, source))?;
    for entry in entries {
        match entry {
            crate::package_image::Entry::File { path, .. } => {
                let from = safe_relative_join(source, &path)?;
                let to = safe_relative_join(target, &path)?;
                hardlink_or_copy_file(&from, &to)?;
            }
            crate::package_image::Entry::Symlink { path, target: link } => {
                let to = safe_relative_join(target, &path)?;
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent).map_err(|source| io_err(parent, source))?;
                }
                make_symlink(Path::new(&link), &to)?;
            }
        }
    }
    Ok(())
}

fn hardlink_or_copy_file(from: &Path, to: &Path) -> Result<(), MaterializeError> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|source| io_err(parent, source))?;
    }
    fs::hard_link(from, to)
        .or_else(|_| fs::copy(from, to).map(|_| ()))
        .map_err(|source| io_err(to, source))
}

/// Validate and materialize a complete lockfile using the selected visibility
/// policy. Strict mode prevents accidental hoisting from exposing packages
/// that no dependency is allowed to see.
pub fn materialize_lockfile(
    project_root: &Path,
    store: &ArtifactStore,
    lockfile: &Lockfile,
    artifact_ids: &[Option<ArtifactId>],
    mode: MaterializeMode,
) -> Result<MaterializeStats, MaterializeError> {
    materialize_lockfile_with_backend(
        project_root,
        store,
        lockfile,
        artifact_ids,
        mode,
        MaterializeBackend::Auto,
    )
}

/// Materialize a lockfile with an explicit package-image backend.
///
/// Next.js and similar tools resolve dependency paths after Node canonicalizes
/// symlinks. A hardlink view keeps those canonical paths inside the project,
/// while the default backend preserves the cheaper symlink view for ordinary
/// projects.
pub fn materialize_lockfile_with_backend(
    project_root: &Path,
    store: &ArtifactStore,
    lockfile: &Lockfile,
    artifact_ids: &[Option<ArtifactId>],
    mode: MaterializeMode,
    backend: MaterializeBackend,
) -> Result<MaterializeStats, MaterializeError> {
    if mode == MaterializeMode::Strict {
        validate_strict_layout(lockfile).map_err(|message| MaterializeError::Io {
            path: project_root.display().to_string(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, message),
        })?;
    }
    let mut workspace_links = 0;
    for package in lockfile.packages.iter().filter(|package| package.link) {
        if let Some(relative) = package.workspace_target.as_deref() {
            link_path(
                &project_root.join(&package.path),
                &project_root.join(relative),
            )?;
            workspace_links += 1;
        }
    }
    let resolved = artifact_ids
        .iter()
        .zip(&lockfile.packages)
        .filter_map(|(id, package)| id.map(|id| (package, id)))
        .collect::<Vec<_>>();
    let mut stats = materialize_with_backend(project_root, store, &resolved, backend)?;
    stats.links_skipped += workspace_links;
    Ok(stats)
}

fn validate_strict_layout(lockfile: &Lockfile) -> Result<(), String> {
    let package_paths = lockfile
        .packages
        .iter()
        .map(|package| package.path.as_str())
        .collect::<BTreeSet<_>>();
    for package in &lockfile.packages {
        let Some((parent, name)) = package.path.rsplit_once("/node_modules/") else {
            let expected = format!("node_modules/{}", package.name);
            if package.path != expected {
                return Err(format!(
                    "package {} has invalid root placement {}",
                    package.name, package.path
                ));
            }
            if !lockfile.root.dependencies.contains_key(&package.name) {
                return Err(format!(
                    "package {} is hoisted without a root dependency",
                    package.path
                ));
            }
            continue;
        };
        let parent_entry = lockfile
            .packages
            .iter()
            .find(|candidate| candidate.path == parent)
            .ok_or_else(|| format!("package {} has missing parent {}", package.path, parent))?;
        let dependency = lockfile
            .resolution
            .packages
            .get(parent)
            .and_then(|resolution| resolution.dependencies.get(name))
            .ok_or_else(|| format!("package {} is not declared by {}", package.path, parent))?;
        if dependency.target != package.path {
            return Err(format!(
                "dependency {} from {} targets {}, not {}",
                name, parent, dependency.target, package.path
            ));
        }
        if !package_paths.contains(dependency.target.as_str()) || parent_entry.name.is_empty() {
            return Err(format!(
                "package {} has an invalid strict dependency target",
                package.path
            ));
        }
    }
    Ok(())
}

/// Point `link` at `target`, creating parent dirs and replacing any stale entry.
/// A correct existing symlink is left untouched (idempotent).
fn link_path(link: &Path, target: &Path) -> Result<(), MaterializeError> {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).map_err(|source| io_err(parent, source))?;
    }
    if let Some(existing) = read_link_if_points_to(link, target)? {
        // Already correct; nothing to do. `existing` is discarded.
        let _ = existing;
        return Ok(());
    }
    // Remove whatever is at `link` (stale symlink, file, or dir) before re-creating.
    remove_any(link)?;
    make_symlink(target, link)
}

/// Record each declared bin as a relative `node_modules/.bin/<name>` link.
fn link_bins(
    project_root: &Path,
    pkg_path: &str,
    image_dir: &Path,
    bins: &std::collections::BTreeMap<String, String>,
    linked_bins: &mut BTreeSet<String>,
    stats: &mut MaterializeStats,
    symlink_bins: bool,
) -> Result<(), MaterializeError> {
    let bin_dir = project_root.join("node_modules").join(".bin");
    #[cfg(windows)]
    let _ = symlink_bins;
    fs::create_dir_all(&bin_dir).map_err(|source| io_err(&bin_dir, source))?;

    for (name, relpath) in bins {
        if linked_bins.contains(name) {
            stats.bins_collisions += 1;
            eprintln!(
                "warning: bin '{}' already linked; keeping the first declarant (skipping bin from {})",
                name, pkg_path
            );
            continue;
        }
        // Windows cannot assume symlink privileges. Publish the conventional
        // pair of shims as one owned bin; a collision in either file keeps the
        // whole pair untouched.
        #[cfg(windows)]
        {
            validate_windows_bin_name(name)?;
            let cmd_path = bin_dir.join(format!("{name}.cmd"));
            let ps1_path = bin_dir.join(format!("{name}.ps1"));
            let rel_target = relative_bin_target(pkg_path, relpath).replace('/', "\\");
            let cmd = windows_cmd_shim(&rel_target);
            let ps1 = windows_ps1_shim(&rel_target);
            let matches = fs::read(&cmd_path).ok().as_deref() == Some(cmd.as_slice())
                && fs::read(&ps1_path).ok().as_deref() == Some(ps1.as_slice());
            if matches {
                linked_bins.insert(name.clone());
                stats.bins_linked += 1;
                continue;
            }
            if cmd_path.exists() || ps1_path.exists() {
                stats.bins_collisions += 1;
                linked_bins.insert(name.clone());
                continue;
            }
            ensure_executable(image_dir, relpath);
            fs::write(&cmd_path, cmd).map_err(|source| io_err(&cmd_path, source))?;
            if let Err(source) = fs::write(&ps1_path, ps1) {
                let _ = fs::remove_file(&cmd_path);
                return Err(io_err(&ps1_path, source));
            }
            linked_bins.insert(name.clone());
            stats.bins_linked += 1;
            continue;
        }
        #[cfg(not(windows))]
        {
            let link = bin_dir.join(name);
            // Relative target from node_modules/.bin/ to <pkg_path>/<relpath>.
            let rel_target = relative_bin_target(pkg_path, relpath);
            let materialized_bin = project_root.join(pkg_path).join(strip_dot_slash(relpath));
            if read_link_if_points_to(&link, Path::new(&rel_target))?.is_some()
                || (!symlink_bins && same_file(&link, &materialized_bin))
            {
                linked_bins.insert(name.clone());
                stats.bins_linked += 1;
                continue;
            }
            if link.exists() || symlink_exists(&link) {
                stats.bins_collisions += 1;
                eprintln!(
                    "warning: bin '{}' already present at {}; keeping the existing link",
                    name,
                    link.display()
                );
                linked_bins.insert(name.clone());
                continue;
            }
            ensure_executable(image_dir, relpath);
            if symlink_bins {
                make_symlink(Path::new(&rel_target), &link)?;
            } else {
                hardlink_or_copy_file(&materialized_bin, &link)?;
            }
            linked_bins.insert(name.clone());
            stats.bins_linked += 1;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn validate_windows_bin_name(name: &str) -> Result<(), MaterializeError> {
    let upper = name.trim_end_matches([' ', '.']).to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"];
    if name.is_empty()
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
        || name.contains(['<', '>', '"', '|', '?', '*'])
        || reserved.contains(&upper.as_str())
    {
        return Err(MaterializeError::Io {
            path: name.to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid Windows bin name",
            ),
        });
    }
    Ok(())
}

#[cfg(windows)]
fn windows_cmd_shim(relative_target: &str) -> Vec<u8> {
    format!("@echo off\r\nnode \"%~dp0\\{}\" %*\r\n", relative_target).into_bytes()
}

#[cfg(windows)]
fn windows_ps1_shim(relative_target: &str) -> Vec<u8> {
    format!("$ErrorActionPreference = 'Stop'\r\n& node (Join-Path $PSScriptRoot '{}') @args\r\nexit $LASTEXITCODE\r\n", relative_target.replace('\\', "/")).into_bytes()
}

/// Compute a relative path from `node_modules/.bin/` to `<pkg_path>/<relpath>`,
/// e.g. `node_modules/foo` + `cli.js` -> `../foo/cli.js`, or
/// `node_modules/a/node_modules/b` + `./cli.js` -> `../a/node_modules/b/cli.js`.
fn relative_bin_target(pkg_path: &str, relpath: &str) -> String {
    let from = Path::new("node_modules").join(".bin");
    let to = Path::new(pkg_path).join(strip_dot_slash(relpath));
    relative_path(&from, &to).unwrap_or_else(|| to.to_string_lossy().into_owned())
}

/// Lexical relative path from directory `from` to `to`, both relative.
/// Returns `../foo/cli.js`-style output. Returns `None` if either path is not
/// purely relative (contains an absolute/root/prefix component).
fn relative_path(from: &Path, to: &Path) -> Option<String> {
    // Collect only Normal components, bailing to None on any non-relative
    // component (CurDir is allowed and dropped). A root or prefix component
    // means the path is not project-relative and we cannot compute a relative
    // target for it.
    let from_comps = clean_components(from)?;
    let to_comps = clean_components(to)?;

    let common = from_comps
        .iter()
        .zip(to_comps.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let ups = from_comps.len().saturating_sub(common);
    let mut parts: Vec<String> = std::iter::repeat_n("..".to_string(), ups).collect();
    for c in &to_comps[common..] {
        parts.push(c.to_string_lossy().into_owned());
    }
    if parts.is_empty() {
        Some(".".to_string())
    } else {
        Some(parts.join("/"))
    }
}

/// Normal components of a relative `path`, or `None` if it carries any
/// absolute/root/prefix/parent component.
fn clean_components(path: &Path) -> Option<Vec<&std::ffi::OsStr>> {
    let mut out = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            // Any absolute, parent, or prefix component aborts.
            _ => return None,
        }
    }
    Some(out)
}

/// Drop a leading `./` from a package-relative path (`./cli.js` -> `cli.js`).
fn strip_dot_slash(p: &str) -> &str {
    p.strip_prefix("./").unwrap_or(p)
}

/// If `link` is a symlink whose target equals `expected`, return `Some(link)`.
/// Returns `Ok(None)` when the link is absent or points elsewhere. Uses
/// symlink metadata so it never follows the link.
fn read_link_if_points_to(
    link: &Path,
    expected: &Path,
) -> Result<Option<PathBuf>, MaterializeError> {
    if !symlink_exists(link) {
        return Ok(None);
    }
    match fs::read_link(link) {
        Ok(actual) if same_path(&actual, expected) => Ok(Some(actual)),
        Ok(_) => Ok(None),
        Err(source) => Err(io_err(link, source)),
    }
}

/// `true` if a symlink (or any entry) exists at `path` without following links.
fn symlink_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Compare two paths component-wise (so `foo/bar` == `foo/bar` regardless of
/// trailing separators or OS separator flavor).
fn same_path(a: &Path, b: &Path) -> bool {
    a.components().eq(b.components())
}

fn same_file(a: &Path, b: &Path) -> bool {
    let (Ok(a), Ok(b)) = (fs::metadata(a), fs::metadata(b)) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(not(unix))]
    {
        a.len() == b.len()
    }
}

fn safe_relative_join(root: &Path, relative: &str) -> Result<PathBuf, MaterializeError> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(MaterializeError::Io {
            path: root.join(relative).display().to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unsafe package image path {relative}"),
            ),
        });
    }
    Ok(root.join(path))
}

/// Remove a file, symlink, or directory tree at `path` (best-effort).
fn remove_any(path: &Path) -> Result<(), MaterializeError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(io_err(path, source)),
    };
    let r = if meta.is_dir() {
        // remove_dir_all on a symlink to a dir would recurse into the target;
        // symlink_metadata reports is_dir()=false for symlinks, so a true
        // directory is only removed when it is a real (stale) directory.
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    r.map_err(|source| io_err(path, source))
}

/// Create a symlink `link -> target`. On non-unix, returns
/// [`MaterializeError::SymlinksUnsupported`].
fn make_symlink(target: &Path, link: &Path) -> Result<(), MaterializeError> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).map_err(|source| io_err(link, source))
    }
    #[cfg(not(unix))]
    {
        let _ = (target, link);
        Err(MaterializeError::SymlinksUnsupported)
    }
}

/// Best-effort `chmod BIN_MODE` on the resolved bin file inside the image.
/// Failures (missing file, read-only store) are swallowed: bins link
/// regardless; exec-ability depends on the archive's own modes in that case.
#[cfg(unix)]
fn ensure_executable(image_dir: &Path, relpath: &str) {
    use std::os::unix::fs::PermissionsExt;
    let file = image_dir.join(strip_dot_slash(relpath));
    if let Ok(meta) = fs::metadata(&file) {
        let perms = meta.permissions().mode();
        if perms & 0o111 != 0o111 {
            let _ = fs::set_permissions(&file, fs::Permissions::from_mode(BIN_MODE));
        }
    }
}

#[cfg(not(unix))]
fn ensure_executable(_image_dir: &Path, _relpath: &str) {}

fn io_err(path: &Path, source: std::io::Error) -> MaterializeError {
    MaterializeError::Io {
        path: path.display().to_string(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::integrity::Integrity;
    use crate::integrity::Sha512Digest;
    use crate::lockfile::{Lockfile, PackageEntry, RootEntry};
    #[cfg(unix)]
    use crate::metrics::Metrics;
    use crate::store::ArtifactStore;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    /// Build an npm-style gzip+tar with `package/<rel>` entries, returning the
    /// raw bytes (mirrors `tests/common`, kept local so the module is unit-test
    /// self-contained).
    #[cfg(unix)]
    fn build_pkg_tgz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(enc);
        let mut dir_header = tar::Header::new_gnu();
        dir_header.set_path("package").unwrap();
        dir_header.set_entry_type(tar::EntryType::Directory);
        dir_header.set_size(0);
        dir_header.set_mode(0o755);
        dir_header.set_cksum();
        builder.append(&dir_header, &[][..]).unwrap();
        for &(path, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_path(path).unwrap();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            builder.append(&h, data).unwrap();
        }
        let enc = builder.into_inner().unwrap();
        enc.finish().unwrap()
    }

    /// Push a package into the store (download via file://, verify, extract),
    /// returning its ArtifactId and the store image path.
    #[cfg(unix)]
    fn stage_package(store: &ArtifactStore, tgz: &[u8], tmp_src: &Path, n: usize) -> ArtifactId {
        let id = Sha512Digest::hash_bytes(tgz);
        let src = tmp_src.join(format!("pkg-{n}.tgz"));
        fs::write(&src, tgz).unwrap();
        let url = format!("file://{}", src.display());
        let integ = Integrity::sha512(id);
        let mut m = Metrics::new();
        let art = store
            .ensure_artifact(&url, Some(&integ), &mut m)
            .expect("ensure_artifact");
        assert_eq!(art.id, id);
        store.ensure_image(&id, &mut m).expect("ensure_image");
        id
    }

    #[cfg(unix)]
    fn entry(path: &str, name: &str, resolved: &str, id: &ArtifactId) -> PackageEntry {
        PackageEntry {
            path: path.into(),
            name: name.into(),
            version: "1.0.0".into(),
            resolved: resolved.into(),
            integrity: Some(Integrity::sha512(*id).to_npm_string()),
            bin: BTreeMap::new(),
            ..Default::default()
        }
    }

    #[cfg(unix)]
    fn read_link_str(p: &Path) -> String {
        fs::read_link(p).unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn computes_relative_bin_targets() {
        // Top-level package bin.
        assert_eq!(
            relative_bin_target("node_modules/foo", "cli.js"),
            "../foo/cli.js"
        );
        // Leading "./" on the relpath is stripped.
        assert_eq!(
            relative_bin_target("node_modules/foo", "./cli.js"),
            "../foo/cli.js"
        );
        // Nested node_modules path.
        assert_eq!(
            relative_bin_target("node_modules/a/node_modules/b", "bin/run.js"),
            "../a/node_modules/b/bin/run.js"
        );
    }

    #[cfg(unix)]
    #[test]
    fn materializes_top_level_package_symlink_and_bin() {
        let project = tempdir().unwrap();
        let store_dir = tempdir().unwrap();
        let src = tempdir().unwrap();
        let store = ArtifactStore::open(store_dir.path()).unwrap();

        let tgz = build_pkg_tgz(&[
            (
                "package/package.json",
                br#"{"name":"foo","version":"1.0.0"}"#,
            ),
            ("package/cli.js", b"#!/usr/bin/env node\nconsole.log(42);\n"),
        ]);
        let id = stage_package(&store, &tgz, src.path(), 0);
        let image = store.image_path(&id);
        assert!(image.join("package.json").is_file());
        assert!(image.join("cli.js").is_file());

        let mut e = entry("node_modules/foo", "foo", "file://x", &id);
        e.bin.insert("foocli".into(), "./cli.js".into());

        let stats = materialize(project.path(), &store, &[(&e, id)]).unwrap();
        assert_eq!(stats.packages_materialized, 1);
        assert_eq!(stats.bins_linked, 1);
        assert_eq!(stats.links_skipped, 0);

        // Package symlink points at the store image and resolves to contents.
        let link = project.path().join("node_modules/foo");
        assert_eq!(read_link_str(&link), image.display().to_string());
        assert!(link.join("package.json").is_file());

        // .bin link is RELATIVE and points into the package path.
        let bin = project.path().join("node_modules/.bin/foocli");
        assert_eq!(read_link_str(&bin), "../foo/cli.js");
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(image.join("cli.js"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn hardlink_backend_keeps_bins_as_relative_symlinks() {
        let project = tempdir().unwrap();
        let store_dir = tempdir().unwrap();
        let src = tempdir().unwrap();
        let store = ArtifactStore::open(store_dir.path()).unwrap();

        let tgz = build_pkg_tgz(&[
            (
                "package/package.json",
                br#"{"name":"foo","version":"1.0.0"}"#,
            ),
            (
                "package/bin/cli.js",
                b"#!/usr/bin/env node\nconsole.log(42);\n",
            ),
        ]);
        let id = stage_package(&store, &tgz, src.path(), 0);
        assert!(store.image_index_path(&id).is_file());
        let mut e = entry("node_modules/foo", "foo", "file://x", &id);
        e.bin.insert("foocli".into(), "./bin/cli.js".into());

        let stats = materialize_with_backend(
            project.path(),
            &store,
            &[(&e, id)],
            MaterializeBackend::Hardlink,
        )
        .unwrap();
        assert_eq!(stats.packages_materialized, 1);
        assert_eq!(stats.bins_linked, 1);

        let package = project.path().join("node_modules/foo");
        assert!(package.is_dir());
        assert!(!fs::symlink_metadata(&package)
            .unwrap()
            .file_type()
            .is_symlink());
        let bin = project.path().join("node_modules/.bin/foocli");
        assert_eq!(read_link_str(&bin), "../foo/bin/cli.js");
        assert!(bin.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn materializes_nested_node_modules_path() {
        let project = tempdir().unwrap();
        let store_dir = tempdir().unwrap();
        let src = tempdir().unwrap();
        let store = ArtifactStore::open(store_dir.path()).unwrap();

        let tgz = build_pkg_tgz(&[(
            "package/package.json",
            br#"{"name":"deep","version":"2.0.0"}"#,
        )]);
        let id = stage_package(&store, &tgz, src.path(), 0);

        let e = entry("node_modules/a/node_modules/b", "b", "file://x", &id);
        let stats = materialize(project.path(), &store, &[(&e, id)]).unwrap();
        assert_eq!(stats.packages_materialized, 1);

        let link = project.path().join("node_modules/a/node_modules/b");
        assert!(link.join("package.json").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn is_idempotent_on_rerun() {
        let project = tempdir().unwrap();
        let store_dir = tempdir().unwrap();
        let src = tempdir().unwrap();
        let store = ArtifactStore::open(store_dir.path()).unwrap();

        let tgz = build_pkg_tgz(&[
            ("package/package.json", br#"{"name":"foo"}"#),
            ("package/cli.js", b"#!/usr/bin/env node\n1;\n"),
        ]);
        let id = stage_package(&store, &tgz, src.path(), 0);
        let mut e = entry("node_modules/foo", "foo", "file://x", &id);
        e.bin.insert("foocli".into(), "./cli.js".into());

        let first = materialize(project.path(), &store, &[(&e, id)]).unwrap();
        assert_eq!(first.packages_materialized, 1);
        assert_eq!(first.bins_linked, 1);

        // Snapshot the link targets.
        let pkg_link = read_link_str(&project.path().join("node_modules/foo"));
        let bin_link = read_link_str(&project.path().join("node_modules/.bin/foocli"));

        let second = materialize(project.path(), &store, &[(&e, id)]).unwrap();
        assert_eq!(second.packages_materialized, 1, "idempotent re-materialize");
        assert_eq!(second.bins_linked, 1);
        assert_eq!(second.bins_collisions, 0);

        // Unchanged.
        assert_eq!(
            read_link_str(&project.path().join("node_modules/foo")),
            pkg_link
        );
        assert_eq!(
            read_link_str(&project.path().join("node_modules/.bin/foocli")),
            bin_link
        );
    }

    #[cfg(unix)]
    #[test]
    fn replaces_stale_symlink_and_skips_link_entries() {
        let project = tempdir().unwrap();
        let store_dir = tempdir().unwrap();
        let src = tempdir().unwrap();
        let store = ArtifactStore::open(store_dir.path()).unwrap();

        let tgz = build_pkg_tgz(&[("package/package.json", br#"{"name":"foo"}"#)]);
        let id = stage_package(&store, &tgz, src.path(), 0);

        // Plant a stale symlink pointing at the wrong place.
        fs::create_dir_all(project.path().join("node_modules")).unwrap();
        std::os::unix::fs::symlink("/nonexistent/old", project.path().join("node_modules/foo"))
            .unwrap();

        let real = entry("node_modules/foo", "foo", "file://x", &id);
        // A link (workspace) entry that must be skipped, not materialized.
        let link_entry = PackageEntry {
            path: "node_modules/ws".into(),
            name: "ws".into(),
            version: "link".into(),
            resolved: String::new(),
            link: true,
            ..Default::default()
        };

        let stats = materialize(project.path(), &store, &[(&real, id), (&link_entry, id)]).unwrap();
        assert_eq!(stats.packages_materialized, 1);
        assert_eq!(stats.links_skipped, 1);

        // Stale symlink was replaced with the correct store target.
        let target = read_link_str(&project.path().join("node_modules/foo"));
        assert_eq!(target, store.image_path(&id).display().to_string());
        assert!(project
            .path()
            .join("node_modules/foo/package.json")
            .is_file());
        // The link entry was NOT turned into a symlink.
        assert!(!symlink_exists(&project.path().join("node_modules/ws")));
    }

    #[cfg(unix)]
    #[test]
    fn bin_collision_keeps_first_and_warns() {
        let project = tempdir().unwrap();
        let store_dir = tempdir().unwrap();
        let src = tempdir().unwrap();
        let store = ArtifactStore::open(store_dir.path()).unwrap();

        // Two distinct packages both declaring a bin named "cli".
        let tgz_a = build_pkg_tgz(&[
            ("package/package.json", br#"{"name":"a"}"#),
            ("package/run.js", b"#!/usr/bin/env node\n1;\n"),
        ]);
        let tgz_b = build_pkg_tgz(&[
            ("package/package.json", br#"{"name":"b"}"#),
            ("package/run.js", b"#!/usr/bin/env node\n2;\n"),
        ]);
        let id_a = stage_package(&store, &tgz_a, src.path(), 0);
        let id_b = stage_package(&store, &tgz_b, src.path(), 1);

        let mut a = entry("node_modules/a", "a", "file://x", &id_a);
        a.bin.insert("cli".into(), "./run.js".into());
        let mut b = entry("node_modules/b", "b", "file://x", &id_b);
        b.bin.insert("cli".into(), "./run.js".into());

        // Lockfile (path-sorted) order: a before b.
        let stats = materialize(project.path(), &store, &[(&a, id_a), (&b, id_b)]).unwrap();
        assert_eq!(stats.bins_linked, 1, "first declarant links the bin");
        assert_eq!(stats.bins_collisions, 1, "second is a collision");

        // The link still points at the first package.
        let bin = project.path().join("node_modules/.bin/cli");
        assert_eq!(read_link_str(&bin), "../a/run.js");
    }

    #[cfg(unix)]
    #[test]
    fn empty_resolved_is_skipped() {
        let project = tempdir().unwrap();
        let store_dir = tempdir().unwrap();
        let store = ArtifactStore::open(store_dir.path()).unwrap();

        // An entry with no resolved URL (and not a link) is skipped defensively.
        let e = PackageEntry {
            path: "node_modules/ghost".into(),
            name: "ghost".into(),
            version: "1.0.0".into(),
            resolved: String::new(),
            ..Default::default()
        };
        let stats = materialize(
            project.path(),
            &store,
            &[(&e, Sha512Digest::from_bytes([0u8; 64]))],
        )
        .unwrap();
        assert_eq!(stats.packages_materialized, 0);
        assert_eq!(stats.links_skipped, 1);
        assert!(!symlink_exists(&project.path().join("node_modules/ghost")));
    }

    /// Sanity: a freshly built lockfile round-trips so the installer can rely on
    /// package ordering. (Lightweight guard against accidental reordering.)
    #[test]
    fn lockfile_package_order_is_stable() {
        let mut lf = Lockfile::new("bpm");
        lf.root = RootEntry {
            name: Some("app".into()),
            version: Some("1.0.0".into()),
            dependencies: BTreeMap::from([("foo".into(), "^1.0.0".into())]),
        };
        lf.packages.push(PackageEntry {
            path: "node_modules/zoo".into(),
            name: "zoo".into(),
            version: "1.0.0".into(),
            resolved: "file:///x".into(),
            ..Default::default()
        });
        lf.packages.push(PackageEntry {
            path: "node_modules/foo".into(),
            name: "foo".into(),
            version: "1.0.0".into(),
            resolved: "file:///y".into(),
            ..Default::default()
        });
        lf.sort_packages();
        let paths: Vec<&str> = lf.packages.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(paths, vec!["node_modules/foo", "node_modules/zoo"]);
    }

    // Keep the non-unix build honest: materialize reports unsupported when it
    // would need to create a symlink. Skipped on unix where symlinks exist.
    #[cfg(not(unix))]
    #[test]
    fn materialize_reports_unsupported_on_non_unix() {
        let project = tempdir().unwrap();
        let store_dir = tempdir().unwrap();
        let store = ArtifactStore::open(store_dir.path()).unwrap();
        let e = PackageEntry {
            path: "node_modules/foo".into(),
            name: "foo".into(),
            version: "1.0.0".into(),
            resolved: "file:///x".into(),
            ..Default::default()
        };
        let err = materialize_with_backend(
            project.path(),
            &store,
            &[(&e, Sha512Digest::from_bytes([0u8; 64]))],
            MaterializeBackend::Symlink,
        )
        .unwrap_err();
        assert!(matches!(err, MaterializeError::SymlinksUnsupported));
    }
}
