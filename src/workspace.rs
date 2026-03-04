//! npm workspaces (IMPLEMENTATION §15 — Milestone 6).
//!
//! Discovers workspace packages declared in the root `package.json` via the
//! standard `"workspaces"` field (array of globs, or `{ "packages": [...] }`).
//! Discovery is deterministic: globs expand in sorted order and a workspace's
//! own `package.json` is its source of truth. Including the workspace layout in
//! the graph id (via [`canonical_workspace_bytes`]) makes a workspace-tree
//! change invalidate the cache, consistent with the dependency graph.
//!
//! Advanced globbing syntax (negation, nested objects beyond `packages`) is
//! deliberately out of scope for the first milestone.
//!
//! A filesystem-capability probe (reflink/clone support) lives here too; it
//! informs future materialization optimization and is recorded so results are
//! attributable to a concrete capability set.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;

use crate::manifest::PackageManifest;

/// A discovered workspace package.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorkspacePackage {
    /// Project-root-relative dir (POSIX slashes), e.g. `packages/a`.
    pub dir: String,
    pub name: Option<String>,
    pub version: Option<String>,
}

/// Discovered workspace layout for a project root.
#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
pub struct WorkspaceLayout {
    /// Source glob patterns (as declared), sorted.
    pub patterns: Vec<String>,
    /// Discovered workspace packages, sorted by `dir` for determinism.
    pub packages: Vec<WorkspacePackage>,
}

/// Discover the workspace layout for `project_root`, reading `workspaces` from
/// its `package.json`. Missing/declared-but-empty workspaces yield a layout
/// with no packages (never an error) so a non-workspace project is a no-op.
pub fn discover(project_root: &Path) -> WorkspaceLayout {
    let manifest = match PackageManifest::from_path(&project_root.join("package.json")) {
        Ok(m) => m,
        Err(_) => return WorkspaceLayout::default(),
    };
    let Some(ws) = manifest.workspaces.clone() else {
        return WorkspaceLayout::default();
    };
    let patterns: Vec<String> = ws.patterns().to_vec();

    let mut found: BTreeSet<String> = BTreeSet::new();
    // Simple, deterministic glob expansion: each pattern is a path prefix dir;
    // `packages/*` means direct children of `packages` that are directories.
    // This is a deliberate subset of npm's glob engine.
    for raw in &patterns {
        let pat = raw.trim_end_matches('/');
        if let Some(parent) = pat.strip_suffix("/*").or_else(|| pat.strip_suffix("/**")) {
            let base = project_root.join(parent);
            if let Ok(entries) = fs::read_dir(&base) {
                for e in entries.flatten() {
                    if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        // Only treat as a workspace if it has a package.json.
                        if e.path().join("package.json").exists() {
                            let rel = rel_from(project_root, &e.path());
                            if let Some(r) = rel {
                                found.insert(r);
                            }
                        }
                    }
                }
            }
        } else {
            // A literal workspace dir.
            let p = project_root.join(pat);
            if p.is_dir() {
                if let Some(r) = rel_from(project_root, &p) {
                    found.insert(r);
                }
            }
        }
    }

    let mut layout = WorkspaceLayout {
        patterns,
        packages: Vec::with_capacity(found.len()),
    };
    for dir in found {
        let manifest_path = project_root.join(&dir).join("package.json");
        let (name, version) = match PackageManifest::from_path(&manifest_path) {
            Ok(m) => (m.name, m.version),
            Err(_) => (None, None),
        };
        layout
            .packages
            .push(WorkspacePackage { dir, name, version });
    }
    layout
}

/// Canonical, byte-stable encoding of a workspace layout, to fold into the
/// graph id so a change to the workspace tree invalidates the cached plan/volume.
pub fn canonical_workspace_bytes(layout: &WorkspaceLayout) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(b"ws-v1\n");
    buf.extend_from_slice(b"patterns\n");
    write_u64(&mut buf, layout.patterns.len() as u64);
    for p in &layout.patterns {
        write_field(&mut buf, p);
    }
    buf.extend_from_slice(b"packages\n");
    write_u64(&mut buf, layout.packages.len() as u64);
    for p in &layout.packages {
        write_field(&mut buf, &p.dir);
        write_field(&mut buf, p.name.as_deref().unwrap_or(""));
        write_field(&mut buf, p.version.as_deref().unwrap_or(""));
    }
    buf
}

/// Result of testing one operation on the destination filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    Supported,
    Unsupported {
        kind: io::ErrorKind,
        raw_os_error: Option<i32>,
    },
}

