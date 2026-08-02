//! Lossless `package.json` dependency editing and crash-safe two-file
//! publishing.
//!
//! The typed [`crate::manifest::PackageManifest`] is a *subset* of npm's
//! package.json schema: serializing it would drop `license`, `exports`,
//! `files`, `publishConfig`, and tool configuration. Dependency mutation must
//! instead edit the raw JSON document and reparse it through the typed
//! manifest for validation. This module owns that lossless edit and the
//! crash-bounded publication of the edited manifest alongside its lock.
//!
//! This module never resolves packages, never invokes the resolver, and never
//! chooses a lock kind. It edits one document and publishes two byte buffers.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use thiserror::Error;

use crate::manifest::{ManifestError, PackageManifest};

/// The npm dependency sections this editor can read or mutate.
pub const DEPENDENCY_SECTIONS: [&str; 4] = [
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
];

/// The two sections a local add may write. Optional and peer mutation are
/// deferred to a later source-protocol plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencySection {
    Production,
    Dev,
}

impl DependencySection {
    /// The JSON object key this section is stored under.
    pub fn json_key(self) -> &'static str {
        match self {
            DependencySection::Production => "dependencies",
            DependencySection::Dev => "devDependencies",
        }
    }
}

/// Error while loading, editing, or rendering a manifest document.
#[derive(Debug, Error)]
pub enum ManifestEditError {
    #[error("cannot read package.json at {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("invalid JSON in package.json at {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("package.json at {path} is not a JSON object at the top level")]
    NotObject { path: PathBuf },
    #[error("dependency section \"{section}\" in {path} is not a JSON object")]
    SectionNotObject { path: PathBuf, section: String },
    #[error(
        "package \"{name}\" is already declared in optionalDependencies or peerDependencies; remove it there first or use a supported section"
    )]
    AmbiguousDependency { path: PathBuf, name: String },
}

/// A lossless view of a `package.json` for dependency editing.
///
/// Stores the parsed JSON object (so unknown top-level fields survive), the
/// original bytes (for no-op detection and rollback), and the detected
/// trailing-newline policy (preserved on render).
#[derive(Debug)]
pub struct ManifestDocument {
    source_path: PathBuf,
    root: Map<String, Value>,
    original_bytes: Vec<u8>,
    trailing_newline: bool,
}

impl ManifestDocument {
    /// Load a manifest document from a file path.
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, ManifestEditError> {
        let path = path.into();
        let bytes = fs::read(&path).map_err(|source| ManifestEditError::Read {
            path: path.clone(),
            source,
        })?;
        Self::from_bytes(bytes, path)
    }

    /// Parse a manifest document from its bytes and source path.
    pub fn from_bytes(
        bytes: Vec<u8>,
        source_path: impl Into<PathBuf>,
    ) -> Result<Self, ManifestEditError> {
        let source_path = source_path.into();
        let trailing_newline = bytes.last() == Some(&b'\n');
        let root_value: Value =
            serde_json::from_slice(&bytes).map_err(|source| ManifestEditError::Parse {
                path: source_path.clone(),
                source,
            })?;
        let Value::Object(root) = root_value else {
            return Err(ManifestEditError::NotObject { path: source_path });
        };
        Ok(Self {
            source_path,
            root,
            original_bytes: bytes,
            trailing_newline,
        })
    }

    /// The source path this document was loaded from.
    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    /// The exact bytes the document was loaded from.
    pub fn original_bytes(&self) -> &[u8] {
        &self.original_bytes
    }

    /// Whether `name` appears in any of the four dependency sections.
    pub fn has_dependency(&self, name: &str) -> bool {
        DEPENDENCY_SECTIONS
            .iter()
            .any(|section| self.contains(section, name))
    }

    fn contains(&self, section: &str, name: &str) -> bool {
        self.root
            .get(section)
            .and_then(Value::as_object)
            .is_some_and(|map| map.contains_key(name))
    }

