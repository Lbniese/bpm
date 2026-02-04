//! Immutable artifact store (IMPLEMENTATION §6, §7).
//!
//! Layout under the store root:
//! ```text
//! artifacts/sha512/<prefix>/<digest>.tgz   # immutable verified tarball
//! images/sha512/<prefix>/<digest>/          # extracted once per artifact
//! tmp/                                      # scratch for in-progress writes
//! locks/                                    # per-artifact advisory locks
//! ```
//! `<prefix>` is the first two hex chars of the digest, for fan-out.
//!
//! Invariants (IMPLEMENTATION §7, §21):
//! - published objects are immutable; writes occur in `tmp/` then atomic-rename
//! - integrity is verified *before* the artifact is published
//! - concurrent writers race safely: a per-digest advisory lock serializes the
//!   ensure step; the atomic rename is a second correctness line so a crashed
//!   writer never leaves a partial published artifact
//! - locks are per-artifact (`locks/<key>.lock`); there is no global install lock

use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha512};
use thiserror::Error;

use crate::archive::{self, ExtractError};
use crate::download::{self, DownloadError};
use crate::integrity::{ArtifactId, Integrity, IntegrityError, Sha512Digest};
use crate::metrics::Metrics;

// `std::fs::File` provides inherent `lock()` (exclusive advisory lock) and
// `unlock()` on stable Rust, used here for per-artifact mutual exclusion.

const ARTIFACTS: &str = "artifacts/sha512";
const IMAGES: &str = "images/sha512";
const GRAPHS: &str = "graphs/blake3";
const TMP: &str = "tmp";
const LOCKS: &str = "locks";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(
        "integrity verification failed for {url}\n  expected: {expected}\n  computed: {computed}"
    )]
    IntegrityMismatch {
        url: String,
        expected: String,
        computed: String,
    },
    #[error("corruption detected for artifact {id} (expected {expected}, computed {actual})")]
    Corruption {
        id: String,
        expected: String,
        actual: String,
    },
    #[error("download failed for {url}")]
    Download {
        url: String,
        #[source]
        source: DownloadError,
    },
    #[error("extraction failed for artifact {id}")]
    Extract {
        id: String,
        #[source]
        source: ExtractError,
    },
    #[error("store io error at {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("artifact {id} not found in store")]
    NotFound { id: String },
    #[error("could not acquire lock at {path}: {source}")]
    Lock {
        path: String,
        source: std::io::Error,
    },
    #[error(transparent)]
    Integrity(#[from] IntegrityError),
}

/// The immutable artifact store rooted at a directory.
pub struct ArtifactStore {
    root: PathBuf,
}

/// A reference to a stored archive.
#[derive(Debug)]
pub struct ArtifactRef {
    pub id: ArtifactId,
    pub path: PathBuf,
    /// `true` when the artifact already existed (no download performed).
    pub cached: bool,
}

/// A reference to an extracted image.
#[derive(Debug)]
pub struct ImageRef {
    pub id: ArtifactId,
    pub path: PathBuf,
    /// `true` when the image already existed (no extraction performed).
    pub cached: bool,
}

