//! Non-registry dependency source resolution (Git, file, tarball, patch).

use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine;
use sha2::Digest;

use super::workspace_metadata;
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
    let bytes =
        fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
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
        return fs::read(path).map_err(|error| format!("cannot read {path}: {error}"));
    }
    if !url.contains("://") {
        return fs::read(url).map_err(|error| format!("cannot read {url}: {error}"));
    }
    let mut response = http.stream(url).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    response
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read tarball response: {error}"))?;
    Ok(bytes)
}

pub(crate) fn write_patched_tarball(base_dir: &Path, bytes: &[u8]) -> Result<String, String> {
    let mut hasher = sha2::Sha512::new();
    hasher.update(bytes);
    let hex = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let root = if base_dir.is_dir() {
        base_dir.to_path_buf()
    } else {
        std::env::temp_dir()
    };
    let dir = root.join(".bpm").join("patches");
    fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create patch cache {}: {error}", dir.display()))?;
    let path = dir.join(format!("{hex}.tgz"));
    if !path.exists() {
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, bytes)
            .map_err(|error| format!("cannot write patched tarball {}: {error}", tmp.display()))?;
        fs::rename(&tmp, &path).map_err(|error| {
            format!(
                "cannot publish patched tarball {} -> {}: {error}",
                tmp.display(),
                path.display()
            )
        })?;
    }
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
    let bytes = fs::read(&tarball)
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
    if Path::new(source).is_dir() {
        return Command::new("git")
            .args(["-C", source, "archive", "--format=tar", "--", commit])
            .output()
            .map_err(|error| format!("cannot archive local Git commit: {error}"));
    }
    let mut hasher = sha2::Sha512::new();
    hasher.update(url.as_bytes());
    hasher.update([0]);
    hasher.update(commit.as_bytes());
    let clone_dir = std::env::temp_dir()
        .join("bpm-git-clones-v1")
        .join(hex::encode(hasher.finalize()));
    if !clone_dir.join(".git").is_dir() {
        if let Some(parent) = clone_dir.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create Git clone cache: {error}"))?;
        }
        let clone = Command::new("git")
            .args([
                "clone",
                "--no-checkout",
                "--",
                url,
                &clone_dir.display().to_string(),
            ])
            .output()
            .map_err(|error| format!("cannot clone Git source: {error}"))?;
        if !clone.status.success() {
            return Err(String::from_utf8_lossy(&clone.stderr).trim().to_owned());
        }
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
    let mut hasher = sha2::Sha512::new();
    hasher.update(url.as_bytes());
    hasher.update([0]);
    hasher.update(commit.as_bytes());
    let key = hex::encode(hasher.finalize());
    let root = std::env::temp_dir().join("bpm-git-sources").join(key);
    let package_root = if root.join("package.json").is_file() {
        root.clone()
    } else if root.join("package/package.json").is_file() {
        root.join("package")
    } else {
        let staging = root.with_extension("tmp");
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging)
            .map_err(|error| format!("cannot create Git source cache: {error}"))?;
        let archive_path = staging.join("source.tgz");
        fs::write(&archive_path, bytes)
            .map_err(|error| format!("cannot stage Git source archive: {error}"))?;
        crate::archive::extract(&archive_path, &staging)
            .map_err(|error| format!("cannot extract Git source archive: {error}"))?;
        let _ = fs::remove_file(&archive_path);
        if staging.join("package.json").is_file() {
            if let Some(parent) = root.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create Git source cache: {error}"))?;
            }
            fs::rename(&staging, &root).map_err(|error| {
                format!(
                    "cannot publish Git source cache {}: {error}",
                    root.display()
                )
            })?;
            root.clone()
        } else {
            if let Some(parent) = root.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create Git source cache: {error}"))?;
            }
            fs::rename(&staging, &root).map_err(|error| {
                format!(
                    "cannot publish Git source cache {}: {error}",
                    root.display()
                )
            })?;
            root.clone()
        }
    };
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
        .map_err(|error| format!("cannot execute git ls-remote for {url}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-remote failed for {remote}#{requested}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
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
    candidate.ok_or_else(|| format!("git reference {requested:?} does not resolve in {remote}"))
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
    if dest.is_file() {
        return Ok(dest);
    }
    let remote_archive = Command::new("git")
        .args([
            "archive",
            "--format=tar",
            &format!("--remote={url}"),
            "--",
            reference,
        ])
        .output()
        .map_err(|error| format!("cannot execute git archive for {url}: {error}"))?;
    let output = if remote_archive.status.success() {
        remote_archive
    } else if is_full_git_commit(reference) {
        let fallback = archive_git_commit_locally(url, resolved_commit).map_err(|error| {
            format!(
                "git archive failed for {url}#{reference}: {}; local commit fallback failed: {error}",
                String::from_utf8_lossy(&remote_archive.stderr).trim()
            )
        })?;
        if !fallback.status.success() {
            return Err(format!(
                "git archive failed for {url}#{reference}: {}; local commit fallback failed: {}",
                String::from_utf8_lossy(&remote_archive.stderr).trim(),
                String::from_utf8_lossy(&fallback.stderr).trim()
            ));
        }
        fallback
    } else {
        return Err(format!(
            "git archive failed for {url}#{reference}: {}",
            String::from_utf8_lossy(&remote_archive.stderr).trim()
        ));
    };
    let tmp = dest.with_extension("tmp");
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