    /// Add `name -> spec` to `section`, applying npm's dual-section rule:
    /// adding to `dependencies` removes a same-name entry from
    /// `devDependencies` and vice-versa. If the name already lives in
    /// `optionalDependencies` or `peerDependencies`, return an ambiguity
    /// error rather than silently moving it.
    pub fn add_dependency(
        &mut self,
        section: DependencySection,
        name: &str,
        spec: &str,
    ) -> Result<(), ManifestEditError> {
        if self.contains("optionalDependencies", name) || self.contains("peerDependencies", name) {
            return Err(ManifestEditError::AmbiguousDependency {
                path: self.source_path.clone(),
                name: name.to_string(),
            });
        }
        self.ensure_section_object(section.json_key())?;
        let map = self
            .root
            .entry(section.json_key().to_string())
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("section validated as object");
        map.insert(name.to_string(), Value::String(spec.to_string()));
        // Move out of the dual section so a name never lives in both
        // `dependencies` and `devDependencies`.
        let dual = match section {
            DependencySection::Production => "devDependencies",
            DependencySection::Dev => "dependencies",
        };
        if let Some(Value::Object(map)) = self.root.get_mut(dual) {
            map.remove(name);
        }
        Ok(())
    }

    /// Remove `name` from every dependency section. Returns whether anything
    /// was removed (used by `remove` to detect a no-op).
    pub fn remove_dependency(&mut self, name: &str) -> bool {
        let mut removed = false;
        for section in DEPENDENCY_SECTIONS {
            if let Some(Value::Object(map)) = self.root.get_mut(section) {
                removed |= map.remove(name).is_some();
            }
        }
        removed
    }

    /// Reject a section that exists but is not a JSON object. A missing
    /// section is fine; [`Self::add_dependency`] creates it on demand.
    fn ensure_section_object(&self, section: &str) -> Result<(), ManifestEditError> {
        match self.root.get(section) {
            None | Some(Value::Object(_)) => Ok(()),
            Some(_) => Err(ManifestEditError::SectionNotObject {
                path: self.source_path.clone(),
                section: section.to_string(),
            }),
        }
    }

    /// Render the edited document to canonical two-space JSON, preserving the
    /// original trailing-newline policy. Unknown top-level fields and
    /// unmodified values survive unchanged at the data level; only whitespace
    /// is normalized.
    pub fn render(&self) -> Vec<u8> {
        let mut out = serde_json::to_vec_pretty(&Value::Object(self.root.clone()))
            .expect("JSON document is always serializable");
        if self.trailing_newline {
            out.push(b'\n');
        }
        out
    }

    /// Whether rendering produces bytes different from the original.
    pub fn changed(&self) -> bool {
        self.render() != self.original_bytes
    }

    /// Reparse the rendered document through the typed manifest so the
    /// resolver never consumes a hand-built structure.
    pub fn to_manifest(&self) -> Result<PackageManifest, ManifestError> {
        let rendered = self.render();
        let text = String::from_utf8(rendered).unwrap_or_default();
        PackageManifest::from_json(&text, &self.source_path)
    }
}

/// An exclusive cross-process mutation lock for one canonical project.
///
/// The lock file name contains only a one-way digest of the canonical OS path.
/// Lock files are intentionally retained after release so waiters can never be
/// split across different inodes.
#[derive(Debug)]
pub struct ProjectMutationGuard {
    file: fs::File,
}

impl Drop for ProjectMutationGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[derive(Debug, Error)]
pub enum ProjectMutationLockError {
    #[error("cannot canonicalize project for mutation locking: {source}")]
    Canonicalize { source: io::Error },
    #[error("cannot create BPM mutation lock directory: {source}")]
    CreateDirectory { source: io::Error },
    #[error("cannot open project mutation lock at {path}: {source}")]
    Open { path: PathBuf, source: io::Error },
    #[error("cannot acquire project mutation lock at {path}: {source}")]
    Acquire { path: PathBuf, source: io::Error },
}