impl Capability {
    fn from_error(error: &io::Error) -> Self {
        Self::Unsupported {
            kind: error.kind(),
            raw_os_error: error.raw_os_error(),
        }
    }

    /// Whether the operation completed and its result passed verification.
    pub fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }
}

/// Filesystem capability profile measured on one destination volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsCapabilities {
    pub directory_symlink: Capability,
    pub directory_junction: Capability,
    pub hardlink_file: Capability,
    pub reflink_file: Capability,
    pub atomic_directory_rename: Capability,
    pub case_sensitive: bool,
}

/// Failure to create or clean up the destination-local probe workspace.
#[derive(Debug)]
pub struct ProbeError {
    operation: &'static str,
    path: PathBuf,
    source: io::Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProbeCacheKey {
    parent: PathBuf,
    #[cfg(unix)]
    device: u64,
}

static FS_CAPABILITY_CACHE: OnceLock<Mutex<HashMap<ProbeCacheKey, FsCapabilities>>> =
    OnceLock::new();

impl fmt::Display for ProbeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "could not {} filesystem capability probe at {}: {}",
            self.operation,
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for ProbeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Test capabilities on the filesystem that will contain `target`.
///
/// A hidden temporary directory is created below the nearest existing ancestor
/// of `target`, ensuring that every operation is attempted on the destination
/// mount rather than inferred from the operating system. The directory is
/// removed even when an individual capability is unsupported.
pub fn probe_fs_capabilities(target: &Path) -> Result<FsCapabilities, ProbeError> {
    let probe_parent = nearest_existing_directory(target).ok_or_else(|| ProbeError {
        operation: "locate an existing ancestor for",
        path: target.to_path_buf(),
        source: io::Error::new(io::ErrorKind::NotFound, "no existing directory ancestor"),
    })?;
    let probe_parent = fs::canonicalize(&probe_parent).map_err(|source| ProbeError {
        operation: "resolve the existing ancestor for",
        path: probe_parent,
        source,
    })?;
    let cache_key = probe_cache_key(probe_parent.clone()).map_err(|source| ProbeError {
        operation: "identify the destination volume for",
        path: probe_parent.clone(),
        source,
    })?;
    let cache = FS_CAPABILITY_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(capabilities) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&cache_key)
        .copied()
    {
        return Ok(capabilities);
    }
    let probe = tempfile::Builder::new()
        .prefix(".bpm-fs-probe-")
        .tempdir_in(&probe_parent)
        .map_err(|source| ProbeError {
            operation: "create",
            path: probe_parent.clone(),
            source,
        })?;
    let root_path = probe.path().to_path_buf();
    let root = root_path.as_path();
    let source_file = root.join("source-file");
    let source_dir = root.join("source-dir");
    fs::write(&source_file, b"bpm-probe-source").map_err(|source| ProbeError {
        operation: "initialize",
        path: source_file.clone(),
        source,
    })?;
    fs::create_dir(&source_dir).map_err(|source| ProbeError {
        operation: "initialize",
        path: source_dir.clone(),
        source,
    })?;
    fs::write(source_dir.join("marker"), b"bpm-probe-directory").map_err(|source| ProbeError {
        operation: "initialize",
        path: source_dir.clone(),
        source,
    })?;

    let directory_symlink = symlink_probe(&source_dir, &root.join("directory-symlink"));
    let directory_junction = junction_probe(
        &source_dir,
        &root.join("directory-junction"),
        directory_symlink,
    );
    let capabilities = FsCapabilities {
        directory_symlink,
        directory_junction,
        hardlink_file: hardlink_probe(&source_file, &root.join("hardlink-file")),
        reflink_file: reflink_probe(&source_file, &root.join("reflink-file")),
        atomic_directory_rename: rename_probe(root),
        case_sensitive: case_sensitivity_probe(root),
    };

    probe.close().map_err(|source| ProbeError {
        operation: "clean up",
        path: root_path,
        source,
    })?;
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(cache_key, capabilities);
    Ok(capabilities)
}

#[cfg(unix)]
fn probe_cache_key(parent: PathBuf) -> io::Result<ProbeCacheKey> {
    use std::os::unix::fs::MetadataExt;

    Ok(ProbeCacheKey {
        device: fs::metadata(&parent)?.dev(),
        parent,
    })
}

#[cfg(not(unix))]
fn probe_cache_key(parent: PathBuf) -> io::Result<ProbeCacheKey> {
    // A canonical Windows path carries its volume/UNC identity and the exact
    // probe parent, so results cannot leak from a generic temporary volume.
    Ok(ProbeCacheKey { parent })
}

