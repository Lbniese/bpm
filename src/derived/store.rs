//! Immutable lifecycle-derived image storage.
//!
//! This module owns the filesystem protocol only. Lifecycle execution is
//! injected as a sandbox callback, filesystem backend selection remains the
//! caller's responsibility, and [`DerivedMetadata`] is the explicit adapter
//! boundary for the M3 repository. Filesystem metadata is authoritative: a hit
//! is accepted only after both `metadata.json` and `image/` validate.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::key::{derived_key, DerivedInputs, DerivedKey};

const DERIVED: &str = "derived/blake3";
const TMP: &str = "tmp";
const LOCKS: &str = "locks";
const METADATA_SCHEMA: u32 = 1;
const MAX_CAPTURE_BYTES: usize = 16 * 1024;

/// Repository-facing description of a published derived object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedRecord {
    pub id: String,
    pub source_artifact_id: String,
    pub rel_path: String,
    pub size_bytes: u64,
    pub published_at_ms: u64,
}

/// Narrow integration boundary implemented by the M3 metadata repository.
///
/// `access_derived` must transactionally repair/upsert `record` and update its
/// access timestamp. This lets a valid filesystem object repair a missing row,
/// while database-only state can never create a cache hit.
pub trait DerivedMetadata {
    fn publish_derived(&self, record: &DerivedRecord) -> Result<(), String>;
    fn access_derived(&self, record: &DerivedRecord, accessed_at_ms: u64) -> Result<(), String>;
}

/// No-op [`DerivedMetadata`] for filesystem-authoritative operation.
///
/// Before the M3 metadata repository is wired, the derived store is usable
/// the moment a store root exists: the filesystem (`metadata.json` + `image/`)
/// is the sole source of truth, and [`DerivedStore::ensure`] accepts a hit
/// only after re-validating it on disk. This adapter satisfies the trait
/// without persisting anywhere. The LRU/GC integration that consumes access
/// timestamps arrives with the metadata repository in a later phase.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullDerivedMetadata;

impl DerivedMetadata for NullDerivedMetadata {
    fn publish_derived(&self, _record: &DerivedRecord) -> Result<(), String> {
        Ok(())
    }

    fn access_derived(&self, _record: &DerivedRecord, _accessed_at_ms: u64) -> Result<(), String> {
        Ok(())
    }
}

/// Options whose behavior does not belong to the cache key itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EnsureOptions {
    pub ignore_scripts: bool,
}

/// A validated reference to one immutable derived image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedRef {
    pub key: DerivedKey,
    pub image_path: PathBuf,
    pub metadata_path: PathBuf,
}

/// Result of ensuring a lifecycle-derived image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsureDerived {
    Hit(DerivedRef),
    Built(DerivedRef),
    Skipped,
}

/// Bounded subprocess output and status supplied by lifecycle integration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxFailure {
    pub package: String,
    pub phase: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl SandboxFailure {
    pub fn new(
        package: impl Into<String>,
        phase: impl Into<String>,
        exit_code: Option<i32>,
        signal: Option<i32>,
        stdout: &[u8],
        stderr: &[u8],
    ) -> Self {
        Self {
            package: package.into(),
            phase: phase.into(),
            exit_code,
            signal,
            stdout: bounded(stdout),
            stderr: bounded(stderr),
        }
    }
}

impl fmt::Display for SandboxFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "lifecycle failed for {} {} (exit={:?}, signal={:?}, stdout={} bytes, stderr={} bytes)",
            self.package,
            self.phase,
            self.exit_code,
            self.signal,
            self.stdout.len(),
            self.stderr.len()
        )
    }
}

#[derive(Debug, Error)]
pub enum DerivedError {
    #[error("derived store io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid derived image at {path}: {reason}")]
    InvalidImage { path: String, reason: String },
    #[error("derived metadata operation failed: {0}")]
    Metadata(String),
    #[error("{failure}")]
    Sandbox { failure: SandboxFailure },
    #[error("could not acquire derived lock at {path}: {source}")]
    Lock {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("could not serialize derived metadata: {0}")]
    Json(#[from] serde_json::Error),
}

/// Store facade rooted at the same directory as the artifact store.
pub struct DerivedStore<'a, M: DerivedMetadata> {
    root: PathBuf,
    metadata: &'a M,
}