/// Acquire the project-scoped mutation critical section.
pub fn acquire_project_mutation_guard(
    project_root: &Path,
) -> Result<ProjectMutationGuard, ProjectMutationLockError> {
    let lock_path = project_mutation_lock_path(project_root)?;
    let lock_dir = lock_path.parent().expect("mutation lock has a parent");
    fs::create_dir_all(lock_dir)
        .map_err(|source| ProjectMutationLockError::CreateDirectory { source })?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|source| ProjectMutationLockError::Open {
            path: lock_path.clone(),
            source,
        })?;
    file.lock()
        .map_err(|source| ProjectMutationLockError::Acquire {
            path: lock_path,
            source,
        })?;
    Ok(ProjectMutationGuard { file })
}

fn project_mutation_lock_path(project_root: &Path) -> Result<PathBuf, ProjectMutationLockError> {
    let canonical = fs::canonicalize(project_root)
        .map_err(|source| ProjectMutationLockError::Canonicalize { source })?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"bpm-project-mutation-v1\0");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(canonical.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in canonical.as_os_str().encode_wide() {
            hasher.update(&unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        // Fallback targets cannot preserve non-Unicode path identity because
        // std exposes no platform byte representation there.
        hasher.update(canonical.as_os_str().to_string_lossy().as_bytes());
    }
    Ok(std::env::temp_dir()
        .join("bpm-project-mutation-locks")
        .join(format!("{}.lock", hasher.finalize().to_hex())))
}

/// The two files a mutation can publish.
#[derive(Debug, Clone)]
pub struct PublishPlan {
    pub manifest_path: PathBuf,
    pub manifest_bytes: Vec<u8>,
    pub lock_path: PathBuf,
    pub lock_bytes: Vec<u8>,
}

/// The publish stage at which an injected failure should occur. Production
/// callers use [`publish`]; tests use [`publish_with_failure`] to prove
/// rollback restores both destinations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishStage {
    /// Fail just before publishing the lock (after temps are written).
    Lock,
    /// Fail just after the lock is published, before the manifest.
    Manifest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackDestination {
    Lock,
    Manifest,
}

impl std::fmt::Display for RollbackDestination {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lock => formatter.write_str("lock"),
            Self::Manifest => formatter.write_str("manifest"),
        }
    }
}

#[derive(Debug)]
pub struct RollbackFailure {
    pub destination: RollbackDestination,
    pub source: io::Error,
}

/// A publication error that records whether rollback was complete.
#[derive(Debug)]
pub struct PublishError {
    primary: io::Error,
    publication_began: bool,
    rollback_failures: Vec<RollbackFailure>,
}

impl PublishError {
    pub fn publication_began(&self) -> bool {
        self.publication_began
    }

    pub fn rollback_complete(&self) -> bool {
        self.rollback_failures.is_empty()
    }

    pub fn rollback_failures(&self) -> &[RollbackFailure] {
        &self.rollback_failures
    }
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "publication failed: {}", self.primary)?;
        if !self.rollback_failures.is_empty() {
            let destinations = self
                .rollback_failures
                .iter()
                .map(|failure| failure.destination.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            write!(formatter, "; rollback incomplete for {destinations}")
        } else if self.publication_began {
            formatter.write_str("; both destinations were restored")
        } else {
            formatter.write_str("; publication had not begun")
        }
    }
}

impl std::error::Error for PublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.primary)
    }
}

/// Publish the manifest and lock with cross-file rollback reporting.
///
/// Both temp files are written first; the lock is then published, then the
/// manifest. If either publish errors, restoration of both destinations is
/// attempted and every restoration failure is retained in [`PublishError`].
pub fn publish(plan: &PublishPlan) -> Result<(), PublishError> {
    publish_impl(plan, None, RestoreFaults::default())
}

/// Publish with a deterministic injected failure at `fail_at`. Exposed for
/// rollback tests; production code calls [`publish`].
pub fn publish_with_failure(plan: &PublishPlan, fail_at: PublishStage) -> Result<(), PublishError> {
    publish_impl(plan, Some(fail_at), RestoreFaults::default())
}

/// An error while atomically publishing one file.
#[derive(Debug, Error)]
#[error("cannot atomically publish {path}: {source}")]
pub struct PublishBytesError {
    path: PathBuf,
    #[source]
    source: io::Error,
}