fn nearest_existing_directory(target: &Path) -> Option<PathBuf> {
    let mut candidate = if target.is_file() {
        target.parent()?.to_path_buf()
    } else {
        target.to_path_buf()
    };
    loop {
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !candidate.pop() {
            return None;
        }
    }
}

#[cfg(unix)]
fn symlink_probe(source: &Path, destination: &Path) -> Capability {
    use std::os::unix::fs::symlink;
    match symlink(source, destination) {
        Ok(()) if verify_directory_link(destination) => Capability::Supported,
        Ok(()) => unsupported(io::ErrorKind::InvalidData),
        Err(error) => Capability::from_error(&error),
    }
}

#[cfg(windows)]
fn symlink_probe(source: &Path, destination: &Path) -> Capability {
    match std::os::windows::fs::symlink_dir(source, destination) {
        Ok(()) if verify_directory_link(destination) => Capability::Supported,
        Ok(()) => unsupported(io::ErrorKind::InvalidData),
        Err(error) => Capability::from_error(&error),
    }
}

#[cfg(not(any(unix, windows)))]
fn symlink_probe(_source: &Path, _destination: &Path) -> Capability {
    unsupported(io::ErrorKind::Unsupported)
}

fn verify_directory_link(destination: &Path) -> bool {
    fs::symlink_metadata(destination)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
        && matches!(
            fs::read(destination.join("marker")),
            Ok(bytes) if bytes == b"bpm-probe-directory"
        )
}

#[cfg(windows)]
fn junction_probe(source: &Path, destination: &Path, symlink: Capability) -> Capability {
    if symlink.is_supported() {
        return unsupported(io::ErrorKind::Unsupported);
    }
    let output = Command::new("cmd")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(destination)
        .arg(source)
        .output();
    match output {
        Ok(output) if output.status.success() && verify_directory_link(destination) => {
            Capability::Supported
        }
        Ok(_) => unsupported(io::ErrorKind::Unsupported),
        Err(error) => Capability::from_error(&error),
    }
}

#[cfg(not(windows))]
fn junction_probe(_source: &Path, _destination: &Path, _symlink: Capability) -> Capability {
    unsupported(io::ErrorKind::Unsupported)
}

fn hardlink_probe(source: &Path, destination: &Path) -> Capability {
    match fs::hard_link(source, destination) {
        Ok(())
            if matches!(
                fs::read(destination),
                Ok(bytes) if bytes == b"bpm-probe-source"
            ) && verify_hardlink_identity(source, destination) =>
        {
            Capability::Supported
        }
        Ok(()) => unsupported(io::ErrorKind::InvalidData),
        Err(error) => Capability::from_error(&error),
    }
}

#[cfg(unix)]
fn verify_hardlink_identity(source: &Path, destination: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    match (fs::metadata(source), fs::metadata(destination)) {
        (Ok(source), Ok(destination)) => {
            source.dev() == destination.dev() && source.ino() == destination.ino()
        }
        _ => false,
    }
}

#[cfg(not(unix))]
fn verify_hardlink_identity(_source: &Path, _destination: &Path) -> bool {
    // Stable std does not expose a portable file identity. Successful creation
    // and byte verification still prove hardlink availability on this volume.
    true
}

fn reflink_probe(source: &Path, destination: &Path) -> Capability {
    let result = clone_file_only(source, destination);
    match result {
        Ok(()) => {
            let isolated = fs::write(destination, b"bpm-probe-clone")
                .and_then(|()| fs::read(source))
                .map(|bytes| bytes == b"bpm-probe-source")
                .unwrap_or(false);
            if isolated {
                Capability::Supported
            } else {
                unsupported(io::ErrorKind::InvalidData)
            }
        }
        Err(error) => Capability::from_error(&error),
    }
}

#[cfg(target_os = "linux")]
fn clone_file_only(source: &Path, destination: &Path) -> io::Result<()> {
    command_succeeded(
        Command::new("cp")
            .arg("--reflink=always")
            .arg("--")
            .arg(source)
            .arg(destination),
    )
}

#[cfg(target_os = "macos")]
fn clone_file_only(source: &Path, destination: &Path) -> io::Result<()> {
    command_succeeded(Command::new("cp").arg("-c").arg(source).arg(destination))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn clone_file_only(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no verified clone-only operation is available",
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn command_succeeded(command: &mut Command) -> io::Result<()> {
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "clone-only copy was rejected by the destination filesystem",
        ))
    }
}

fn case_sensitivity_probe(root: &Path) -> bool {
    let lower = root.join("case-probe");
    let upper = root.join("CASE-PROBE");
    if fs::write(&lower, b"lower").is_err() {
        return false;
    }
    match OpenOptions::new().write(true).create_new(true).open(&upper) {
        Ok(_) => matches!(fs::read(&lower), Ok(bytes) if bytes == b"lower"),
        Err(_) => false,
    }
}

