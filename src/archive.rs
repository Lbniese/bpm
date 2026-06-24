//! Safe one-time extraction of a package tarball into an immutable image.
//!
//! npm tarballs are gzip-compressed tar archives whose entries share a leading
//! `package/` directory. We strip that prefix so the image root holds the
//! package contents directly (IMPLEMENTATION §8: "normalize package root
//! layout").
//!
//! Security (IMPLEMENTATION §8, §21): rejected or handled explicitly —
//! - absolute entry paths (`/etc/passwd`)
//! - path traversal (`..`)
//! - device/fifo/hardlink/other unsupported entry types
//! - symlinks whose target escapes the image root (prevents following an
//!   attacker-controlled link to write outside the store)
//! - duplicate entries (suspicious in package tarballs, rejected for safety)
//!
//! Permissions: executable bits from the archive are preserved, but setuid /
//! setgid / sticky bits and world-write are dropped (IMPLEMENTATION §21:
//! "avoid world-writable store paths").

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use thiserror::Error;

/// Leading component of npm-packed tarball entries, stripped on extraction.
const PACKAGE_PREFIX: &str = "package";

#[derive(Debug, Error)]
pub enum ExtractError {
    #[error("cannot read archive at {path}: {source}")]
    Read { path: String, source: io::Error },
    #[error("archive is not a valid gzip/tar stream: {0}")]
    InvalidArchive(String),
    #[error("unsafe entry path {path}: {reason}")]
    UnsafePath { path: String, reason: String },
    #[error("unsafe symlink at {link} -> {target} (target escapes image root)")]
    UnsafeSymlink { link: String, target: String },
    #[error("unsupported entry type {typ} at {path}")]
    UnsupportedEntry { typ: String, path: String },
    #[error("duplicate entry: {0}")]
    DuplicateEntry(String),
    #[error("io error writing image at {path}: {source}")]
    Write { path: String, source: io::Error },
    #[error("symlinks are unsupported on this platform")]
    SymlinksUnsupported,
}

/// Extract the gzip+tar archive at `archive_path` into `image_root`.
///
/// `image_root` must already exist (the store creates it). The caller writes
/// into a temporary directory and renames atomically (see [`crate::store`]).
pub fn extract(archive_path: &Path, image_root: &Path) -> Result<(), ExtractError> {
    let strip_prefix = detect_archive_root_prefix(archive_path)?;
    let file = fs::File::open(archive_path).map_err(|source| ExtractError::Read {
        path: archive_path.display().to_string(),
        source,
    })?;
    let gz = GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    let entries = archive
        .entries()
        .map_err(|e| ExtractError::InvalidArchive(format!("cannot enumerate tar entries: {e}")))?;

    let mut seen: HashSet<PathBuf> = HashSet::new();
    #[cfg(windows)]
    let mut deferred_links: Vec<(PathBuf, PathBuf)> = Vec::new();
    for entry_result in entries {
        let mut entry = entry_result
            .map_err(|e| ExtractError::InvalidArchive(format!("corrupt tar entry: {e}")))?;
        let raw = entry
            .path()
            .map_err(|e| ExtractError::InvalidArchive(format!("invalid entry path header: {e}")))?
            .into_owned();
        let stripped = strip_package_prefix(&raw, strip_prefix.as_deref());
        let rel =
            validate_returned_relative(&stripped).map_err(|reason| ExtractError::UnsafePath {
                path: raw.display().to_string(),
                reason,
            })?;
        if rel.as_os_str().is_empty() {
            // Root directory entry (e.g. `package/`); image_root already exists.
            continue;
        }
        if !seen.insert(rel.clone()) {
            return Err(ExtractError::DuplicateEntry(rel.display().to_string()));
        }

        let dest = image_root.join(&rel);
        match entry.header().entry_type() {
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).map_err(|source| write_err(parent, source))?;
                }
                let mut out = fs::File::create(&dest).map_err(|source| write_err(&dest, source))?;
                io::copy(&mut entry, &mut out).map_err(|source| write_err(&dest, source))?;
                // The image is built in a private temporary directory and
                // published with one atomic rename by `ArtifactStore`.
                // Fsyncing every file here serialized extraction on large
                // packages without improving the all-or-nothing visibility
                // guarantee; callers can safely retry an unpublished temp
                // image after a crash.
                let mode = entry.header().mode().unwrap_or(0o644);
                apply_mode(&dest, mode).map_err(|source| write_err(&dest, source))?;
            }
            tar::EntryType::Directory => {
                fs::create_dir_all(&dest).map_err(|source| write_err(&dest, source))?;
                let mode = entry.header().mode().unwrap_or(0o755);
                // Directory mode is advisory; ignore failure on read-only trees.
                let _ = apply_mode(&dest, mode);
            }
            tar::EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .map_err(|e| {
                        ExtractError::InvalidArchive(format!("invalid symlink header: {e}"))
                    })?
                    .ok_or_else(|| {
                        ExtractError::InvalidArchive(format!(
                            "symlink entry missing link name: {}",
                            raw.display()
                        ))
                    })?
                    .into_owned();
                if !symlink_within_root(&rel, &target) {
                    return Err(ExtractError::UnsafeSymlink {
                        link: rel.display().to_string(),
                        target: target.display().to_string(),
                    });
                }
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).map_err(|source| write_err(parent, source))?;
                }
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(&target, &dest)
                        .map_err(|source| write_err(&dest, source))?;
                }
                #[cfg(all(not(unix), not(windows)))]
                {
                    let _ = target;
                    return Err(ExtractError::SymlinksUnsupported);
                }
                #[cfg(windows)]
                {
                    // Windows installs must not require Developer Mode or
                    // elevation. Resolve safe links after all regular entries
                    // have been extracted, which also supports forward links.
                    deferred_links.push((rel.clone(), target));
                }
            }
            other => {
                return Err(ExtractError::UnsupportedEntry {
                    typ: format!("{other:?}"),
                    path: rel.display().to_string(),
                });
            }
        }
    }

    #[cfg(windows)]
    for (link, target) in &deferred_links {
        let mut visiting = HashSet::new();
        materialize_windows_link(image_root, link, target, &deferred_links, &mut visiting)?;
    }
    Ok(())
}

