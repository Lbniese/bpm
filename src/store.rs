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
use crate::http::HttpClient;
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

    /// Path to the deterministic seekable package-image sidecar.
    pub fn image_index_path(&self, id: &ArtifactId) -> PathBuf {
        self.image_path(id).with_extension("bpi")
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

    /// Compatibility-only artifact entry point using the process-default HTTP
    /// client.
    ///
    /// New production callers must use [`Self::ensure_artifact_with_client`] so
    /// registry metadata and tarball requests share one effective npm
    /// configuration and pooled client. Local files retain their existing
    /// behavior because the download layer does not consult the HTTP client for
    /// local paths.
    pub fn ensure_artifact(
        &self,
        url: &str,
        integrity: Option<&Integrity>,
        metrics: &mut Metrics,
    ) -> Result<ArtifactRef, StoreError> {
        self.ensure_artifact_using(url, integrity, metrics, download::download)
    }

    /// Ensure the tarball for `url` is present and verified using a
    /// caller-owned pooled HTTP client.
    ///
    /// HTTP downloads inherit the client's authentication, retry, redirect,
    /// and timeout policy. Local file sources bypass HTTP while preserving the
    /// same integrity, locking, metrics, cleanup, and atomic publication path.
    pub fn ensure_artifact_with_client(
        &self,
        client: &HttpClient,
        url: &str,
        integrity: Option<&Integrity>,
        metrics: &mut Metrics,
    ) -> Result<ArtifactRef, StoreError> {
        self.ensure_artifact_using(url, integrity, metrics, |url, dest| {
            download::download_with_client(client, url, dest)
        })
    }

    /// Shared artifact pipeline for compatibility and configured-client entry
    /// points. Keeping all store semantics here prevents either HTTP boundary
    /// from diverging in integrity, locking, metrics, cleanup, or publication.
    fn ensure_artifact_using<F>(
        &self,
        url: &str,
        integrity: Option<&Integrity>,
        metrics: &mut Metrics,
        mut retrieve: F,
    ) -> Result<ArtifactRef, StoreError>
    where
        F: FnMut(&str, &Path) -> Result<Sha512Digest, DownloadError>,
    {
        // Repeated calls with the same integrity perform no network work. When
        // integrity is known, a per-digest lock protects the whole step. When
        // it is absent, publication remains atomic, though concurrent misses
        // may redundantly download an artifact that cannot yet be named.
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
                    .measure("artifact_download", || retrieve(url, &tmp))
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
                    .measure("artifact_download", || retrieve(url, &tmp))
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
            let index = self.image_index_path(id);
            if !index.exists() {
                let bytes = crate::package_image::from_directory(&img)
                    .map_err(|error| io_err(&index, std::io::Error::other(error.to_string())))?;
                fs::write(&index, bytes).map_err(|source| io_err(&index, source))?;
            }
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
            Ok(()) => {
                let index = self.image_index_path(id);
                let bytes = crate::package_image::from_directory(&img)
                    .map_err(|error| io_err(&index, std::io::Error::other(error.to_string())))?;
                fs::write(&index, bytes).map_err(|source| io_err(&index, source))?;
                Ok(ImageRef {
                    id: *id,
                    path: img,
                    cached: false,
                })
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NpmConfig;
    use crate::http::HttpClient;
    use std::io::Write as _;
    use std::net::TcpListener;

    fn authenticated_server(body: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept artifact request");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).expect("read artifact request");
                assert_ne!(read, 0, "request ended before headers");
                request.extend_from_slice(&chunk[..read]);
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("write response headers");
            stream.write_all(&body).expect("write response body");
            String::from_utf8(request).expect("request headers are utf8")
        });
        (format!("http://{address}/artifact.tgz"), handle)
    }

    fn configured_client(url: &str, directory: &Path) -> HttpClient {
        let authority = url
            .strip_prefix("http://")
            .and_then(|value| value.split_once('/'))
            .map(|(authority, _)| authority)
            .expect("test URL authority");
        let npmrc = directory.join("configured.npmrc");
        fs::write(
            &npmrc,
            format!("//{authority}/:_authToken=artifact-secret\nfetch-retries=0\n"),
        )
        .expect("write npmrc");
        HttpClient::new(NpmConfig::load_paths(None, Some(&npmrc)).expect("load npmrc"))
    }

    #[test]
    fn configured_client_is_used_for_known_integrity_http_artifacts() {
        let body = b"known integrity artifact".to_vec();
        let expected = Integrity::sha512(Sha512Digest::hash_bytes(&body));
        let server_dir = tempfile::tempdir().expect("server config dir");
        let (url, request) = authenticated_server(body.clone());
        let client = configured_client(&url, server_dir.path());
        let store_dir = tempfile::tempdir().expect("store dir");
        let store = ArtifactStore::open(store_dir.path()).expect("open store");
        let mut metrics = Metrics::new();

        let artifact = store
            .ensure_artifact_with_client(&client, &url, Some(&expected), &mut metrics)
            .expect("store configured artifact");

        assert!(!artifact.cached);
        assert_eq!(fs::read(&artifact.path).expect("read artifact"), body);
        assert!(metrics.to_json().contains("artifact_download"));
        assert!(metrics.to_json().contains("integrity_verify"));
        assert!(request
            .join()
            .expect("join server")
            .contains("Authorization: Bearer artifact-secret\r\n"));
        assert_eq!(
            fs::read_dir(store.root().join(TMP))
                .expect("read temp directory")
                .count(),
            0
        );
    }

    #[test]
    fn configured_client_preserves_anonymous_and_local_artifact_paths() {
        let body = b"anonymous artifact".to_vec();
        let server_dir = tempfile::tempdir().expect("server config dir");
        let (url, request) = authenticated_server(body.clone());
        let client = configured_client(&url, server_dir.path());
        let store_dir = tempfile::tempdir().expect("store dir");
        let store = ArtifactStore::open(store_dir.path()).expect("open store");
        let mut metrics = Metrics::new();

        let anonymous = store
            .ensure_artifact_with_client(&client, &url, None, &mut metrics)
            .expect("store anonymous configured artifact");
        assert_eq!(anonymous.id, Sha512Digest::hash_bytes(&body));
        assert!(!anonymous.cached);
        assert!(request
            .join()
            .expect("join server")
            .contains("Authorization: Bearer artifact-secret\r\n"));

        let local_body = b"local artifact";
        let local_source = store_dir.path().join("local.tgz");
        fs::write(&local_source, local_body).expect("write local artifact");
        let local_integrity = Integrity::sha512(Sha512Digest::hash_bytes(local_body));
        let local = store
            .ensure_artifact_with_client(
                &client,
                local_source.to_str().expect("utf8 temp path"),
                Some(&local_integrity),
                &mut metrics,
            )
            .expect("store local artifact");
        assert_eq!(
            fs::read(local.path).expect("read local artifact"),
            local_body
        );
    }
}