fn rename_probe(root: &Path) -> Capability {
    let source = root.join("rename-source");
    let destination = root.join("rename-destination");
    let result = fs::create_dir(&source)
        .and_then(|()| fs::write(source.join("marker"), b"complete"))
        .and_then(|()| fs::rename(&source, &destination));
    match result {
        Ok(())
            if matches!(
                fs::read(destination.join("marker")),
                Ok(bytes) if bytes == b"complete"
            ) =>
        {
            Capability::Supported
        }
        Ok(()) => unsupported(io::ErrorKind::InvalidData),
        Err(error) => Capability::from_error(&error),
    }
}

fn unsupported(kind: io::ErrorKind) -> Capability {
    Capability::Unsupported {
        kind,
        raw_os_error: None,
    }
}

fn rel_from(root: &Path, p: &Path) -> Option<String> {
    if let Ok(rel) = p.strip_prefix(root) {
        let s = rel.to_string_lossy().replace('\\', "/");
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    } else {
        None
    }
}

fn write_field(buf: &mut Vec<u8>, s: &str) {
    write_u64(buf, s.len() as u64);
    buf.extend_from_slice(s.as_bytes());
}

fn write_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_manifest(dir: &Path, json: &str) {
        fs::write(dir.join("package.json"), json).unwrap();
    }

    #[test]
    fn discovers_simple_workspace_packages() {
        let root = tempdir().unwrap();
        write_manifest(
            root.path(),
            r#"{"name":"root","workspaces":["packages/*"]}"#,
        );
        fs::create_dir_all(root.path().join("packages/a")).unwrap();
        write_manifest(
            &root.path().join("packages/a"),
            r#"{"name":"a","version":"1.0.0"}"#,
        );
        fs::create_dir_all(root.path().join("packages/b")).unwrap();
        write_manifest(
            &root.path().join("packages/b"),
            r#"{"name":"b","version":"2.0.0"}"#,
        );
        // Non-package dir (no package.json):
        fs::create_dir_all(root.path().join("packages/empty")).unwrap();

        let layout = discover(root.path());
        let dirs: Vec<&str> = layout.packages.iter().map(|p| p.dir.as_str()).collect();
        assert_eq!(dirs, vec!["packages/a", "packages/b"]);
    }

    #[test]
    fn no_workspaces_yields_empty_layout() {
        let root = tempdir().unwrap();
        write_manifest(root.path(), r#"{"name":"root"}"#);
        let layout = discover(root.path());
        assert!(layout.packages.is_empty());
        assert!(layout.patterns.is_empty());
    }

    #[test]
    fn canonical_bytes_stable_for_same_layout() {
        let a = WorkspaceLayout {
            patterns: vec!["packages/*".into()],
            packages: vec![WorkspacePackage {
                dir: "packages/a".into(),
                name: Some("a".into()),
                version: Some("1.0.0".into()),
            }],
        };
        let b = a.clone(); // same content
        assert_eq!(canonical_workspace_bytes(&a), canonical_workspace_bytes(&b));
    }

    #[test]
    fn workspace_change_changes_bytes() {
        let a = WorkspaceLayout {
            patterns: vec!["packages/*".into()],
            packages: vec![],
        };
        let b = WorkspaceLayout {
            patterns: vec!["packages/*".into()],
            packages: vec![WorkspacePackage {
                dir: "packages/a".into(),
                name: Some("a".into()),
                version: Some("1.0.0".into()),
            }],
        };
        assert_ne!(canonical_workspace_bytes(&a), canonical_workspace_bytes(&b));
    }

    #[test]
    fn fs_capability_probe_tests_target_and_cleans_up() {
        let root = tempdir().unwrap();
        let target = root.path().join("future/store/path");
        let caps = probe_fs_capabilities(&target).unwrap();
        let cached = probe_fs_capabilities(&target).unwrap();

        assert!(caps.hardlink_file.is_supported());
        assert!(caps.atomic_directory_rename.is_supported());
        assert_eq!(cached, caps);
        assert!(!target.exists());
        assert!(fs::read_dir(root.path()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".bpm-fs-probe-")));
    }

    #[cfg(unix)]
    #[test]
    fn unix_probe_verifies_directory_symlinks_on_target_volume() {
        let root = tempdir().unwrap();
        let caps = probe_fs_capabilities(root.path()).unwrap();
        assert!(caps.directory_symlink.is_supported());
        assert!(!caps.directory_junction.is_supported());
    }
}