impl<'a, M: DerivedMetadata> DerivedStore<'a, M> {
    /// Open the derived-store view of an artifact-store root.
    ///
    /// Callers currently pass `ArtifactStore::root()`; a direct constructor is
    /// intentionally deferred to `src/derived/mod.rs`, avoiding a cyclic module
    /// dependency before the receptionist is wired.
    pub fn open(store_root: &Path, metadata: &'a M) -> Result<Self, DerivedError> {
        fs::create_dir_all(store_root.join(TMP)).map_err(|source| io_err(store_root, source))?;
        fs::create_dir_all(store_root.join(LOCKS)).map_err(|source| io_err(store_root, source))?;
        Ok(Self {
            root: store_root.to_path_buf(),
            metadata,
        })
    }

    pub fn derived_path(&self, key: &DerivedKey) -> PathBuf {
        let hex = key.to_hex();
        self.root.join(DERIVED).join(&hex[..2]).join(hex)
    }

    /// Capture a mutable sandbox into one immutable, atomically published image.
    ///
    /// The callback runs while the per-key lock is held and receives only the
    /// staging image path. It must execute dependency lifecycles before their
    /// dependents and return [`SandboxFailure`] for any required failure.
    pub fn ensure<F>(
        &self,
        inputs: &DerivedInputs<'_>,
        source_image: &Path,
        options: EnsureOptions,
        build: F,
    ) -> Result<EnsureDerived, DerivedError>
    where
        F: FnOnce(&Path) -> Result<(), SandboxFailure>,
    {
        if options.ignore_scripts {
            return Ok(EnsureDerived::Skipped);
        }

        let key = derived_key(inputs);
        let _lock = self.acquire_lock(&key)?;
        let destination = self.derived_path(&key);
        if destination.exists() {
            let (reference, record) = self.validate_published(&key, &destination)?;
            self.metadata
                .access_derived(&record, now_ms())
                .map_err(DerivedError::Metadata)?;
            return Ok(EnsureDerived::Hit(reference));
        }

        let staging_path = self.unique_staging(&key);
        let mut staging = StagingDir::create(staging_path)?;
        let staging_image = staging.path().join("image");
        copy_tree(source_image, &staging_image, source_image)?;
        build(&staging_image).map_err(|failure| DerivedError::Sandbox { failure })?;
        validate_tree(&staging_image, &staging_image)?;

        let (tree_digest, size_bytes) = tree_identity(&staging_image)?;
        let published_at_ms = now_ms();
        let persisted =
            PersistedMetadata::new(&key, inputs, tree_digest, size_bytes, published_at_ms);
        write_metadata(staging.path(), &persisted)?;
        seal_tree_contents(staging.path())?;

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|source| io_err(parent, source))?;
        }
        match fs::rename(staging.path(), &destination) {
            Ok(()) => staging.disarm(),
            Err(_) if destination.exists() => {
                let (reference, record) = self.validate_published(&key, &destination)?;
                self.metadata
                    .access_derived(&record, now_ms())
                    .map_err(DerivedError::Metadata)?;
                return Ok(EnsureDerived::Hit(reference));
            }
            Err(source) => return Err(io_err(&destination, source)),
        }
        seal_path(&destination)?;

        let reference = derived_ref(key, &destination);
        let record = record_for(&self.root, &persisted, &destination)?;
        self.metadata
            .publish_derived(&record)
            .map_err(DerivedError::Metadata)?;
        Ok(EnsureDerived::Built(reference))
    }

    fn validate_published(
        &self,
        key: &DerivedKey,
        destination: &Path,
    ) -> Result<(DerivedRef, DerivedRecord), DerivedError> {
        let metadata_path = destination.join("metadata.json");
        let bytes = fs::read(&metadata_path).map_err(|source| io_err(&metadata_path, source))?;
        let persisted: PersistedMetadata = serde_json::from_slice(&bytes)?;
        if persisted.schema != METADATA_SCHEMA || persisted.key != key.to_hex() {
            return Err(invalid(
                destination,
                "metadata schema or key does not match",
            ));
        }
        let image = destination.join("image");
        validate_tree(&image, &image)?;
        let (tree_digest, size_bytes) = tree_identity(&image)?;
        if persisted.tree_digest != tree_digest || persisted.size_bytes != size_bytes {
            return Err(invalid(
                destination,
                "image tree identity does not match metadata",
            ));
        }
        let record = record_for(&self.root, &persisted, destination)?;
        Ok((derived_ref(*key, destination), record))
    }

    fn acquire_lock(&self, key: &DerivedKey) -> Result<FileGuard, DerivedError> {
        let path = self
            .root
            .join(LOCKS)
            .join(format!("derived-{}.lock", key.to_hex()));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| DerivedError::Lock {
                path: path.display().to_string(),
                source,
            })?;
        file.lock().map_err(|source| DerivedError::Lock {
            path: path.display().to_string(),
            source,
        })?;
        Ok(FileGuard(file))
    }

    fn unique_staging(&self, key: &DerivedKey) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        self.root.join(TMP).join(format!(
            "derived-{}-{}-{counter}",
            key.to_hex(),
            std::process::id()
        ))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedMetadata {
    schema: u32,
    key: String,
    source_artifact: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_revision: Option<String>,
    dependency_graph: String,
    target: PersistedTarget,
    runtime: PersistedRuntime,
    script_digests: BTreeMap<String, String>,
    runner_version: u32,
    policy_version: u32,
    size_bytes: u64,
    published_at_ms: u64,
    tree_digest: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedTarget {
    os: String,
    architecture: String,
    family: String,
    abi: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedRuntime {
    executable_digest: String,
    version: String,
    modules_abi: String,
    napi_version: Option<String>,
}

impl PersistedMetadata {
    fn new(
        key: &DerivedKey,
        inputs: &DerivedInputs<'_>,
        tree_digest: String,
        size_bytes: u64,
        published_at_ms: u64,
    ) -> Self {
        let script_digests = inputs
            .scripts
            .iter()
            .map(|(phase, command)| {
                (
                    phase.clone(),
                    blake3::hash(command.as_bytes()).to_hex().to_string(),
                )
            })
            .collect();
        Self {
            schema: METADATA_SCHEMA,
            key: key.to_hex(),
            source_artifact: hex::encode(inputs.source_artifact),
            source_revision: inputs.source_revision.map(str::to_owned),
            dependency_graph: hex::encode(inputs.dependency_graph),
            target: PersistedTarget {
                os: inputs.target.os.to_owned(),
                architecture: inputs.target.architecture.to_owned(),
                family: inputs.target.family.to_owned(),
                abi: inputs.target.abi.to_owned(),
            },
            runtime: PersistedRuntime {
                executable_digest: blake3::hash(inputs.runtime.executable).to_hex().to_string(),
                version: inputs.runtime.version.to_owned(),
                modules_abi: inputs.runtime.modules_abi.to_owned(),
                napi_version: inputs.runtime.napi_version.map(str::to_owned),
            },
            script_digests,
            runner_version: inputs.runner_version,
            policy_version: inputs.policy_version,
            size_bytes,
            published_at_ms,
            tree_digest,
        }
    }
}

fn write_metadata(root: &Path, metadata: &PersistedMetadata) -> Result<(), DerivedError> {
    let path = root.join("metadata.json");
    let mut bytes = serde_json::to_vec(metadata)?;
    bytes.push(b'\n');
    let mut file = File::create(&path).map_err(|source| io_err(&path, source))?;
    file.write_all(&bytes)
        .map_err(|source| io_err(&path, source))?;
    file.sync_all().map_err(|source| io_err(&path, source))
}

fn copy_tree(source: &Path, destination: &Path, source_root: &Path) -> Result<(), DerivedError> {
    let metadata = fs::symlink_metadata(source).map_err(|error| io_err(source, error))?;
    if !metadata.file_type().is_dir() {
        return Err(invalid(source, "source image root is not a directory"));
    }
    fs::create_dir(destination).map_err(|source| io_err(destination, source))?;
    for entry in sorted_entries(source)? {
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|error| io_err(&source_path, error))?;
        if file_type.is_dir() {
            copy_tree(&source_path, &destination_path, source_root)?;
        } else if file_type.is_file() {
            if !try_clone_file(&source_path, &destination_path) {
                fs::copy(&source_path, &destination_path)
                    .map_err(|error| io_err(&destination_path, error))?;
            }
        } else if file_type.is_symlink() {
            let target =
                fs::read_link(&source_path).map_err(|error| io_err(&source_path, error))?;
            validate_link_target(source_root, &source_path, &target)?;
            create_symlink(&target, &destination_path)?;
        } else {
            return Err(invalid(&source_path, "special files are not allowed"));
        }
    }
    Ok(())
}

fn try_clone_file(source: &Path, destination: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("cp")
            .args([
                "--reflink=always",
                "--",
                source.to_string_lossy().as_ref(),
                destination.to_string_lossy().as_ref(),
            ])
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (source, destination);
        false
    }
}