#[cfg(windows)]
fn materialize_windows_link(
    root: &Path,
    link: &Path,
    target: &Path,
    deferred: &[(PathBuf, PathBuf)],
    visiting: &mut HashSet<PathBuf>,
) -> Result<(), ExtractError> {
    let normalized_target = resolve_relative(link.parent().unwrap_or(Path::new("")), target)?;
    if !visiting.insert(link.to_path_buf()) {
        return Err(ExtractError::UnsafeSymlink {
            link: link.display().to_string(),
            target: target.display().to_string(),
        });
    }
    let source = root.join(&normalized_target);
    if !source.exists() {
        if let Some((_, next_target)) = deferred
            .iter()
            .find(|(candidate, _)| candidate == &normalized_target)
        {
            materialize_windows_link(root, &normalized_target, next_target, deferred, visiting)?;
        }
    }
    if !source.exists() {
        return Err(ExtractError::UnsafeSymlink {
            link: link.display().to_string(),
            target: target.display().to_string(),
        });
    }
    if let Some(parent) = root.join(link).parent() {
        fs::create_dir_all(parent).map_err(|source| write_err(parent, source))?;
    }
    if source.is_dir() {
        copy_windows_tree(
            root,
            &normalized_target,
            root.join(link),
            deferred,
            visiting,
        )?;
    } else {
        fs::copy(&source, root.join(link)).map_err(|source| write_err(&root.join(link), source))?;
    }
    visiting.remove(link);
    Ok(())
}

#[cfg(windows)]
fn resolve_relative(parent: &Path, target: &Path) -> Result<PathBuf, ExtractError> {
    let mut parts = parent.to_path_buf();
    for component in target.components() {
        match component {
            Component::Normal(value) => parts.push(value),
            Component::CurDir => {}
            Component::ParentDir => {
                if !parts.pop() {
                    return Err(ExtractError::UnsafeSymlink {
                        link: parent.display().to_string(),
                        target: target.display().to_string(),
                    });
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ExtractError::UnsafeSymlink {
                    link: parent.display().to_string(),
                    target: target.display().to_string(),
                })
            }
        }
    }
    Ok(parts)
}

#[cfg(windows)]
fn copy_windows_tree(
    root: &Path,
    source_rel: &Path,
    destination: PathBuf,
    deferred: &[(PathBuf, PathBuf)],
    visiting: &mut HashSet<PathBuf>,
) -> Result<(), ExtractError> {
    let source = root.join(source_rel);
    fs::create_dir_all(&destination).map_err(|source| write_err(&destination, source))?;
    for entry in fs::read_dir(&source).map_err(|error| write_err(&source, error))? {
        let entry = entry.map_err(|error| write_err(&source, error))?;
        let child_rel = source_rel.join(entry.file_name());
        let child_dest = destination.join(entry.file_name());
        if entry
            .file_type()
            .map_err(|source| write_err(&entry.path(), source))?
            .is_dir()
        {
            copy_windows_tree(root, &child_rel, child_dest, deferred, visiting)?;
        } else {
            fs::copy(entry.path(), &child_dest).map_err(|source| write_err(&child_dest, source))?;
        }
    }
    Ok(())
}