/// Publish bytes through a unique, synchronized sibling and an atomic
/// replace-existing operation. The helper does not alter the supplied bytes.
pub fn publish_bytes(path: &Path, bytes: &[u8]) -> Result<(), PublishBytesError> {
    publish_bytes_impl(path, bytes, false)
}

fn publish_bytes_impl(
    path: &Path,
    bytes: &[u8],
    fail_before_replace: bool,
) -> Result<(), PublishBytesError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| PublishBytesError {
        path: path.to_path_buf(),
        source,
    })?;
    let hint = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("document");
    let tmp = unique_sibling(parent, hint);
    let result = (|| -> io::Result<()> {
        write_temp(&tmp, bytes)?;
        if fail_before_replace {
            return Err(io::Error::other("injected failure before replacement"));
        }
        replace_destination(&tmp, path)
    })();
    let _ = fs::remove_file(&tmp);
    result.map_err(|source| PublishBytesError {
        path: path.to_path_buf(),
        source,
    })
}

#[derive(Clone)]
struct Original {
    existed: bool,
    bytes: Option<Vec<u8>>,
}

fn capture_original(path: &Path) -> io::Result<Original> {
    match fs::read(path) {
        Ok(bytes) => Ok(Original {
            existed: true,
            bytes: Some(bytes),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Original {
            existed: false,
            bytes: None,
        }),
        Err(error) => Err(error),
    }
}

fn restore(path: &Path, original: &Original) -> io::Result<()> {
    match original.bytes.as_deref() {
        Some(bytes) if original.existed => {
            // Restore in-place via a sibling temp + rename so a crash between
            // truncate and write cannot leave the file empty.
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            let tmp = unique_sibling(
                parent,
                path.file_name().and_then(|n| n.to_str()).unwrap_or("doc"),
            );
            write_temp(&tmp, bytes)?;
            rename_or_replace(&tmp, path)
        }
        _ => {
            if path.exists() {
                fs::remove_file(path)?;
            }
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct RestoreFaults {
    lock: bool,
    manifest: bool,
}

fn publish_impl(
    plan: &PublishPlan,
    fail_at: Option<PublishStage>,
    restore_faults: RestoreFaults,
) -> Result<(), PublishError> {
    let before_publication = |primary| PublishError {
        primary,
        publication_began: false,
        rollback_failures: Vec::new(),
    };
    let manifest_original = capture_original(&plan.manifest_path).map_err(before_publication)?;
    let lock_original = capture_original(&plan.lock_path).map_err(before_publication)?;

    let manifest_parent = plan
        .manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let lock_parent = plan.lock_path.parent().unwrap_or_else(|| Path::new("."));
    let manifest_tmp = unique_sibling(
        manifest_parent,
        plan.manifest_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("package.json"),
    );
    let lock_tmp = unique_sibling(
        lock_parent,
        plan.lock_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("bpm.lock"),
    );

    let mut publication_began = false;
    let outcome = (|| -> io::Result<()> {
        write_temp(&manifest_tmp, &plan.manifest_bytes)?;
        write_temp(&lock_tmp, &plan.lock_bytes)?;

        if matches!(fail_at, Some(PublishStage::Lock)) {
            return Err(io::Error::other("injected failure before lock publish"));
        }

        publication_began = true;
        rename_or_replace(&lock_tmp, &plan.lock_path)?;
        // The lock temp has been consumed by the rename. Only the manifest
        // temp remains to clean up on failure.
        if matches!(fail_at, Some(PublishStage::Manifest)) {
            return Err(io::Error::other("injected failure before manifest publish"));
        }

        rename_or_replace(&manifest_tmp, &plan.manifest_path)?;
        Ok(())
    })();

    // Best-effort cleanup of any temp that was not consumed by a rename.
    let _ = fs::remove_file(&manifest_tmp);
    let _ = fs::remove_file(&lock_tmp);

    match outcome {
        Ok(()) => Ok(()),
        Err(primary) => {
            let mut rollback_failures = Vec::new();
            let lock_restore = if restore_faults.lock {
                Err(io::Error::other("injected lock restoration failure"))
            } else {
                restore(&plan.lock_path, &lock_original)
            };
            if let Err(source) = lock_restore {
                rollback_failures.push(RollbackFailure {
                    destination: RollbackDestination::Lock,
                    source,
                });
            }
            let manifest_restore = if restore_faults.manifest {
                Err(io::Error::other("injected manifest restoration failure"))
            } else {
                restore(&plan.manifest_path, &manifest_original)
            };
            if let Err(source) = manifest_restore {
                rollback_failures.push(RollbackFailure {
                    destination: RollbackDestination::Manifest,
                    source,
                });
            }
            Err(PublishError {
                primary,
                publication_began,
                rollback_failures,
            })
        }
    }
}

/// Write a unique sibling temp file with `O_CREAT|O_EXCL` so two concurrent
/// mutations cannot clobber each other's temp. Syncs before returning so the
/// bytes survive a crash immediately after publication.
fn write_temp(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Rename `tmp` onto `dest`, replacing an existing destination. On Unix this
/// is a single atomic rename; on Windows, where `rename` refuses to overwrite,
/// the destination is removed first. The cross-file boundary (manifest +
/// lock) is still not globally atomic — see [`publish`] and the plan's crash
/// documentation.
fn rename_or_replace(tmp: &Path, dest: &Path) -> io::Result<()> {
    replace_destination(tmp, dest)
}

fn replace_destination(tmp: &Path, dest: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::rename(tmp, dest)?;
        let parent = dest.parent().unwrap_or_else(|| Path::new("."));
        fs::File::open(parent)?.sync_all()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, ReplaceFileW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
            REPLACEFILE_WRITE_THROUGH,
        };
        let wide = |path: &Path| {
            path.as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect::<Vec<_>>()
        };
        let tmp_wide = wide(tmp);
        let dest_wide = wide(dest);
        let success = if dest.exists() {
            // ReplaceFileW preserves the destination name and performs a
            // write-through replace without a remove-then-rename window.
            unsafe {
                ReplaceFileW(
                    dest_wide.as_ptr(),
                    tmp_wide.as_ptr(),
                    std::ptr::null(),
                    REPLACEFILE_WRITE_THROUGH,
                    std::ptr::null(),
                    std::ptr::null(),
                )
            }
        } else {
            unsafe {
                MoveFileExW(
                    tmp_wide.as_ptr(),
                    dest_wide.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            }
        };
        if success == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        fs::rename(tmp, dest)
    }
}

fn unique_sibling(parent: &Path, hint: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let safe_hint = hint.replace('/', "_");
    parent.join(format!(".{safe_hint}.bpm-{pid}-{nanos}.tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn doc(json: &str) -> ManifestDocument {
        ManifestDocument::from_bytes(json.as_bytes().to_vec(), Path::new("package.json")).unwrap()
    }

    #[test]
    fn preserves_unknown_top_level_fields() {
        let mut document = doc(
            r#"{"name":"app","license":"MIT","exports":{".":"./index.js"},
            "publishConfig":{"access":"public"},"dependencies":{"a":"^1.0.0"}}"#,
        );
        document
            .add_dependency(DependencySection::Production, "b", "^2.0.0")
            .unwrap();
        let manifest = document.to_manifest().unwrap();
        assert_eq!(
            manifest.dependencies.get("b").map(String::as_str),
            Some("^2.0.0")
        );
        let rendered = String::from_utf8(document.render()).unwrap();
        assert!(rendered.contains("\"license\""), "{rendered}");
        assert!(rendered.contains("MIT"), "{rendered}");
        assert!(rendered.contains("\"exports\""), "{rendered}");
        assert!(rendered.contains("\"publishConfig\""), "{rendered}");
    }

    #[test]
    fn keeps_dependency_names_sorted_within_a_modified_section() {
        let mut document = doc(r#"{"dependencies":{"zebra":"^1.0.0"}}"#);
        document
            .add_dependency(DependencySection::Production, "apple", "^2.0.0")
            .unwrap();
        document
            .add_dependency(DependencySection::Production, "mango", "^3.0.0")
            .unwrap();
        let manifest = document.to_manifest().unwrap();
        assert_eq!(
            manifest.dependencies.keys().collect::<Vec<_>>(),
            vec!["apple", "mango", "zebra"]
        );
    }

    #[test]
    fn adding_to_dependencies_moves_out_of_devdependencies() {
        let mut document =
            doc(r#"{"dependencies":{"a":"^1.0.0"},"devDependencies":{"b":"^2.0.0"}}"#);
        document
            .add_dependency(DependencySection::Production, "b", "^2.0.0")
            .unwrap();
        let manifest = document.to_manifest().unwrap();
        assert!(manifest.dependencies.contains_key("b"));
        assert!(!manifest.dev_dependencies.contains_key("b"));
    }

    #[test]
    fn adding_to_devdependencies_moves_out_of_dependencies() {
        let mut document = doc(r#"{"dependencies":{"a":"^1.0.0"}}"#);
        document
            .add_dependency(DependencySection::Dev, "a", "^1.0.0")
            .unwrap();
        let manifest = document.to_manifest().unwrap();
        assert!(!manifest.dependencies.contains_key("a"));
        assert!(manifest.dev_dependencies.contains_key("a"));
    }

    #[test]
    fn rejects_ambiguous_optional_or_peer_declaration() {
        let mut document =
            doc(r#"{"dependencies":{"a":"^1.0.0"},"optionalDependencies":{"b":"^2.0.0"}}"#);
        let error = document
            .add_dependency(DependencySection::Production, "b", "^2.0.0")
            .unwrap_err();
        assert!(matches!(
            error,
            ManifestEditError::AmbiguousDependency { .. }
        ));
    }

    #[test]
    fn rejects_non_object_dependency_section() {
        let mut document = doc(r#"{"dependencies":["nope"]}"#);
        let error = document
            .add_dependency(DependencySection::Production, "b", "^2.0.0")
            .unwrap_err();
        assert!(matches!(error, ManifestEditError::SectionNotObject { .. }));
    }

    #[test]
    fn rejects_non_object_root() {
        let error = ManifestDocument::from_bytes(b"[1,2,3]".to_vec(), Path::new("package.json"))
            .unwrap_err();
        assert!(matches!(error, ManifestEditError::NotObject { .. }));
    }

    #[test]
    fn preserves_trailing_newline_policy() {
        let with_newline = doc("{\"name\":\"app\"}\n");
        let without_newline = doc("{\"name\":\"app\"}");
        assert!(with_newline.render().ends_with(b"\n"));
        assert!(!without_newline.render().ends_with(b"\n"));
    }

    #[test]
    fn no_op_remove_reports_no_change_and_real_remove_reports_change() {
        let mut document = doc(r#"{"name":"app","dependencies":{"a":"^1.0.0"}}"#);
        // A remove that finds nothing is a no-op signal so the orchestrator
        // can skip rewriting the file entirely.
        assert!(!document.remove_dependency("missing"));
        // A remove that hits a real entry reports the change.
        assert!(document.remove_dependency("a"));
        let manifest = document.to_manifest().unwrap();
        assert_eq!(manifest.dependency_count(), 0);
    }

    #[test]
    fn remove_strips_from_every_section() {
        let mut document = doc(
            r#"{"dependencies":{"a":"^1.0.0"},"devDependencies":{"a":"^1.0.0"},
            "optionalDependencies":{"a":"^1.0.0"},"peerDependencies":{"a":"^1.0.0"}}"#,
        );
        assert!(document.remove_dependency("a"));
        let manifest = document.to_manifest().unwrap();
        assert_eq!(manifest.dependency_count(), 0);
    }

    #[test]
    fn render_is_byte_stable_across_repeated_calls() {
        let mut document = doc(r#"{"dependencies":{"a":"^1.0.0"}}"#);
        document
            .add_dependency(DependencySection::Production, "b", "^2.0.0")
            .unwrap();
        let first = document.render();
        let second = document.render();
        assert_eq!(first, second);
    }

    #[test]
    fn scoped_names_are_preserved() {
        let mut document = doc(r#"{"dependencies":{}}"#);
        document
            .add_dependency(DependencySection::Production, "@scope/pkg", "^1.0.0")
            .unwrap();
        let manifest = document.to_manifest().unwrap();
        assert_eq!(
            manifest.dependencies.get("@scope/pkg").map(String::as_str),
            Some("^1.0.0")
        );
    }

    #[test]
    fn project_mutation_distinct_projects_lock_independently() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let first_guard = acquire_project_mutation_guard(first.path()).unwrap();
        let second_guard = acquire_project_mutation_guard(second.path()).unwrap();
        drop((first_guard, second_guard));
    }

    #[test]
    fn project_mutation_second_acquisition_blocks_until_drop() {
        let project = tempfile::tempdir().unwrap();
        let first_guard = acquire_project_mutation_guard(project.path()).unwrap();
        let project_path = project.path().to_path_buf();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let guard = acquire_project_mutation_guard(&project_path).unwrap();
            acquired_tx.send(()).unwrap();
            drop(guard);
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(
            acquired_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "second mutation lock unexpectedly acquired while the first was held"
        );
        drop(first_guard);
        acquired_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        waiter.join().unwrap();
    }

    #[test]
    fn project_mutation_guard_releases_during_error_unwind() {
        let project = tempfile::tempdir().unwrap();
        let operation = || -> io::Result<()> {
            let _guard =
                acquire_project_mutation_guard(project.path()).map_err(io::Error::other)?;
            Err(io::Error::other("injected operation failure"))
        };
        assert!(operation().is_err());
        acquire_project_mutation_guard(project.path()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn project_mutation_symlink_route_has_same_identity() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let alias = root.path().join("alias");
        fs::create_dir(&project).unwrap();
        symlink(&project, &alias).unwrap();
        assert_eq!(
            project_mutation_lock_path(&project).unwrap(),
            project_mutation_lock_path(&alias).unwrap()
        );
    }

    #[test]
    fn publish_bytes_replaces_existing_file_without_changing_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bpm.lock");
        fs::write(&path, b"old").unwrap();
        publish_bytes(&path, b"new\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new\n");
    }

    #[test]
    fn publish_bytes_creates_missing_parent_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("bpm.lock");
        publish_bytes(&path, b"lock\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"lock\n");
    }

    #[test]
    fn publish_bytes_failure_before_replace_preserves_old_bytes_and_cleans_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bpm.lock");
        fs::write(&path, b"old").unwrap();
        publish_bytes_impl(&path, b"new", true).unwrap_err();
        assert_eq!(fs::read(&path).unwrap(), b"old");
        let leftovers = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".bpm-"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn publish_bytes_staging_failure_leaves_no_sibling_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing-parent").join("bpm.lock");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::create_dir(path.with_file_name("bpm.lock")).unwrap();
        let error = publish_bytes(&path, b"new").unwrap_err();
        assert!(error.to_string().contains("bpm.lock"));
        let leftovers = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".bpm-"))
            .count();
        assert_eq!(leftovers, 0);
    }

    fn plan_for(dir: &Path, manifest: &str, lock: &str) -> PublishPlan {
        PublishPlan {
            manifest_path: dir.join("package.json"),
            manifest_bytes: manifest.as_bytes().to_vec(),
            lock_path: dir.join("bpm.lock"),
            lock_bytes: lock.as_bytes().to_vec(),
        }
    }

    #[test]
    fn publish_writes_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_for(dir.path(), r#"{"name":"app"}"#, r#"{"lock":"v1"}"#);
        publish(&plan).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("package.json")).unwrap(),
            r#"{"name":"app"}"#
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("bpm.lock")).unwrap(),
            r#"{"lock":"v1"}"#
        );
    }

    #[test]
    fn injected_failure_before_lock_restores_both_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), r#"{"name":"old"}"#).unwrap();
        fs::write(dir.path().join("bpm.lock"), r#"{"lock":"old"}"#).unwrap();
        let plan = plan_for(dir.path(), r#"{"name":"new"}"#, r#"{"lock":"new"}"#);
        let error = publish_with_failure(&plan, PublishStage::Lock).unwrap_err();
        assert!(error.to_string().contains("injected failure"));
        assert_eq!(
            fs::read_to_string(dir.path().join("package.json")).unwrap(),
            r#"{"name":"old"}"#
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("bpm.lock")).unwrap(),
            r#"{"lock":"old"}"#
        );
    }

    #[test]
    fn injected_failure_before_manifest_restores_both_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), r#"{"name":"old"}"#).unwrap();
        fs::write(dir.path().join("bpm.lock"), r#"{"lock":"old"}"#).unwrap();
        let plan = plan_for(dir.path(), r#"{"name":"new"}"#, r#"{"lock":"new"}"#);
        publish_with_failure(&plan, PublishStage::Manifest).unwrap_err();
        // The lock was published and then rolled back to its original bytes.
        assert_eq!(
            fs::read_to_string(dir.path().join("bpm.lock")).unwrap(),
            r#"{"lock":"old"}"#
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("package.json")).unwrap(),
            r#"{"name":"old"}"#
        );
        // No temp files left behind.
        let leftover = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".bpm-"))
            .count();
        assert_eq!(leftover, 0);
    }

    #[test]
    fn injected_failure_restores_absence_when_files_did_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_for(dir.path(), r#"{"name":"new"}"#, r#"{"lock":"new"}"#);
        publish_with_failure(&plan, PublishStage::Manifest).unwrap_err();
        assert!(!dir.path().join("package.json").exists());
        assert!(!dir.path().join("bpm.lock").exists());
    }

    #[test]
    fn publish_error_reports_complete_rollback_truthfully() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), b"old manifest").unwrap();
        fs::write(dir.path().join("bpm.lock"), b"old lock").unwrap();
        let plan = plan_for(dir.path(), "new manifest", "new lock");
        let error = publish_with_failure(&plan, PublishStage::Manifest).unwrap_err();
        assert!(error.publication_began());
        assert!(error.rollback_complete());
        assert!(error
            .to_string()
            .contains("both destinations were restored"));
        assert!(!error.to_string().contains("old manifest"));
        assert!(!error.to_string().contains("old lock"));
    }

    #[test]
    fn publish_error_collects_both_restore_failures() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), b"old manifest").unwrap();
        fs::write(dir.path().join("bpm.lock"), b"old lock").unwrap();
        let plan = plan_for(dir.path(), "new manifest", "new lock");
        let error = publish_impl(
            &plan,
            Some(PublishStage::Manifest),
            RestoreFaults {
                lock: true,
                manifest: true,
            },
        )
        .unwrap_err();
        assert!(!error.rollback_complete());
        assert_eq!(error.rollback_failures().len(), 2);
        assert_eq!(
            error.rollback_failures()[0].destination,
            RollbackDestination::Lock
        );
        assert_eq!(
            error.rollback_failures()[1].destination,
            RollbackDestination::Manifest
        );
        assert!(error.to_string().contains("rollback incomplete"));
        assert!(!error.to_string().contains("old manifest"));
        assert!(!error.to_string().contains("old lock"));
    }

    #[test]
    fn failed_lock_restore_does_not_skip_manifest_restore() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), b"old manifest").unwrap();
        fs::write(dir.path().join("bpm.lock"), b"old lock").unwrap();
        let plan = plan_for(dir.path(), "new manifest", "new lock");
        let error = publish_impl(
            &plan,
            Some(PublishStage::Manifest),
            RestoreFaults {
                lock: true,
                manifest: false,
            },
        )
        .unwrap_err();
        assert_eq!(error.rollback_failures().len(), 1);
        assert_eq!(
            fs::read(dir.path().join("package.json")).unwrap(),
            b"old manifest"
        );
    }
}