fn validate_tree(path: &Path, root: &Path) -> Result<(), DerivedError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_err(path, error))?;
    if !metadata.file_type().is_dir() {
        return Err(invalid(path, "image root is not a directory"));
    }
    for entry in sorted_entries(path)? {
        let child = entry.path();
        let file_type = entry.file_type().map_err(|error| io_err(&child, error))?;
        if file_type.is_dir() {
            validate_tree(&child, root)?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(&child).map_err(|error| io_err(&child, error))?;
            validate_link_target(root, &child, &target)?;
        } else if !file_type.is_file() {
            return Err(invalid(&child, "special files are not allowed"));
        }
    }
    Ok(())
}

fn validate_link_target(root: &Path, link: &Path, target: &Path) -> Result<(), DerivedError> {
    if target.is_absolute() {
        return Err(invalid(link, "absolute symlink target is not allowed"));
    }
    let parent = link
        .parent()
        .ok_or_else(|| invalid(link, "symlink has no parent"))?;
    let relative_parent = parent
        .strip_prefix(root)
        .map_err(|_| invalid(link, "symlink is outside the image root"))?;
    let mut depth = relative_parent.components().count();
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir => return Err(invalid(link, "symlink target escapes image root")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(invalid(link, "prefixed symlink target is not allowed"))
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, destination: &Path) -> Result<(), DerivedError> {
    std::os::unix::fs::symlink(target, destination).map_err(|source| io_err(destination, source))
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, destination: &Path) -> Result<(), DerivedError> {
    Err(invalid(
        destination,
        "symlink sandbox capture requires the portable backend adapter",
    ))
}

fn tree_identity(root: &Path) -> Result<(String, u64), DerivedError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"bpm-derived-tree-v1\0");
    let mut size = 0_u64;
    hash_tree(root, root, &mut hasher, &mut size)?;
    Ok((hasher.finalize().to_hex().to_string(), size))
}