/// Detect the archive root directory to strip.
///
/// npm tarballs conventionally use `package/`, while hosted Git archives use a
/// generated `repo-ref/` directory. Strip a common first component only when a
/// `package.json` lives directly under that component; archives already rooted
/// at `package.json` are left untouched.
fn detect_archive_root_prefix(archive_path: &Path) -> Result<Option<PathBuf>, ExtractError> {
    let file = fs::File::open(archive_path).map_err(|source| ExtractError::Read {
        path: archive_path.display().to_string(),
        source,
    })?;
    let gz = GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    let entries = archive
        .entries()
        .map_err(|e| ExtractError::InvalidArchive(format!("cannot enumerate tar entries: {e}")))?;
    let mut common: Option<PathBuf> = None;
    let mut has_prefixed_manifest = false;
    for entry in entries {
        let entry =
            entry.map_err(|e| ExtractError::InvalidArchive(format!("corrupt tar entry: {e}")))?;
        let raw = entry
            .path()
            .map_err(|e| ExtractError::InvalidArchive(format!("invalid entry path header: {e}")))?
            .into_owned();
        let mut comps = raw.components();
        let Some(Component::Normal(first)) = comps.next() else {
            continue;
        };
        let first = PathBuf::from(first);
        if common.as_ref().is_some_and(|value| value != &first) {
            return Ok(None);
        }
        common = Some(first.clone());
        if comps.as_path() == Path::new("package.json") {
            has_prefixed_manifest = true;
        }
    }
    Ok(common.filter(|_| has_prefixed_manifest))
}

/// Drop the detected archive root component if present.
fn strip_package_prefix(p: &Path, detected_prefix: Option<&Path>) -> PathBuf {
    let mut comps = p.components();
    if let Some(Component::Normal(first)) = comps.next() {
        let should_strip = detected_prefix.is_some_and(|prefix| prefix == Path::new(first))
            || first == std::ffi::OsStr::new(PACKAGE_PREFIX);
        if should_strip {
            return comps.as_path().to_path_buf();
        }
    }
    p.to_path_buf()
}

/// Normalize a relative path, rejecting absolute components and `..`.
///
/// Returns the cleaned path, or `Err(reason)` on a policy violation. A path
/// that cleans to empty (the image root itself) is returned as empty; callers
/// skip root entries.
fn validate_returned_relative(p: &Path) -> Result<PathBuf, String> {
    if p.as_os_str().is_empty() {
        return Ok(PathBuf::new());
    }
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err("path traversal (\"..\") is not allowed".to_string())
            }
            Component::RootDir => return Err("absolute paths are not allowed".to_string()),
            Component::Prefix(_) => {
                return Err("windows drive/prefix paths are not allowed".to_string());
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Ok(PathBuf::new());
    }
    Ok(out)
}

/// Whether a symlink `target` resolved from the link at `rel` stays within the
/// image root. Resolves lexically (no filesystem access, so it cannot be
/// fooled by existing symlinks) and rejects absolute targets.
fn symlink_within_root(rel: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    // Depth of the directory containing the link, relative to the image root.
    let mut depth: i32 = 0;
    for c in rel.parent().unwrap_or(Path::new("")).components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            _ => return false,
        }
    }
    for c in target.components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    depth >= 0
}

#[cfg(unix)]
fn apply_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // Keep rwx up to owner+group, drop special bits and world-write.
    let masked = (mode & 0o777) & !0o002;
    fs::set_permissions(path, fs::Permissions::from_mode(masked))
}

#[cfg(not(unix))]
fn apply_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

fn write_err(path: &Path, source: io::Error) -> ExtractError {
    ExtractError::Write {
        path: path.display().to_string(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_package_prefix() {
        assert_eq!(
            strip_package_prefix(Path::new("package/file.js"), None),
            Path::new("file.js")
        );
        assert_eq!(
            strip_package_prefix(Path::new("package/sub/x.js"), None),
            Path::new("sub/x.js")
        );
        assert_eq!(
            strip_package_prefix(Path::new("other/x.js"), None),
            Path::new("other/x.js")
        );
    }

    #[test]
    fn rejects_absolute_and_traversal() {
        assert!(validate_returned_relative(Path::new("/etc/passwd")).is_err());
        assert!(validate_returned_relative(Path::new("../x")).is_err());
        assert!(validate_returned_relative(Path::new("a/../../x")).is_err());
        assert_eq!(
            validate_returned_relative(Path::new("a/b/./c")).unwrap(),
            Path::new("a/b/c")
        );
    }

    #[test]
    fn symlink_must_stay_within_root() {
        assert!(!symlink_within_root(
            Path::new("link"),
            Path::new("/etc/passwd")
        ));
        assert!(!symlink_within_root(
            Path::new("link"),
            Path::new("../../escape")
        ));
        assert!(symlink_within_root(Path::new("link"), Path::new("target")));
        assert!(symlink_within_root(Path::new("a/link"), Path::new("../b")));
        // Escapes after resolving: a/b/link -> ../../.. is two ups from b -> above root.
        assert!(!symlink_within_root(
            Path::new("a/b/link"),
            Path::new("../../..")
        ));
    }
}
