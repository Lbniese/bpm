//! Artifact source retrieval by exact URL.
//!
//! Supports two schemes:
//! - `http://` / `https://` (default): streams the response body to a
//!   destination file while hashing it with SHA-512, so a tarball is only read
//!   once from the network and never re-read from disk for verification.
//! - `file://<path>` or a bare local path: streams+hashes a local file through
//!   the *same* SHA-512 path, enabling fully offline, deterministic fixture
//!   tests with no server process and no network dependency.
//!
//! In both cases the bytes are read exactly once into a destination temp file
//! and the computed digest is returned; the store layer compares it to the
//! declared integrity (see [`crate::store`]).
//!
//! A blocking client is intentional: Milestone 1's concurrency is
//! inter-process publication safety, not request parallelism. Bounded
//! concurrency is a later milestone.

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sha2::{Digest, Sha512};
use thiserror::Error;

use crate::config::NpmConfig;
use crate::http::{HttpClient, HttpError};
use crate::integrity::Sha512Digest;

const BUF_BYTES: usize = 64 * 1024;
const FILE_SCHEME: &str = "file://";

/// Process-wide default client used by compatibility callers that do not
/// supply effective npm configuration explicitly.
static DEFAULT_HTTP_CLIENT: OnceLock<HttpClient> = OnceLock::new();

/// Which kind of source `url` denotes, and (for files) the resolved path.
enum Source<'a> {
    Http(&'a str),
    File(PathBuf),
}

/// Classify `url` into an HTTP URL or a local file path.
///
/// Recognized local forms:
/// - `file://<path>` — the remainder after `file://` is taken verbatim as a
///   path. `file:///abs/x.tgz` becomes `/abs/x.tgz`; `file://rel/x` becomes
///   `rel/x`.
/// - a bare path with no `://` separator (e.g. `/abs/x.tgz`, `./rel/x`).
fn classify(url: &str) -> Source<'_> {
    if let Some(rest) = url.strip_prefix(FILE_SCHEME) {
        return Source::File(PathBuf::from(rest));
    }
    if !has_scheme(url) {
        return Source::File(PathBuf::from(url));
    }
    Source::Http(url)
}

/// `true` if `url` begins with a `<scheme>:` component (`http:`, `https:`...).
fn has_scheme(url: &str) -> bool {
    match url.split_once("://") {
        None => false,
        Some((scheme, _)) => {
            !scheme.is_empty()
                && scheme
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        }
    }
}