impl ArtifactStore {
    /// Open (creating) the store at `root`.
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        fs::create_dir_all(root).map_err(|source| io_err(root, source))?;
        for sub in [ARTIFACTS, IMAGES, GRAPHS, TMP, LOCKS] {
            fs::create_dir_all(root.join(sub)).map_err(|source| io_err(root, source))?;
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Absolute path of the archive for `id`.
    pub fn artifact_path(&self, id: &ArtifactId) -> PathBuf {
        let hex = id.to_hex();
        self.root
            .join(ARTIFACTS)
            .join(&hex[..2])
            .join(format!("{hex}.tgz"))
    }

    /// Absolute path of the extracted image for `id`.
    pub fn image_path(&self, id: &ArtifactId) -> PathBuf {
        let hex = id.to_hex();
        self.root.join(IMAGES).join(&hex[..2]).join(&hex)
    }

    /// Absolute path of the reusable graph volume for `graph_hex` (a 64-char
    /// lowercase blake3 hex). The volume holds an immutable `node_modules`
    /// projection keyed by graph id (IMPLEMENTATION §13), shared across
    /// projects that have the same graph.
    pub fn graph_volume_path(&self, graph_hex: &str) -> PathBuf {
        let prefix = graph_hex.get(..2).unwrap_or("");
        self.root.join(GRAPHS).join(prefix).join(graph_hex)
    }

    fn lock_path(&self, key: &str) -> PathBuf {
        self.root.join(LOCKS).join(format!("{key}.lock"))
    }

    /// Block until an exclusive per-`key` lock is acquired. Released on drop.
    fn acquire_lock(&self, key: &str) -> Result<FileGuard, StoreError> {
        let path = self.lock_path(key);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|source| StoreError::Lock {
                path: path.display().to_string(),
                source,
            })?;
        file.lock().map_err(|source| StoreError::Lock {
            path: path.display().to_string(),
            source,
        })?;
        Ok(FileGuard(file))
    }

