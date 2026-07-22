//! Non-registry dependency source resolution (Git, file, tarball, patch).

use std::fs::{self, OpenOptions};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use sha2::Digest;

use super::workspace_metadata;
use crate::download::MAX_ARTIFACT_BYTES;
use crate::http::redact_url;
use crate::lockfile::LockSource;
use crate::manifest::PackageManifest;
use crate::registry::VersionMetadata;

pub(crate) enum DependencySource {
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
pub(crate) struct SourceResolution {
    pub(crate) metadata: VersionMetadata,
    pub(crate) resolved: String,
    pub(crate) integrity: Option<String>,
    pub(crate) source: LockSource,
    pub(crate) link: bool,
    pub(crate) workspace_target: Option<String>,
    pub(crate) source_dir: Option<PathBuf>,
}

impl DependencySource {
    pub(crate) fn parse(spec: &str) -> Option<Self> {
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

    pub(crate) fn resolve(self, base_dir: &Path) -> Result<SourceResolution, String> {
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
    let bytes = read_file_bounded(path)?;
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

pub(crate) fn read_source_bytes(
    http: &crate::http::HttpClient,
    url: &str,
) -> Result<Vec<u8>, String> {
    if let Some(path) = url.strip_prefix("file://") {
        return read_file_bounded(Path::new(path))
            .map_err(|error| format!("cannot read {path}: {error}"));
    }
    if !url.contains("://") {
        return read_file_bounded(Path::new(url))
            .map_err(|error| format!("cannot read {url}: {error}"));
    }
    let mut response = http.stream(url).map_err(|error| error.to_string())?;
    read_bounded_to_vec(&mut response, MAX_ARTIFACT_BYTES)
        .map_err(|error| format!("cannot read tarball response: {error}"))
}

/// Read a file into memory, bounded by the compressed-artifact policy. A
/// metadata length check is a fast rejection for regular files, but the read
/// itself is still bounded because the file can change shape or be a
/// non-regular stream behind the same path.
pub(crate) fn read_file_bounded(path: &Path) -> Result<Vec<u8>, String> {
    read_file_bounded_with_limit(path, MAX_ARTIFACT_BYTES)
}

/// Read a file into memory, bounded by `limit`. A metadata length check is a
/// fast rejection for regular files, but the read itself is still bounded
/// because the file can change shape or be a non-regular stream behind the
/// same path. Production callers use [`read_file_bounded`] (the
/// [`MAX_ARTIFACT_BYTES`] policy); a smaller `limit` exists for tests.
pub(crate) fn read_file_bounded_with_limit(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() > limit {
            return Err(format!(
                "artifact {} exceeds the {}-byte compressed source limit",
                path.display(),
                limit
            ));
        }
    }
    let mut file = fs::File::open(path).map_err(|error| error.to_string())?;
    read_bounded_to_vec(&mut file, limit).map_err(|error| error.to_string())
}

/// Read `reader` into a `Vec`, failing once `limit` bytes are exceeded. Bytes
/// are counted with checked arithmetic so an overflow is also an error.
pub(crate) fn read_bounded_to_vec<R: Read>(reader: &mut R, limit: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut total: u64 = 0;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(|error| error.to_string())?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n as u64)
            .ok_or_else(|| format!("artifact exceeded the {limit}-byte compressed source limit"))?;
        if total > limit {
            return Err(format!(
                "artifact exceeded the {limit}-byte compressed source limit"
            ));
        }
        bytes.extend_from_slice(&buf[..n]);
    }
    Ok(bytes)
}

// === Plan 013: race-safe deterministic source caches ===
//
// Git archives, extracted Git source trees, local clone caches, and patched
// tarballs are keyed deterministically but were previously built through
// deterministic `.tmp` staging paths with check-then-act publication. Two BPM
// processes resolving the same source could truncate, remove, rename, or run
// Git commands against each other's staging state. The helpers below give each
// cache key a cross-process advisory lock, a process-unique scratch path with
// cleanup-on-drop, under-lock revalidation, and atomic publication. Raw
// credential-bearing URLs are never used in lock or scratch filenames — only
// content/URL+commit hashes.

/// Hash `url` and `commit` into the deterministic hex key shared by the
/// source-tree and clone caches.
fn source_key(url: &str, commit: &str) -> String {
    let mut hasher = sha2::Sha512::new();
    hasher.update(url.as_bytes());
    hasher.update([0]);
    hasher.update(commit.as_bytes());
    sha512_hex(&hasher.finalize())
}

fn sha512_hex(digest: &[u8]) -> String {
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

/// RAII advisory lock over a per-key file under the temp directory. The lock is
/// held exclusively and released on drop (best-effort explicit `unlock`; closing
/// the file is the definitive OS release). A crashed process may leave a lock
/// file behind but never a held advisory lock, so a later process can proceed.
struct SourceLockGuard(std::fs::File);
impl Drop for SourceLockGuard {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Acquire an exclusive cross-process advisory lock scoped to `namespace` +
/// `key`. Distinct namespaces (`patch`, `source-tree`, `archive`, `clone`) let
/// nested operations (e.g. archive → clone fallback) acquire a clone lock
/// without recursively touching the archive lock. `key` must already be a
/// credential-free hash.
fn source_lock(namespace: &str, key: &str) -> Result<SourceLockGuard, String> {
    let dir = std::env::temp_dir()
        .join("bpm-source-locks")
        .join(namespace);
    fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create source lock directory: {error}"))?;
    let path = dir.join(format!("{key}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .map_err(|error| format!("cannot open source lock {}: {error}", path.display()))?;
    file.lock()
        .map_err(|error| format!("cannot acquire source lock {}: {error}", path.display()))?;
    Ok(SourceLockGuard(file))
}

/// A process-unique scratch path under `parent`. `hint` must be a credential-
/// free hash used only to make the name recognizable. Uniqueness comes from a
/// monotonic counter, the current nanosecond count, and the process id.
fn unique_scratch(parent: &Path, hint: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    parent.join(format!("{hint}.{n}.{nanos}.{}.tmp", std::process::id()))
}

/// Kind of scratch artifact tracked by [`ScratchGuard`].
enum ScratchKind {
    File,
    Dir,
}

/// RAII guard that removes a unique scratch file or directory on drop unless
/// [`ScratchGuard::disarm`] is called after successful publication. This keeps
/// future early-return branches from leaking partial staging state.
struct ScratchGuard {
    path: PathBuf,
    kind: ScratchKind,
    armed: bool,
}

impl ScratchGuard {
    fn file(path: PathBuf) -> Self {
        Self {
            path,
            kind: ScratchKind::File,
            armed: true,
        }
    }
    fn dir(path: PathBuf) -> Self {
        Self {
            path,
            kind: ScratchKind::Dir,
            armed: true,
        }
    }
    /// Confirm publication succeeded; the guard will no longer remove the path.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ScratchGuard {
    fn drop(&mut self) {
        if self.armed {
            match self.kind {
                ScratchKind::File => {
                    let _ = fs::remove_file(&self.path);
                }
                ScratchKind::Dir => {
                    let _ = fs::remove_dir_all(&self.path);
                }
            }
        }
    }
}

/// Validate an existing normalized Git archive cache file before reuse: it must
/// be a bounded, decodable gzip+tar containing a package manifest. `is_file()`
/// alone is not enough — a truncated/partial file from a crashed writer fails
/// here so the caller rebuilds it under the lock.
fn validate_git_archive_cache(path: &Path) -> bool {
    read_file_bounded(path)
        .ok()
        .and_then(|bytes| manifest_from_tarball(&bytes).ok())
        .is_some()
}

pub(crate) fn write_patched_tarball(base_dir: &Path, bytes: &[u8]) -> Result<String, String> {
    let hex = {
        let mut hasher = sha2::Sha512::new();
        hasher.update(bytes);
        sha512_hex(&hasher.finalize())
    };
    let root = if base_dir.is_dir() {
        base_dir.to_path_buf()
    } else {
        std::env::temp_dir()
    };
    let dir = root.join(".bpm").join("patches");
    fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create patch cache {}: {error}", dir.display()))?;
    let path = dir.join(format!("{hex}.tgz"));
    // Serialize publication per content key across processes.
    let _guard = source_lock("patch", &hex)?;
    // Recheck under the lock and validate an existing final file by content: a
    // corrupt/partial file (e.g. from a crashed writer) must be rebuilt.
    if path.is_file() {
        if let Ok(existing) = read_file_bounded(&path) {
            let existing_hex = {
                let mut hasher = sha2::Sha512::new();
                hasher.update(&existing);
                sha512_hex(&hasher.finalize())
            };
            if existing_hex == hex {
                return Ok(format!("file://{}", path.display()));
            }
        }
        let _ = fs::remove_file(&path);
    }
    let tmp = unique_scratch(&dir, &hex);
    let mut scratch = ScratchGuard::file(tmp.clone());
    fs::write(&tmp, bytes)
        .map_err(|error| format!("cannot write patched tarball {}: {error}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|error| {
        format!(
            "cannot publish patched tarball {} -> {}: {error}",
            tmp.display(),
            path.display()
        )
    })?;
    scratch.disarm();
    Ok(format!("file://{}", path.display()))
}

pub(crate) fn resolve_git_source(
    url: &str,
    reference: Option<&str>,
) -> Result<SourceResolution, String> {
    reject_git_option_value("url", url)?;
    if let Some(reference) = reference {
        reject_git_option_value("reference", reference)?;
    }
    let resolved_commit = resolve_git_commit(url, reference)?;
    if let Some(tarball_url) = hosted_git_tarball_url(url, Some(&resolved_commit)) {
        let http = crate::http::HttpClient::new(crate::config::NpmConfig::default());
        let bytes = read_source_bytes(&http, &tarball_url)?;
        let mut resolution = source_from_tarball_bytes(
            &tarball_url,
            bytes.clone(),
            LockSource::Git {
                url: url.to_owned(),
                reference: reference.map(str::to_owned),
                resolved_commit: resolved_commit.clone(),
            },
        )?;
        resolution.source_dir = Some(cache_git_source_tree(url, &resolved_commit, &bytes)?);
        return Ok(resolution);
    }
    // Raw git transports may not accept a SHA as the archive ref. Fetch using
    // the user's ref (or HEAD), but key the local archive by the resolved SHA
    // so branch/tag aliases for the same commit share bytes.
    let fetch_reference = reference.unwrap_or("HEAD");
    let tarball = git_archive_tarball(url, fetch_reference, &resolved_commit)?;
    let cache_url = format!("file://{}", tarball.display());
    let bytes = read_file_bounded(&tarball)
        .map_err(|error| format!("cannot read git archive {}: {error}", tarball.display()))?;
    let mut resolution = source_from_tarball_bytes(
        &cache_url,
        bytes.clone(),
        LockSource::Git {
            url: url.to_owned(),
            reference: reference.map(str::to_owned),
            resolved_commit: resolved_commit.clone(),
        },
    )?;
    resolution.source_dir = Some(cache_git_source_tree(url, &resolved_commit, &bytes)?);
    Ok(resolution)
}

/// Fetch a commit into a local repository when `git archive --remote` rejects
/// a raw SHA (the common behavior for `file://`, SSH, and git-daemon remotes).
fn archive_git_commit_locally(url: &str, commit: &str) -> Result<std::process::Output, String> {
    let source = url.strip_prefix("file://").unwrap_or(url);
    reject_git_option_value("url", source)?;
    // A local source directory passed to `git -C <source> archive` is read-only
    // from BPM's perspective, so it needs no cross-process cache lock.
    if Path::new(source).is_dir() {
        return Command::new("git")
            .args(["-C", source, "archive", "--format=tar", "--", commit])
            .output()
            .map_err(|error| format!("cannot archive local Git commit: {error}"));
    }
    // Non-directory remote (file:// bundle, git://, ssh://, ...): maintain a
    // shared clone cache keyed by url+commit. All mutation of that clone
    // directory is serialized through a `clone`-namespace lock held across
    // clone validation, fetch, and archive. The distinct namespace lets the
    // archive caller hold its own `archive` lock without re-entering this one.
    let key = source_key(url, commit);
    let _guard = source_lock("clone", &key)?;
    let clones_root = std::env::temp_dir().join("bpm-git-clones-v1");
    fs::create_dir_all(&clones_root)
        .map_err(|error| format!("cannot create Git clone cache: {error}"))?;
    let clone_dir = clones_root.join(&key);
    if !clone_dir.join(".git").is_dir() {
        // If a final clone exists but is not a usable repository, quarantine
        // and rebuild it only while holding the lock.
        if clone_dir.is_dir() {
            let _ = fs::remove_dir_all(&clone_dir);
        } else if clone_dir.exists() {
            let _ = fs::remove_file(&clone_dir);
        }
        // Clone into a unique sibling staging directory, validate `.git`, then
        // rename to the shared final path. Never run `git clone` directly into
        // the shared final path.
        let staging = unique_scratch(&clones_root, &key);
        let mut scratch = ScratchGuard::dir(staging.clone());
        let clone = Command::new("git")
            .args([
                "clone",
                "--no-checkout",
                "--",
                url,
                &staging.display().to_string(),
            ])
            .output()
            .map_err(|error| format!("cannot clone Git source: {error}"))?;
        if !clone.status.success() {
            return Err(String::from_utf8_lossy(&clone.stderr).trim().to_owned());
        }
        if !staging.join(".git").is_dir() {
            return Err(format!(
                "git clone of {} did not produce a repository",
                redact_url(url)
            ));
        }
        fs::rename(&staging, &clone_dir).map_err(|error| {
            format!(
                "cannot publish Git clone cache {} -> {}: {error}",
                staging.display(),
                clone_dir.display()
            )
        })?;
        scratch.disarm();
    }
    let fetch = Command::new("git")
        .args([
            "-C",
            &clone_dir.display().to_string(),
            "fetch",
            "origin",
            "--",
            commit,
        ])
        .output()
        .map_err(|error| format!("cannot fetch Git commit: {error}"))?;
    if !fetch.status.success() {
        return Err(String::from_utf8_lossy(&fetch.stderr).trim().to_owned());
    }
    Command::new("git")
        .args([
            "-C",
            &clone_dir.display().to_string(),
            "archive",
            "--format=tar",
            "--",
            commit,
        ])
        .output()
        .map_err(|error| format!("cannot archive fetched Git commit: {error}"))
}

/// Extract a Git archive once so relative `file:` dependencies are resolved
/// against the Git package itself rather than the consumer project.
fn cache_git_source_tree(url: &str, commit: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let key = source_key(url, commit);
    let root = std::env::temp_dir().join("bpm-git-sources").join(&key);
    // Serialize construction per source key across processes.
    let _guard = source_lock("source-tree", &key)?;
    // Recognize both supported valid layouts; a root lacking either manifest is
    // treated as incomplete/corrupt and rebuilt.
    if root.join("package.json").is_file() {
        return Ok(root);
    }
    if root.join("package/package.json").is_file() {
        return Ok(root.join("package"));
    }
    let parent = root
        .parent()
        .ok_or_else(|| "cannot resolve parent for Git source cache".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create Git source cache: {error}"))?;
    // Clear any incomplete/corrupt final root left by a crashed writer so the
    // atomic rename below can replace it.
    if root.is_dir() {
        let _ = fs::remove_dir_all(&root);
    } else if root.exists() {
        let _ = fs::remove_file(&root);
    }
    // Stage into a unique sibling directory; never the deterministic shared
    // `root.with_extension("tmp")` another process might be using.
    let staging = unique_scratch(parent, &key);
    let mut scratch = ScratchGuard::dir(staging.clone());
    fs::create_dir_all(&staging)
        .map_err(|error| format!("cannot create Git source cache staging: {error}"))?;
    let archive_path = staging.join("source.tgz");
    fs::write(&archive_path, bytes)
        .map_err(|error| format!("cannot stage Git source archive: {error}"))?;
    crate::archive::extract(&archive_path, &staging)
        .map_err(|error| format!("cannot extract Git source archive: {error}"))?;
    let _ = fs::remove_file(&archive_path);
    // Validate the staged package-root layout before publishing.
    let package_root = if staging.join("package.json").is_file() {
        root.clone()
    } else if staging.join("package/package.json").is_file() {
        root.join("package")
    } else {
        return Err("Git source archive contains no package.json".into());
    };
    fs::rename(&staging, &root).map_err(|error| {
        format!(
            "cannot publish Git source cache {}: {error}",
            root.display()
        )
    })?;
    scratch.disarm();
    Ok(package_root)
}

pub(crate) fn is_full_git_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Reject values that git would parse as options. Git parses a leading `-`
/// (single or double) as an option flag, so a Git reference or url beginning
/// with `-` becomes an injected option rather than a positional. The `--`
/// separator added at the call sites makes this defense-in-depth, but
/// rejecting leading-dash values yields a clearer error than a confusing git
/// message and blocks the injection class explicitly.
pub(crate) fn reject_git_option_value(label: &str, value: &str) -> Result<(), String> {
    if value.starts_with('-') {
        return Err(format!(
            "git {label} {value:?} is rejected: values beginning with '-' \
             would be parsed as a git option"
        ));
    }
    Ok(())
}

fn resolve_git_commit(url: &str, reference: Option<&str>) -> Result<String, String> {
    if let Some(reference) = reference.filter(|value| is_full_git_commit(value)) {
        return Ok(reference.to_ascii_lowercase());
    }
    let requested = reference.unwrap_or("HEAD");
    reject_git_option_value("reference", requested)?;
    let remote = git_clone_url(url);
    reject_git_option_value("remote", &remote)?;
    let output = Command::new("git")
        .args(["ls-remote", "--", &remote, requested])
        .output()
        .map_err(|error| {
            format!(
                "cannot execute git ls-remote for {}: {error}",
                redact_url(url)
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "git ls-remote failed for {}#{}: {}",
            redact_url(&remote),
            requested,
            sanitize_git_stderr(&output.stderr, &remote, url)
        ));
    }
    let mut candidate = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        let Some(sha) = fields.next() else { continue };
        let name = fields.next().unwrap_or_default();
        if !is_full_git_commit(sha) {
            continue;
        }
        // Annotated tags produce both the tag object and a peeled commit. The
        // peeled line is the commit npm records for a tag.
        if name.ends_with("^{}") {
            return Ok(sha.to_ascii_lowercase());
        }
        candidate = Some(sha.to_ascii_lowercase());
    }
    candidate.ok_or_else(|| {
        format!(
            "git reference {requested:?} does not resolve in {}",
            redact_url(&remote)
        )
    })
}

fn sanitize_git_stderr(stderr: &[u8], remote: &str, url: &str) -> String {
    let mut msg = String::from_utf8_lossy(stderr).trim().to_string();
    // Replace known raw URL/remote arguments with redacted forms
    // so credential-bearing URLs cannot leak through git stderr.
    if !remote.is_empty() {
        msg = msg.replace(remote, &redact_url(remote));
    }
    if !url.is_empty() && url != remote {
        msg = msg.replace(url, &redact_url(url));
    }
    msg
}

fn git_archive_tarball(
    url: &str,
    reference: &str,
    resolved_commit: &str,
) -> Result<PathBuf, String> {
    let mut key_hasher = sha2::Sha512::new();
    key_hasher.update(url.as_bytes());
    key_hasher.update([0]);
    key_hasher.update(resolved_commit.as_bytes());
    let key = key_hasher
        .finalize()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let cache_dir = std::env::temp_dir().join("bpm-git-archives-v2");
    fs::create_dir_all(&cache_dir).map_err(|error| {
        format!(
            "cannot create git archive cache {}: {error}",
            cache_dir.display()
        )
    })?;
    let dest = cache_dir.join(format!("{key}.tgz"));
    // Serialize publication per archive key across processes. Held across the
    // git command and normalization so a racing waiter reuses the completed
    // immutable result. The `clone` fallback uses a distinct lock namespace.
    let _archive_lock = source_lock("archive", &key)?;
    // Recheck under the lock and validate an existing final file: `is_file()`
    // alone is not enough — a truncated/partial file from a crashed writer must
    // be rebuilt.
    if dest.is_file() {
        if validate_git_archive_cache(&dest) {
            return Ok(dest);
        }
        let _ = fs::remove_file(&dest);
    }
    let display_url = redact_url(url);
    let remote_archive = Command::new("git")
        .args([
            "archive",
            "--format=tar",
            &format!("--remote={url}"),
            "--",
            reference,
        ])
        .output()
        .map_err(|error| format!("cannot execute git archive for {}: {error}", display_url))?;
    let output = if remote_archive.status.success() {
        remote_archive
    } else if is_full_git_commit(reference) {
        let sanitized_stderr = sanitize_git_stderr(&remote_archive.stderr, url, url);
        let fallback = archive_git_commit_locally(url, resolved_commit).map_err(|error| {
            format!(
                "git archive failed for {}#{}: {}; local commit fallback failed: {error}",
                display_url, reference, sanitized_stderr,
            )
        })?;
        if !fallback.status.success() {
            return Err(format!(
                "git archive failed for {}#{}: {}; local commit fallback failed: {}",
                display_url,
                reference,
                sanitized_stderr,
                sanitize_git_stderr(&fallback.stderr, url, url),
            ));
        }
        fallback
    } else {
        return Err(format!(
            "git archive failed for {}#{}: {}",
            display_url,
            reference,
            sanitize_git_stderr(&remote_archive.stderr, url, url),
        ));
    };
    let tmp = unique_scratch(&cache_dir, &key);
    let mut scratch = ScratchGuard::file(tmp.clone());
    {
        let file = fs::File::create(&tmp)
            .map_err(|error| format!("cannot create {}: {error}", tmp.display()))?;
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut archive = tar::Archive::new(Cursor::new(output.stdout));
        let entries = archive
            .entries()
            .map_err(|error| format!("cannot enumerate git archive: {error}"))?;
        for entry in entries {
            let mut entry = entry.map_err(|error| format!("cannot read git archive: {error}"))?;
            let kind = entry.header().entry_type();
            if matches!(
                kind,
                tar::EntryType::Regular
                    | tar::EntryType::Continuous
                    | tar::EntryType::Directory
                    | tar::EntryType::Symlink
            ) {
                let header = entry.header().clone();
                builder
                    .append(&header, &mut entry)
                    .map_err(|error| format!("cannot normalize git archive: {error}"))?;
            }
        }
        let encoder = builder
            .into_inner()
            .map_err(|error| format!("cannot finish git archive: {error}"))?;
        encoder
            .finish()
            .map_err(|error| format!("cannot finish git archive gzip: {error}"))?;
    }
    fs::rename(&tmp, &dest).map_err(|error| {
        format!(
            "cannot publish git archive {} -> {}: {error}",
            tmp.display(),
            dest.display()
        )
    })?;
    scratch.disarm();
    Ok(dest)
}

pub(crate) fn source_from_tarball_bytes(
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

/// Normalize npm's hosted-Git shortcuts to a URL accepted by `git ls-remote`.
pub(crate) fn git_clone_url(spec: &str) -> String {
    if let Some(rest) = spec
        .strip_prefix("github:")
        .or_else(|| spec.strip_prefix("github.com/"))
    {
        return format!("https://github.com/{}", rest.trim_end_matches(".git"));
    }
    if let Some(rest) = spec
        .strip_prefix("gitlab:")
        .or_else(|| spec.strip_prefix("gitlab.com/"))
    {
        return format!("https://gitlab.com/{}", rest.trim_end_matches(".git"));
    }
    if let Some(rest) = spec
        .strip_prefix("bitbucket:")
        .or_else(|| spec.strip_prefix("bitbucket.org/"))
    {
        return format!("https://bitbucket.org/{}", rest.trim_end_matches(".git"));
    }
    spec.to_owned()
}

pub(crate) fn looks_like_hosted_git(spec: &str) -> bool {
    for prefix in [
        "https://github.com/",
        "https://gitlab.com/",
        "https://bitbucket.org/",
    ] {
        if let Some(rest) = spec.strip_prefix(prefix) {
            return rest.split('/').count() == 2;
        }
    }
    let mut parts = spec.split('/');
    matches!((parts.next(), parts.next(), parts.next()), (Some(owner), Some(repo), None) if !owner.is_empty() && !repo.is_empty() && !owner.contains(':') && !repo.contains(':'))
}

pub(crate) fn hosted_git_tarball_url(spec: &str, reference: Option<&str>) -> Option<String> {
    let reference = reference.unwrap_or("HEAD");
    if let Some(rest) = spec
        .strip_prefix("github:")
        .or_else(|| spec.strip_prefix("github.com/"))
        .or_else(|| spec.strip_prefix("https://github.com/"))
    {
        return hosted_tarball("https://codeload.github.com", rest, "tar.gz", reference);
    }
    if let Some(rest) = spec
        .strip_prefix("gitlab:")
        .or_else(|| spec.strip_prefix("gitlab.com/"))
        .or_else(|| spec.strip_prefix("https://gitlab.com/"))
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
        .or_else(|| spec.strip_prefix("https://bitbucket.org/"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    #[test]
    fn read_bounded_accepts_exactly_at_limit() {
        let payload = b"hello"; // 5 bytes
        let mut cursor = Cursor::new(payload);
        let bytes = read_bounded_to_vec(&mut cursor, 5).expect("exactly-at-limit succeeds");
        assert_eq!(bytes, payload);
    }

    #[test]
    fn read_bounded_rejects_one_byte_over_limit() {
        let payload = b"hello!"; // 6 bytes
        let mut cursor = Cursor::new(payload);
        let err = read_bounded_to_vec(&mut cursor, 5).expect_err("one byte over must fail");
        assert!(
            err.contains("exceeded"),
            "expected an over-limit error; got: {err}"
        );
    }

    #[test]
    fn read_file_bounded_metadata_rejects_an_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let payload = b"oversized";
        let path = dir.path().join("big.tgz");
        fs::write(&path, payload).unwrap();
        let err = read_file_bounded_with_limit(&path, 4)
            .expect_err("must reject via the metadata fast path");
        assert!(
            err.contains("exceeds"),
            "expected an oversized-file error; got: {err}"
        );
    }

    // === Plan 013: race-safe source cache tests ===

    fn unique_suffix() -> String {
        static SUFFIX_COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = SUFFIX_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        format!("{}-{n}-{nanos}", std::process::id())
    }

    #[test]
    fn source_lock_serializes_same_key() {
        let key = unique_suffix();
        let concurrency = std::sync::Arc::new(AtomicU64::new(0));
        let max = std::sync::Arc::new(AtomicU64::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let (key, concurrency, max) = (key.clone(), concurrency.clone(), max.clone());
            handles.push(std::thread::spawn(move || {
                let _guard = source_lock("test", &key).unwrap();
                let now = concurrency.fetch_add(1, Ordering::SeqCst) + 1;
                max.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(5));
                concurrency.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(
            max.load(Ordering::SeqCst),
            1,
            "same-key source lock must serialize critical sections"
        );
    }

    #[test]
    fn source_lock_allows_different_keys_concurrently() {
        let concurrency = std::sync::Arc::new(AtomicU64::new(0));
        let max = std::sync::Arc::new(AtomicU64::new(0));
        let mut handles = Vec::new();
        for i in 0..4u32 {
            let (concurrency, max) = (concurrency.clone(), max.clone());
            let key = format!("{}-{i}", unique_suffix());
            handles.push(std::thread::spawn(move || {
                let _guard = source_lock("test", &key).unwrap();
                let now = concurrency.fetch_add(1, Ordering::SeqCst) + 1;
                max.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(20));
                concurrency.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert!(
            max.load(Ordering::SeqCst) >= 2,
            "different-key source locks must run concurrently (saw max {})",
            max.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn scratch_guard_removes_file_and_dir_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = unique_scratch(dir.path(), "hint");
        {
            let _guard = ScratchGuard::file(file_path.clone());
            fs::write(&file_path, b"x").unwrap();
        }
        assert!(!file_path.exists(), "armed file guard must remove on drop");

        let dir_path = unique_scratch(dir.path(), "hint2");
        {
            let _guard = ScratchGuard::dir(dir_path.clone());
            fs::create_dir_all(&dir_path).unwrap();
            fs::write(dir_path.join("inner"), b"y").unwrap();
        }
        assert!(!dir_path.exists(), "armed dir guard must remove on drop");
    }

    #[test]
    fn scratch_guard_disarm_keeps_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = unique_scratch(dir.path(), "hint");
        {
            let mut guard = ScratchGuard::file(path.clone());
            fs::write(&path, b"z").unwrap();
            guard.disarm();
        }
        assert!(path.exists(), "disarmed guard must keep the published file");
    }

    #[test]
    fn unique_scratch_paths_differ_within_a_process() {
        let dir = tempfile::tempdir().unwrap();
        let a = unique_scratch(dir.path(), "h");
        let b = unique_scratch(dir.path(), "h");
        assert_ne!(a, b, "successive scratch paths must be unique");
    }

    #[test]
    fn patched_tarball_cache_concurrent_same_key_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"deterministic-patched-payload".to_vec();
        let expected_hex = {
            let mut hasher = sha2::Sha512::new();
            hasher.update(&bytes);
            sha512_hex(&hasher.finalize())
        };

        let mut handles = Vec::new();
        for _ in 0..8 {
            let (base, bytes) = (dir.path().to_path_buf(), bytes.clone());
            handles.push(std::thread::spawn(move || {
                write_patched_tarball(&base, &bytes)
            }));
        }
        let mut urls = std::collections::HashSet::new();
        for handle in handles {
            urls.insert(handle.join().unwrap().unwrap());
        }
        assert_eq!(urls.len(), 1, "all writers must return the same URL");
        let url = urls.into_iter().next().unwrap();
        let path = Path::new(url.strip_prefix("file://").unwrap());
        assert!(
            path.ends_with(format!("{expected_hex}.tgz")),
            "deterministic filename must be preserved: {}",
            path.display()
        );
        assert_eq!(fs::read(path).unwrap(), bytes);

        let patches = dir.path().join(".bpm").join("patches");
        let entries: Vec<String> = fs::read_dir(&patches)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries.len(), 1, "exactly one final file: {entries:?}");
        assert!(
            !entries[0].ends_with(".tmp"),
            "no scratch residue must remain: {entries:?}"
        );
    }

    #[test]
    fn patched_tarball_cache_recovers_from_corrupt_final() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"recover-this-patch".to_vec();
        let hex = {
            let mut hasher = sha2::Sha512::new();
            hasher.update(&bytes);
            sha512_hex(&hasher.finalize())
        };
        let patches = dir.path().join(".bpm").join("patches");
        fs::create_dir_all(&patches).unwrap();
        let final_path = patches.join(format!("{hex}.tgz"));
        fs::write(&final_path, b"corrupt-truncated-by-crash").unwrap();

        let url = write_patched_tarball(dir.path(), &bytes).unwrap();
        assert_eq!(
            fs::read(url.strip_prefix("file://").unwrap()).unwrap(),
            bytes
        );
        assert!(final_path.is_file());
    }

    /// Build a gzipped tar containing a root-level `package.json` (the layout
    /// `cache_git_source_tree` extracts and validates).
    fn make_root_pkg_tgz(package_json: &str) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_path("package.json").unwrap();
        header.set_size(package_json.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        builder.append(&header, package_json.as_bytes()).unwrap();
        let tar_bytes = builder.into_inner().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&tar_bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn git_source_tree_cache_concurrent_build_is_deterministic() {
        let bytes = make_root_pkg_tgz(r#"{"name":"gitpkg","version":"1.4.0"}"#);
        let url = format!("https://example.test/{}.git", unique_suffix());
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let root = std::env::temp_dir()
            .join("bpm-git-sources")
            .join(source_key(&url, commit));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let (url, bytes) = (url.clone(), bytes.clone());
            handles.push(std::thread::spawn(move || {
                cache_git_source_tree(&url, commit, &bytes)
            }));
        }
        let mut roots = std::collections::HashSet::new();
        for handle in handles {
            roots.insert(handle.join().unwrap().unwrap());
        }
        assert_eq!(
            roots.len(),
            1,
            "all builders must return the same package root"
        );
        let package_root = roots.into_iter().next().unwrap();
        assert!(package_root.join("package.json").is_file());
        // No deterministic shared `.tmp` staging sibling remains.
        assert!(
            !root.with_extension("tmp").exists(),
            "old shared staging name must not linger"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn git_source_tree_cache_cleans_up_on_malformed_then_succeeds() {
        let url = format!("https://example.test/malformed-{}.git", unique_suffix());
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let root = std::env::temp_dir()
            .join("bpm-git-sources")
            .join(source_key(&url, commit));

        let err = cache_git_source_tree(&url, commit, b"not a tarball").unwrap_err();
        assert!(
            err.to_lowercase().contains("extract") || err.to_lowercase().contains("source"),
            "expected an extraction error; got: {err}"
        );
        assert!(
            !root.exists(),
            "no final root must remain after a failed build"
        );
        assert!(
            !root.with_extension("tmp").exists(),
            "staging must be cleaned after failure"
        );

        let bytes = make_root_pkg_tgz(r#"{"name":"ok","version":"2.0.0"}"#);
        let package_root = cache_git_source_tree(&url, commit, &bytes).unwrap();
        assert!(package_root.join("package.json").is_file());
        let _ = fs::remove_dir_all(&root);
    }

    /// Create a local Git repository with one commit and return (repo_dir, sha).
    fn local_git_repo() -> (tempfile::TempDir, String) {
        let root = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| -> bool {
            Command::new("git")
                .env("GIT_AUTHOR_NAME", "bpm")
                .env("GIT_AUTHOR_EMAIL", "bpm@bpm.local")
                .env("GIT_COMMITTER_NAME", "bpm")
                .env("GIT_COMMITTER_EMAIL", "bpm@bpm.local")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .current_dir(root.path())
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        };
        assert!(git(&["init", "-q"]), "git init failed");
        fs::write(
            root.path().join("package.json"),
            r#"{"name":"gitpkg","version":"9.9.9"}"#,
        )
        .unwrap();
        assert!(git(&["add", "-A"]), "git add failed");
        assert!(git(&["commit", "-q", "-m", "init"]), "git commit failed");
        let sha = String::from_utf8(
            Command::new("git")
                .current_dir(root.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        (root, sha)
    }

    #[test]
    fn git_archive_cache_concurrent_from_local_repo() {
        let (repo, sha) = local_git_repo();
        let url = format!("file://{}", repo.path().display());
        let cache_dir = std::env::temp_dir().join("bpm-git-archives-v2");
        fs::create_dir_all(&cache_dir).unwrap();

        let mut handles = Vec::new();
        for _ in 0..4 {
            let (url, sha) = (url.clone(), sha.clone());
            handles.push(std::thread::spawn(move || {
                git_archive_tarball(&url, &sha, &sha)
            }));
        }
        let mut dests = std::collections::HashSet::new();
        for handle in handles {
            dests.insert(handle.join().unwrap().unwrap());
        }
        assert_eq!(
            dests.len(),
            1,
            "concurrent same-key archive must yield one path"
        );
        let dest = dests.into_iter().next().unwrap();
        assert!(dest.is_file());
        let bytes = fs::read(&dest).unwrap();
        assert!(
            manifest_from_tarball(&bytes).is_ok(),
            "cached archive must be a valid gzip+tar with a package manifest"
        );
        let _ = fs::remove_file(&dest);
    }

    #[test]
    fn git_archive_cache_recovers_from_truncated_final() {
        let (repo, sha) = local_git_repo();
        let url = format!("file://{}", repo.path().display());
        let key = {
            let mut hasher = sha2::Sha512::new();
            hasher.update(url.as_bytes());
            hasher.update([0]);
            hasher.update(sha.as_bytes());
            hasher
                .finalize()
                .iter()
                .take(16)
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let cache_dir = std::env::temp_dir().join("bpm-git-archives-v2");
        fs::create_dir_all(&cache_dir).unwrap();
        let dest = cache_dir.join(format!("{key}.tgz"));
        fs::write(&dest, b"truncated").unwrap();

        let got = git_archive_tarball(&url, &sha, &sha).unwrap();
        assert_eq!(got, dest, "must reuse the deterministic cache path");
        assert!(
            manifest_from_tarball(&fs::read(&dest).unwrap()).is_ok(),
            "corrupt cache must be rebuilt into a valid archive"
        );
        let _ = fs::remove_file(&dest);
    }
}