fn hash_tree(
    root: &Path,
    directory: &Path,
    hasher: &mut blake3::Hasher,
    size: &mut u64,
) -> Result<(), DerivedError> {
    for entry in sorted_entries(directory)? {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| invalid(&path, "tree entry is outside image root"))?;
        let path_bytes = relative.as_os_str().as_encoded_bytes();
        let file_type = entry.file_type().map_err(|error| io_err(&path, error))?;
        if file_type.is_dir() {
            hash_field(hasher, b'd', path_bytes);
            hash_tree(root, &path, hasher, size)?;
        } else if file_type.is_file() {
            hash_field(hasher, b'f', path_bytes);
            let mut file = File::open(&path).map_err(|error| io_err(&path, error))?;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = file
                    .read(&mut buffer)
                    .map_err(|error| io_err(&path, error))?;
                if count == 0 {
                    break;
                }
                *size = size
                    .checked_add(count as u64)
                    .ok_or_else(|| invalid(root, "image size exceeds u64"))?;
                hasher.update(&buffer[..count]);
            }
        } else if file_type.is_symlink() {
            hash_field(hasher, b'l', path_bytes);
            let target = fs::read_link(&path).map_err(|error| io_err(&path, error))?;
            hash_field(hasher, b't', target.as_os_str().as_encoded_bytes());
        } else {
            return Err(invalid(&path, "special files are not allowed"));
        }
    }
    Ok(())
}

fn hash_field(hasher: &mut blake3::Hasher, kind: u8, bytes: &[u8]) {
    hasher.update(&[kind]);
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn sorted_entries(path: &Path) -> Result<Vec<fs::DirEntry>, DerivedError> {
    let mut entries = fs::read_dir(path)
        .map_err(|source| io_err(path, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| io_err(path, source))?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

fn seal_tree(path: &Path) -> Result<(), DerivedError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_err(path, source))?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in sorted_entries(path)? {
            seal_tree(&entry.path())?;
        }
    }
    seal_path_with_metadata(path, metadata)
}

fn seal_tree_contents(path: &Path) -> Result<(), DerivedError> {
    for entry in sorted_entries(path)? {
        seal_tree(&entry.path())?;
    }
    Ok(())
}

fn seal_path(path: &Path) -> Result<(), DerivedError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_err(path, source))?;
    seal_path_with_metadata(path, metadata)
}