    /// A unique scratch path under `tmp/`.
    fn unique_tmp(&self, hint: &str) -> Result<PathBuf, StoreError> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let name = format!("{hint}.{}.{}.{}.tmp", std::process::id(), n, nanos);
        Ok(self.root.join(TMP).join(name))
    }

    /// Ensure the tarball for `url` is present and verified, downloading if
    /// necessary. Repeated calls with the same integrity perform no network work.
    ///
    /// When `integrity` is `Some`, the target path is known up front and a
    /// per-digest lock protects the whole step (cache hit serves immediately).
    /// When `integrity` is `None`, the digest is only known after download; the
    /// artifact is still published atomically, but a redundant download may
    /// occur on concurrent misses (acceptable: we cannot share what we cannot name).
    pub fn ensure_artifact(
        &self,
        url: &str,
        integrity: Option<&Integrity>,
        metrics: &mut Metrics,
    ) -> Result<ArtifactRef, StoreError> {
        match integrity {
            Some(integ) => {
                let id = *integ.digest();
                let _guard = self.acquire_lock(&id.to_hex())?;
                let dest = self.artifact_path(&id);
                if dest.exists() {
                    metrics.record("artifact_download", std::time::Duration::ZERO);
                    metrics.record("integrity_verify", std::time::Duration::ZERO);
                    return Ok(ArtifactRef {
                        id,
                        path: dest,
                        cached: true,
                    });
                }
                let tmp = self.unique_tmp(&format!("dl-{}", id.to_hex()))?;
                let computed = metrics
                    .measure("artifact_download", || download::download(url, &tmp))
                    .map_err(|source| StoreError::Download {
                        url: url.to_string(),
                        source,
                    })?;
                let ok = metrics.measure("integrity_verify", || computed == id);
                if !ok {
                    let _ = fs::remove_file(&tmp);
                    return Err(StoreError::IntegrityMismatch {
                        url: url.to_string(),
                        expected: integ.to_npm_string(),
                        computed: computed.to_npm_string(),
                    });
                }
                let created = self.publish_file(&tmp, &dest)?;
                Ok(ArtifactRef {
                    id,
                    path: dest,
                    cached: !created,
                })
            }
            None => {
                let tmp = self.unique_tmp(&format!("dl-anon-{}", sanitize_hint(url)))?;
                let computed = metrics
                    .measure("artifact_download", || download::download(url, &tmp))
                    .map_err(|source| StoreError::Download {
                        url: url.to_string(),
                        source,
                    })?;
                let dest = self.artifact_path(&computed);
                if dest.exists() {
                    let _ = fs::remove_file(&tmp);
                    metrics.record("integrity_verify", std::time::Duration::ZERO);
                    return Ok(ArtifactRef {
                        id: computed,
                        path: dest,
                        cached: true,
                    });
                }
                self.publish_file(&tmp, &dest)?;
                Ok(ArtifactRef {
                    id: computed,
                    path: dest,
                    cached: false,
                })
            }
        }
    }

    /// Ensure the extracted image for `id` exists, extracting once if needed.
    /// Repeated calls perform no extraction work. Requires the archive to exist.
    pub fn ensure_image(
        &self,
        id: &ArtifactId,
        metrics: &mut Metrics,
    ) -> Result<ImageRef, StoreError> {
        let _guard = self.acquire_lock(&format!("img-{}", id.to_hex()))?;
        let img = self.image_path(id);
        if img.exists() {
            metrics.record("artifact_extract", std::time::Duration::ZERO);
            return Ok(ImageRef {
                id: *id,
                path: img,
                cached: true,
            });
        }
        let archive = self.artifact_path(id);
        if !archive.exists() {
            return Err(StoreError::NotFound { id: id.to_hex() });
        }
        let tmp = self.unique_tmp(&format!("img-{}", id.to_hex()))?;
        fs::create_dir_all(&tmp).map_err(|source| io_err(&tmp, source))?;
        metrics
            .measure("artifact_extract", || archive::extract(&archive, &tmp))
            .map_err(|source| StoreError::Extract {
                id: id.to_hex(),
                source,
            })?;
        if let Some(parent) = img.parent() {
            fs::create_dir_all(parent).map_err(|source| io_err(parent, source))?;
        }
        match fs::rename(&tmp, &img) {
            Ok(()) => Ok(ImageRef {
                id: *id,
                path: img,
                cached: false,
            }),
            Err(e) => {
                if img.exists() {
                    let _ = fs::remove_dir_all(&tmp);
                    Ok(ImageRef {
                        id: *id,
                        path: img,
                        cached: true,
                    })
                } else {
                    let _ = fs::remove_dir_all(&tmp);
                    Err(io_err(&img, e))
                }
            }
        }
    }

    /// Atomic-rename `tmp` to a freshly created `dest`. Returns whether *we*
    /// created the destination; a concurrent winner is treated as cached.
    fn publish_file(&self, tmp: &Path, dest: &Path) -> Result<bool, StoreError> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|source| io_err(parent, source))?;
        }
        match fs::rename(tmp, dest) {
            Ok(()) => Ok(true),
            Err(e) => {
                if dest.exists() {
                    let _ = fs::remove_file(tmp);
                    Ok(false)
                } else {
                    let _ = fs::remove_file(tmp);
                    Err(io_err(dest, e))
                }
            }
        }
    }

    /// Re-hash the stored archive and confirm it matches `id`; detects tampering
    /// or corruption of existing objects (AGENTS "corrupt existing objects").
    pub fn verify_artifact(&self, id: &ArtifactId) -> Result<(), StoreError> {
        let path = self.artifact_path(id);
        let mut file = fs::File::open(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => StoreError::NotFound { id: id.to_hex() },
            _ => io_err(&path, e),
        })?;
        let mut hasher = Sha512::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|source| io_err(&path, source))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let mut actual = [0u8; 64];
        actual.copy_from_slice(&hasher.finalize());
        if &actual != id.as_bytes() {
            return Err(StoreError::Corruption {
                id: id.to_hex(),
                expected: Sha512Digest::from_bytes(*id.as_bytes()).to_npm_string(),
                actual: Sha512Digest::from_bytes(actual).to_npm_string(),
            });
        }
        Ok(())
    }
}

/// RAII exclusive lock; the OS releases it when the file handle closes.
struct FileGuard(std::fs::File);

impl Drop for FileGuard {
    fn drop(&mut self) {
        // Best-effort explicit unlock; dropping the File also releases it.
        let _ = self.0.unlock();
    }
}

fn io_err(path: &Path, source: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Reduce `url` to a filesystem-hint-safe slug (lock/tmp names only).
fn sanitize_hint(url: &str) -> String {
    url.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(*c, '-' | '_' | '.' | '/'))
        .take(96)
        .collect::<String>()
        .replace('/', "-")
}