/// Copy bytes from `reader` to a fresh file at `dest`, hashing with SHA-512.
///
/// `dest` must not exist (the caller manages a unique temp path). The file is
/// created fresh, written fully, and flushed before returning; the caller
/// performs the final atomic rename. The destination path is reported in any
/// IO error so the failure is locatable.
fn stream_to_dest_and_hash<R: Read>(
    reader: &mut R,
    dest: &Path,
) -> Result<Sha512Digest, DownloadError> {
    let dest_str = dest.display().to_string();
    let file = File::create(dest).map_err(|source| DownloadError::Io {
        path: dest_str.clone(),
        source,
    })?;
    let mut writer = BufWriter::new(file);

    let mut hasher = Sha512::new();
    let mut buf = vec![0u8; BUF_BYTES];
    loop {
        let n = reader.read(&mut buf).map_err(|source| DownloadError::Io {
            path: dest_str.clone(),
            source,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        writer
            .write_all(&buf[..n])
            .map_err(|source| DownloadError::Io {
                path: dest_str.clone(),
                source,
            })?;
    }
    writer.flush().map_err(|source| DownloadError::Io {
        path: dest_str,
        source,
    })?;

    let mut digest = [0u8; 64];
    digest.copy_from_slice(&hasher.finalize());
    Ok(Sha512Digest::from_bytes(digest))
}

/// Retrieve the artifact at `url` to `dest`, returning the SHA-512 of the
/// received bytes.
///
/// Dispatches on scheme (see [`classify`]); HTTP/HTTPS behavior is identical
/// to the original download path, and `file://` (or a bare path) streams the
/// local file through the same hashing pipeline.
///
/// `dest` must not exist (the caller manages a unique temp path).
pub fn download(url: &str, dest: &Path) -> Result<Sha512Digest, DownloadError> {
    let client = DEFAULT_HTTP_CLIENT.get_or_init(|| HttpClient::new(NpmConfig::default()));
    download_with_client(client, url, dest)
}

/// Retrieve an artifact using a caller-owned pooled HTTP client.
///
/// HTTP sources inherit the client's configured authentication, redirects,
/// timeouts, and bounded retries. Local paths and `file://` sources never use
/// the client and preserve the same open, stream, hash, and error behavior as
/// [`download`].
pub fn download_with_client(
    client: &HttpClient,
    url: &str,
    dest: &Path,
) -> Result<Sha512Digest, DownloadError> {
    match classify(url) {
        Source::Http(u) => {
            let mut reader = client.stream(u).map_err(map_http_error)?;
            stream_to_dest_and_hash(&mut reader, dest)
        }
        Source::File(path) => {
            let mut reader = File::open(&path).map_err(|source| DownloadError::Io {
                path: path.display().to_string(),
                source,
            })?;
            stream_to_dest_and_hash(&mut reader, dest)
        }
    }
}

fn map_http_error(error: HttpError) -> DownloadError {
    match error {
        HttpError::Status {
            url,
            code,
            attempts,
        } => DownloadError::HttpStatus {
            url,
            code,
            attempts,
        },
        HttpError::Transport {
            url,
            kind,
            attempts,
        } => DownloadError::Transport {
            kind,
            url,
            attempts,
        },
    }
}

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("HTTP GET {url} returned status {code} after {attempts} attempt(s)")]
    HttpStatus {
        url: String,
        code: u16,
        attempts: usize,
    },
    #[error("HTTP GET {url} failed with transport error {kind} after {attempts} attempt(s)")]
    Transport {
        kind: String,
        url: String,
        attempts: usize,
    },
    #[error("io error at {path}: {source}")]
    Io { path: String, source: io::Error },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrity::Sha512Digest;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_known_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    #[test]
    fn classifies_http_urls() {
        assert!(matches!(
            classify("https://example.com/x.tgz"),
            Source::Http(_)
        ));
        assert!(matches!(
            classify("http://example.com/x.tgz"),
            Source::Http(_)
        ));
    }

    #[test]
    fn classifies_file_urls_and_bare_paths() {
        // file:// followed by an absolute path (three-slash form).
        let s = classify("file:///tmp/x.tgz");
        assert!(matches!(s, Source::File(_)));
        if let Source::File(p) = s {
            assert_eq!(p, PathBuf::from("/tmp/x.tgz"));
        }
        // Bare absolute path (no scheme).
        let s = classify("/tmp/x.tgz");
        assert!(matches!(s, Source::File(_)));
        // Bare relative path.
        assert!(matches!(classify("./x.tgz"), Source::File(_)));
        assert!(matches!(classify("x.tgz"), Source::File(_)));
    }

    #[test]
    fn downloads_file_url_with_correct_digest() {
        let dir = tempdir().unwrap();
        let payload = b"hello file world, with \0 bytes and markers \xff";
        let src = write_known_file(dir.path(), "pkg.tgz", payload);
        let url = format!("file://{}", src.display());
        let dest = dir.path().join("dest.bin");

        let digest = download(&url, &dest).expect("file download succeeds");
        assert_eq!(digest, Sha512Digest::hash_bytes(payload));
        // Destination file contains the exact bytes (streamed copy).
        let written = std::fs::read(&dest).unwrap();
        assert_eq!(written, payload);
    }

    #[test]
    fn downloads_bare_absolute_path_identically() {
        let dir = tempdir().unwrap();
        let payload = b"bare path payload";
        let src = write_known_file(dir.path(), "pkg.tgz", payload);
        let dest = dir.path().join("out.bin");

        // Same source, accessed by bare path instead of file:// URL.
        let digest = download(src.to_str().unwrap(), &dest).expect("bare path succeeds");
        assert_eq!(digest, Sha512Digest::hash_bytes(payload));
        assert_eq!(std::fs::read(&dest).unwrap(), payload);
    }

    #[test]
    fn large_file_streams_in_chunks() {
        // Larger than BUF_BYTES (64 KiB) to exercise the read loop.
        let dir = tempdir().unwrap();
        let payload: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
        let src = write_known_file(dir.path(), "big.tgz", &payload);
        let dest = dir.path().join("big.out");

        let digest =
            download(&format!("file://{}", src.display()), &dest).expect("large file streams");
        assert_eq!(digest, Sha512Digest::hash_bytes(&payload));
        assert_eq!(std::fs::read(&dest).unwrap(), payload);
    }

    #[test]
    fn missing_file_source_errors_clearly() {
        let dir = tempdir().unwrap();
        let url = format!("file://{}/nope.tgz", dir.path().display());
        let dest = dir.path().join("dest.bin");
        let err = download(&url, &dest).expect_err("missing source should fail");
        // The source path is carried in the error for an actionable message.
        let msg = format!("{err}");
        assert!(msg.contains("nope.tgz"), "error lacks source path: {msg}");
        assert!(!dest.exists(), "dest must not be created on source failure");
    }
}