fn seal_path_with_metadata(path: &Path, metadata: fs::Metadata) -> Result<(), DerivedError> {
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    // Directories are intentionally left writable. Content immutability only
    // requires the regular files to be read-only, and GC reclaims derived
    // images with remove_dir_all, which needs write permission on each
    // directory to unlink its entries. Sealing directories read-only would
    // make a published image undeletable, so disk growth would be unbounded.
    if metadata.is_dir() {
        return Ok(());
    }
    let mut permissions = metadata.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(permissions.mode() & !0o222);
    }
    #[cfg(not(unix))]
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions).map_err(|source| io_err(path, source))
}

fn record_for(
    store_root: &Path,
    metadata: &PersistedMetadata,
    destination: &Path,
) -> Result<DerivedRecord, DerivedError> {
    let relative = destination
        .strip_prefix(store_root)
        .map_err(|_| invalid(destination, "derived object is outside store root"))?;
    let rel_path = relative
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| invalid(destination, "store path is not valid UTF-8 metadata"))?
        .join("/");
    Ok(DerivedRecord {
        id: metadata.key.clone(),
        source_artifact_id: metadata.source_artifact.clone(),
        rel_path,
        size_bytes: metadata.size_bytes,
        published_at_ms: metadata.published_at_ms,
    })
}

fn derived_ref(key: DerivedKey, destination: &Path) -> DerivedRef {
    DerivedRef {
        key,
        image_path: destination.join("image"),
        metadata_path: destination.join("metadata.json"),
    }
}

fn bounded(bytes: &[u8]) -> Vec<u8> {
    bytes[..bytes.len().min(MAX_CAPTURE_BYTES)].to_vec()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn io_err(path: &Path, source: std::io::Error) -> DerivedError {
    DerivedError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn invalid(path: &Path, reason: impl Into<String>) -> DerivedError {
    DerivedError::InvalidImage {
        path: path.display().to_string(),
        reason: reason.into(),
    }
}

struct FileGuard(File);

impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

struct StagingDir {
    path: PathBuf,
    armed: bool,
}

impl StagingDir {
    fn create(path: PathBuf) -> Result<Self, DerivedError> {
        fs::create_dir(&path).map_err(|source| io_err(&path, source))?;
        Ok(Self { path, armed: true })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if self.armed {
            let _ = make_writable(&self.path);
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn make_writable(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        let mut permissions = metadata.permissions();
        set_writable(&mut permissions);
        fs::set_permissions(path, permissions)?;
        for entry in fs::read_dir(path)? {
            make_writable(&entry?.path())?;
        }
    } else if !metadata.file_type().is_symlink() {
        let mut permissions = metadata.permissions();
        set_writable(&mut permissions);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_writable(permissions: &mut fs::Permissions) {
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(permissions.mode() | 0o700);
}

#[cfg(not(unix))]
fn set_writable(permissions: &mut fs::Permissions) {
    permissions.set_readonly(false);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::sync::Mutex;

    use super::super::key::{RuntimeIdentity, TargetDescriptor};
    use super::*;

    #[derive(Default)]
    struct FakeMetadata {
        published: Mutex<Vec<String>>,
        accessed: Mutex<Vec<String>>,
    }

    impl DerivedMetadata for FakeMetadata {
        fn publish_derived(&self, record: &DerivedRecord) -> Result<(), String> {
            self.published.lock().unwrap().push(record.id.clone());
            Ok(())
        }

        fn access_derived(
            &self,
            record: &DerivedRecord,
            _accessed_at_ms: u64,
        ) -> Result<(), String> {
            self.accessed.lock().unwrap().push(record.id.clone());
            Ok(())
        }
    }

    fn inputs<'a>(
        scripts: &'a BTreeMap<String, String>,
        environment: &'a BTreeMap<OsString, OsString>,
    ) -> DerivedInputs<'a> {
        static GRAPH: [u8; 32] = [2; 32];
        inputs_with_graph(scripts, environment, &GRAPH)
    }

    fn inputs_with_graph<'a>(
        scripts: &'a BTreeMap<String, String>,
        environment: &'a BTreeMap<OsString, OsString>,
        graph: &'a [u8; 32],
    ) -> DerivedInputs<'a> {
        static SOURCE: [u8; 64] = [1; 64];
        DerivedInputs {
            source_artifact: &SOURCE,
            source_revision: None,
            dependency_graph: graph,
            target: TargetDescriptor {
                os: "linux",
                architecture: "x86_64",
                family: "unix",
                abi: "gnu",
            },
            runtime: RuntimeIdentity {
                executable: b"node-digest",
                version: "22.17.0",
                modules_abi: "127",
                napi_version: Some("10"),
            },
            phases: &["preinstall", "install", "postinstall"],
            scripts,
            environment,
            runner_version: 1,
            policy_version: 1,
        }
    }

    /// Read the persisted tree identity back from a published image.
    fn tree_digest_of(reference: &DerivedRef) -> String {
        let bytes = fs::read(&reference.metadata_path).unwrap();
        let persisted: PersistedMetadata = serde_json::from_slice(&bytes).unwrap();
        persisted.tree_digest
    }

    /// Mirror of the inject -> build -> strip contract the lifecycle layer will
    /// use as its `DerivedStore::ensure` build callback: remove exactly the
    /// injected dependency subtrees and drop the `node_modules/` container when
    /// it is left empty, so the published image is a deterministic function of
    /// the package's own post-lifecycle tree.
    fn strip_injected_deps(image: &Path, injected: &[&str]) {
        let node_modules = image.join("node_modules");
        for name in injected {
            let path = node_modules.join(name);
            if path.exists() {
                fs::remove_dir_all(&path).unwrap();
            }
        }
        if node_modules.exists() && fs::read_dir(&node_modules).unwrap().next().is_none() {
            fs::remove_dir(&node_modules).unwrap();
        }
    }

    #[test]
    fn immutable_publication_is_reused_without_mutating_source() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("base"), b"base").unwrap();
        let scripts = BTreeMap::from([("install".to_owned(), "build".to_owned())]);
        let environment = BTreeMap::from([(
            OsString::from("SECRET_TOKEN"),
            OsString::from("never-persist-me"),
        )]);
        let metadata = FakeMetadata::default();
        let store = DerivedStore::open(&temp.path().join("store"), &metadata).unwrap();

        let built = store
            .ensure(
                &inputs(&scripts, &environment),
                &source,
                EnsureOptions::default(),
                |sandbox| {
                    fs::write(sandbox.join("generated"), b"output").unwrap();
                    Ok(())
                },
            )
            .unwrap();
        let reference = match built {
            EnsureDerived::Built(reference) => reference,
            other => panic!("expected built object, got {other:?}"),
        };
        assert_eq!(fs::read(source.join("base")).unwrap(), b"base");
        assert!(!source.join("generated").exists());
        assert_eq!(
            fs::read(reference.image_path.join("generated")).unwrap(),
            b"output"
        );
        let persisted = fs::read(&reference.metadata_path).unwrap();
        assert!(!persisted
            .windows(b"never-persist-me".len())
            .any(|window| window == b"never-persist-me"));

        let hit = store
            .ensure(
                &inputs(&scripts, &environment),
                &source,
                EnsureOptions::default(),
                |_| panic!("cache hit reran lifecycle builder"),
            )
            .unwrap();
        assert!(matches!(hit, EnsureDerived::Hit(_)));
        assert_eq!(metadata.published.lock().unwrap().len(), 1);
        assert_eq!(metadata.accessed.lock().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn published_image_dirs_remain_writable_so_gc_can_reclaim() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("base"), b"base").unwrap();
        fs::write(source.join("nested").join("leaf"), b"leaf").unwrap();
        let scripts = BTreeMap::from([("install".to_owned(), "build".to_owned())]);
        let environment = BTreeMap::new();
        let metadata = FakeMetadata::default();
        let store = DerivedStore::open(&temp.path().join("store"), &metadata).unwrap();
        let built = store
            .ensure(
                &inputs(&scripts, &environment),
                &source,
                EnsureOptions::default(),
                |sandbox| {
                    fs::write(sandbox.join("generated"), b"out").unwrap();
                    Ok(())
                },
            )
            .unwrap();
        let reference = match built {
            EnsureDerived::Built(reference) => reference,
            other => panic!("expected built object, got {other:?}"),
        };
        // Regular files are sealed read-only for content immutability...
        let file_mode = fs::metadata(reference.image_path.join("base"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(file_mode & 0o222, 0, "published files must be read-only");
        // ...but directories stay writable so GC's remove_dir_all can unlink
        // entries -- sealing dirs read-only would make images undeletable and
        // disk growth unbounded.
        let dir_mode = fs::metadata(&reference.image_path)
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(dir_mode & 0o200, 0, "image dir must stay writable for GC");
        let nested_mode = fs::metadata(reference.image_path.join("nested"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(
            nested_mode & 0o200,
            0,
            "nested dirs must stay writable for GC"
        );
        // Concrete proof: the published tree is reclaimable.
        let destination = reference.image_path.parent().unwrap();
        fs::remove_dir_all(destination)
            .expect("GC must be able to remove a published derived image");
    }

    #[test]
    fn skipped_and_failed_builds_publish_nothing_and_bound_output() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let scripts = BTreeMap::new();
        let environment = BTreeMap::new();
        let metadata = FakeMetadata::default();
        let store = DerivedStore::open(&temp.path().join("store"), &metadata).unwrap();

        assert!(matches!(
            store
                .ensure(
                    &inputs(&scripts, &environment),
                    &source,
                    EnsureOptions {
                        ignore_scripts: true
                    },
                    |_| panic!("ignore-scripts ran builder"),
                )
                .unwrap(),
            EnsureDerived::Skipped
        ));

        let output = vec![b'x'; MAX_CAPTURE_BYTES + 10];
        let error = store
            .ensure(
                &inputs(&scripts, &environment),
                &source,
                EnsureOptions::default(),
                |_| {
                    Err(SandboxFailure::new(
                        "pkg",
                        "install",
                        Some(9),
                        None,
                        &output,
                        &output,
                    ))
                },
            )
            .unwrap_err();
        let DerivedError::Sandbox { failure } = error else {
            panic!("expected sandbox failure");
        };
        assert_eq!(failure.stdout.len(), MAX_CAPTURE_BYTES);
        assert_eq!(failure.stderr.len(), MAX_CAPTURE_BYTES);
        assert!(!temp.path().join("store").join(DERIVED).exists());
    }

    #[cfg(unix)]
    #[test]
    fn escaping_symlink_is_rejected_before_builder_runs() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        std::os::unix::fs::symlink("../outside", source.join("escape")).unwrap();
        let scripts = BTreeMap::new();
        let environment = BTreeMap::new();
        let metadata = FakeMetadata::default();
        let store = DerivedStore::open(&temp.path().join("store"), &metadata).unwrap();

        let error = store
            .ensure(
                &inputs(&scripts, &environment),
                &source,
                EnsureOptions::default(),
                |_| panic!("invalid source reached builder"),
            )
            .unwrap_err();
        assert!(matches!(error, DerivedError::InvalidImage { .. }));
        assert!(!temp.path().join("store").join(DERIVED).exists());
    }

    #[test]
    fn injected_dependency_is_excluded_from_image_but_readable_during_build() {
        // The lifecycle build callback injects the package's dependency
        // subtree so scripts can resolve it, then strips the injected entries
        // before returning. The published derived image must contain the
        // package's own post-lifecycle tree only -- never the injected deps.
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("pkg.js"), b"module.exports = 1;").unwrap();
        let scripts = BTreeMap::from([("install".to_owned(), "node build.js".to_owned())]);
        let environment = BTreeMap::new();
        let metadata = FakeMetadata::default();
        let store = DerivedStore::open(&temp.path().join("store"), &metadata).unwrap();

        let built = store
            .ensure(
                &inputs(&scripts, &environment),
                &source,
                EnsureOptions::default(),
                |sandbox| {
                    let dep = sandbox.join("node_modules").join("my-dep");
                    fs::create_dir_all(&dep).unwrap();
                    fs::write(dep.join("package.json"), b"{\"name\":\"my-dep\"}").unwrap();
                    // The script reads the injected dependency during build.
                    let manifest = fs::read_to_string(dep.join("package.json")).unwrap();
                    fs::write(
                        sandbox.join("built.js"),
                        format!("// built against {manifest}"),
                    )
                    .unwrap();
                    strip_injected_deps(sandbox, &["my-dep"]);
                    Ok(())
                },
            )
            .unwrap();
        let reference = match built {
            EnsureDerived::Built(reference) => reference,
            other => panic!("expected built object, got {other:?}"),
        };

        // Injected dependency is absent from the published image ...
        assert!(!reference.image_path.join("node_modules").exists());
        // ... but the derived output that consumed it is present.
        let published = fs::read_to_string(reference.image_path.join("built.js")).unwrap();
        assert!(published.contains("built against {\"name\":\"my-dep\"}"));
        // The source image is never mutated by the build.
        assert!(!source.join("node_modules").exists());
        assert!(!source.join("built.js").exists());
    }

    #[test]
    fn strip_is_surgical_script_created_node_modules_entry_survives() {
        // Stripping removes only the paths bpm injected. Content the lifecycle
        // script itself wrote -- even inside `node_modules/` -- is derived
        // output and must survive into the published image.
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("pkg.js"), b"module.exports = 1;").unwrap();
        let scripts = BTreeMap::from([("postinstall".to_owned(), "node gen.js".to_owned())]);
        let environment = BTreeMap::new();
        let metadata = FakeMetadata::default();
        let store = DerivedStore::open(&temp.path().join("store"), &metadata).unwrap();

        let built = store
            .ensure(
                &inputs(&scripts, &environment),
                &source,
                EnsureOptions::default(),
                |sandbox| {
                    let dep = sandbox.join("node_modules").join("my-dep");
                    fs::create_dir_all(&dep).unwrap();
                    fs::write(dep.join("package.json"), b"{}").unwrap();
                    // Script writes a sibling entry that bpm did not inject.
                    let generated = sandbox.join("node_modules").join("generated");
                    fs::create_dir_all(&generated).unwrap();
                    fs::write(generated.join("extra.js"), b"// derived").unwrap();
                    strip_injected_deps(sandbox, &["my-dep"]);
                    Ok(())
                },
            )
            .unwrap();
        let reference = match built {
            EnsureDerived::Built(reference) => reference,
            other => panic!("expected built object, got {other:?}"),
        };

        assert!(!reference.image_path.join("node_modules/my-dep").exists());
        assert_eq!(
            fs::read(reference.image_path.join("node_modules/generated/extra.js")).unwrap(),
            b"// derived"
        );
    }

    #[test]
    fn stripped_dependencies_do_not_fold_into_published_identity() {
        // Two builds with different dependency graphs inject different deps,
        // produce identical derived output, and strip their deps. Their cache
        // keys differ (dependency identity is captured by the key) but their
        // published tree identities match -- proving the derived image is a
        // pure function of the package's own post-lifecycle tree, not of the
        // stripped build-time dependencies.
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("pkg.js"), b"module.exports = 1;").unwrap();
        let scripts = BTreeMap::from([("install".to_owned(), "node build.js".to_owned())]);
        let environment = BTreeMap::new();
        let metadata = FakeMetadata::default();
        let store = DerivedStore::open(&temp.path().join("store"), &metadata).unwrap();

        let graph_a: [u8; 32] = [2; 32];
        let graph_b: [u8; 32] = [3; 32];
        let inject_and_build = |sandbox: &Path, dep: &str| {
            let dir = sandbox.join("node_modules").join(dep);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("package.json"), b"{}").unwrap();
            fs::write(sandbox.join("built.js"), b"compiled-output").unwrap();
            strip_injected_deps(sandbox, &[dep]);
            Ok(())
        };

        let built_a = store
            .ensure(
                &inputs_with_graph(&scripts, &environment, &graph_a),
                &source,
                EnsureOptions::default(),
                |sandbox| inject_and_build(sandbox, "dep-a"),
            )
            .unwrap();
        let built_b = store
            .ensure(
                &inputs_with_graph(&scripts, &environment, &graph_b),
                &source,
                EnsureOptions::default(),
                |sandbox| inject_and_build(sandbox, "dep-b"),
            )
            .unwrap();
        let reference_a = match built_a {
            EnsureDerived::Built(reference) => reference,
            other => panic!("expected built object, got {other:?}"),
        };
        let reference_b = match built_b {
            EnsureDerived::Built(reference) => reference,
            other => panic!("expected built object, got {other:?}"),
        };

        // Dependency identity is captured by the key, so the keys differ ...
        assert_ne!(reference_a.key, reference_b.key);
        // ... but the published images are byte-identical (deps were stripped).
        assert_eq!(tree_digest_of(&reference_a), tree_digest_of(&reference_b));
        assert_eq!(
            fs::read(reference_a.image_path.join("built.js")).unwrap(),
            fs::read(reference_b.image_path.join("built.js")).unwrap()
        );
        assert!(!reference_a.image_path.join("node_modules").exists());
        assert!(!reference_b.image_path.join("node_modules").exists());
    }
}
