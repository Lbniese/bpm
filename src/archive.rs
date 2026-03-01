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
    /// A decompression/resource budget was exceeded. Identifies the resource
    /// (`entries`, `file bytes`, or `total bytes`), the numeric limit, and the
    /// non-sensitive archive-relative path when one exists. Never reports file
    /// contents.
    #[error("extraction {resource} limit exceeded ({limit} max){path}")]
    ResourceLimit {
        resource: &'static str,
        limit: u64,
        path: String,
    },
}

/// Maximum total extracted/materialized regular-file bytes per archive.
const MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB
/// Maximum size of a single extracted regular file.
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024; // 512 MiB
/// Maximum number of archive entries (headers) and generated output entries.
const MAX_ENTRIES: u64 = 100_000;

/// Configurable extraction budgets. Production uses [`ExtractionLimits::default`];
/// tests inject tiny limits without allocating large fixtures.
#[derive(Debug, Clone, Copy)]
struct ExtractionLimits {
    max_total_bytes: u64,
    max_file_bytes: u64,
    max_entries: u64,
}

impl Default for ExtractionLimits {
    fn default() -> Self {
        Self {
            max_total_bytes: MAX_TOTAL_BYTES,
            max_file_bytes: MAX_FILE_BYTES,
            max_entries: MAX_ENTRIES,
        }
    }
}

/// Running accounting of extracted/materialized bytes and entries against an
/// [`ExtractionLimits`]. All mutations use checked arithmetic; a value exactly
/// at a limit is allowed and limit-plus-one is rejected.
#[derive(Debug)]
struct ExtractionBudget {
    limits: ExtractionLimits,
    entries: u64,
    total_bytes: u64,
}

impl ExtractionBudget {
    fn new(limits: ExtractionLimits) -> Self {
        Self {
            limits,
            entries: 0,
            total_bytes: 0,
        }
    }

    /// Consume one entry (a tar header or a generated output entry). Returns
    /// `Err` once the entry budget is exhausted.
    fn observe_entry(&mut self, rel: Option<&Path>) -> Result<(), ExtractError> {
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| resource_overflow("entries", self.limits.max_entries, rel))?;
        if self.entries > self.limits.max_entries {
            return Err(ExtractError::ResourceLimit {
                resource: "entries",
                limit: self.limits.max_entries,
                path: entry_path_note(rel),
            });
        }
        Ok(())
    }

    /// Reserve a regular file's declared size against the per-file and total
    /// budgets before any bytes are written. Called exactly once per file; the
    /// verified copy later does not add to the total again.
    fn reserve_file(&mut self, declared: u64, rel: &Path) -> Result<(), ExtractError> {
        if declared > self.limits.max_file_bytes {
            return Err(ExtractError::ResourceLimit {
                resource: "file bytes",
                limit: self.limits.max_file_bytes,
                path: entry_path_note(Some(rel)),
            });
        }
        let new_total = self.total_bytes.checked_add(declared).ok_or_else(|| {
            resource_overflow("total bytes", self.limits.max_total_bytes, Some(rel))
        })?;
        if new_total > self.limits.max_total_bytes {
            return Err(ExtractError::ResourceLimit {
                resource: "total bytes",
                limit: self.limits.max_total_bytes,
                path: entry_path_note(Some(rel)),
            });
        }
        self.total_bytes = new_total;
        Ok(())
    }

    /// Account for bytes generated by Windows link materialization against the
    /// same total budget.
    #[cfg(windows)]
    fn charge_materialized(&mut self, bytes: u64, rel: &Path) -> Result<(), ExtractError> {
        let new_total = self.total_bytes.checked_add(bytes).ok_or_else(|| {
            resource_overflow("total bytes", self.limits.max_total_bytes, Some(rel))
        })?;
        if new_total > self.limits.max_total_bytes {
            return Err(ExtractError::ResourceLimit {
                resource: "total bytes",
                limit: self.limits.max_total_bytes,
                path: entry_path_note(Some(rel)),
            });
        }
        self.total_bytes = new_total;
        Ok(())
    }
}

fn entry_path_note(rel: Option<&Path>) -> String {
    rel.map(|p| format!(" at {}", p.display()))
        .unwrap_or_default()
}

fn resource_overflow(resource: &'static str, limit: u64, rel: Option<&Path>) -> ExtractError {
    ExtractError::ResourceLimit {
        resource,
        limit,
        path: entry_path_note(rel),
    }
}

/// Extract the gzip+tar archive at `archive_path` into `image_root`.
///
/// `image_root` must already exist (the store creates it). The caller writes
/// into a temporary directory and renames atomically (see [`crate::store`]).
pub fn extract(archive_path: &Path, image_root: &Path) -> Result<(), ExtractError> {
    extract_with_limits(archive_path, image_root, ExtractionLimits::default())
}

fn extract_with_limits(
    archive_path: &Path,
    image_root: &Path,
    limits: ExtractionLimits,
) -> Result<(), ExtractError> {
    let mut budget = ExtractionBudget::new(limits);
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
        // Charge every tar header toward the entry budget before duplicate
        // detection so an entry-heavy archive cannot force unbounded work.
        budget.observe_entry(Some(&rel))?;
        if !seen.insert(rel.clone()) {
            return Err(ExtractError::DuplicateEntry(rel.display().to_string()));
        }

        let dest = image_root.join(&rel);
        match entry.header().entry_type() {
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                // Validate the declared size before creating any destination
                // path, so an oversized file is rejected before allocation.
                let declared = entry.header().size().unwrap_or(0);
                budget.reserve_file(declared, &rel)?;
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
        materialize_windows_link(
            image_root,
            link,
            target,
            &deferred_links,
            &mut visiting,
            &mut budget,
        )?;
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
    budget: &mut ExtractionBudget,
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
            materialize_windows_link(
                root,
                &normalized_target,
                next_target,
                deferred,
                visiting,
                budget,
            )?;
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
            budget,
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
#[allow(clippy::too_many_arguments)]
fn copy_windows_tree(
    root: &Path,
    source_rel: &Path,
    destination: PathBuf,
    _deferred: &[(PathBuf, PathBuf)],
    _visiting: &mut HashSet<PathBuf>,
    budget: &mut ExtractionBudget,
) -> Result<(), ExtractError> {
    let source = root.join(source_rel);
    fs::create_dir_all(&destination).map_err(|source| write_err(&destination, source))?;
    for entry in fs::read_dir(&source).map_err(|error| write_err(&source, error))? {
        let entry = entry.map_err(|error| write_err(&source, error))?;
        let child_rel = source_rel.join(entry.file_name());
        let child_dest = destination.join(entry.file_name());
        budget.observe_entry(Some(&child_rel))?;
        if entry
            .file_type()
            .map_err(|source| write_err(&entry.path(), source))?
            .is_dir()
        {
            copy_windows_tree(root, &child_rel, child_dest, _deferred, _visiting, budget)?;
        } else {
            let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
            budget.charge_materialized(bytes, &child_rel)?;
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
    // Bound the detection scan independently so a nonstandard archive cannot
    // force unlimited enumeration before the main extraction pass. The
    // conventional `package/` archive returns on its first entry regardless.
    let mut budget = ExtractionBudget::new(ExtractionLimits::default());
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
        budget.observe_entry(None)?;
        let raw = entry
            .path()
            .map_err(|e| ExtractError::InvalidArchive(format!("invalid entry path header: {e}")))?
            .into_owned();
        let mut comps = raw.components();
        let Some(Component::Normal(first)) = comps.next() else {
            continue;
        };
        let first = PathBuf::from(first);
        // `strip_package_prefix` strips the conventional npm `package/`
        // root unconditionally, even when an archive contains mixed roots.
        // Once the first entry proves this is an npm-shaped archive, there is
        // no need to decompress and enumerate the entire archive a second
        // time before extraction.
        if first == Path::new(PACKAGE_PREFIX) {
            return Ok(Some(first));
        }
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

    // ── extraction resource-budget regressions ──────────────────────────

    /// Build a gzip+tar of regular files under `package/` (npm convention) from
    /// (path, data) pairs. Uses only already-available deps so the lib's unit
    /// tests can exercise the private limit-parameterized extraction path.
    fn build_tgz_files(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(enc);
        for (path, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_path(path).unwrap();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            builder.append(&h, *data).unwrap();
        }
        let mut enc = builder.into_inner().unwrap();
        enc.flush().unwrap();
        enc.finish().unwrap()
    }

    fn extract_limits(
        tgz: &[u8],
        limits: ExtractionLimits,
    ) -> (Result<(), ExtractError>, tempfile::TempDir) {
        let image = tempfile::tempdir().unwrap();
        let archive = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(archive.path(), tgz).unwrap();
        let result = extract_with_limits(archive.path(), image.path(), limits);
        (result, image)
    }

    fn limits(max_entries: u64, max_file_bytes: u64, max_total_bytes: u64) -> ExtractionLimits {
        ExtractionLimits {
            max_entries,
            max_file_bytes,
            max_total_bytes,
        }
    }

    #[test]
    fn extraction_under_limits_succeeds_identically() {
        let tgz = build_tgz_files(&[
            ("package/a.txt", b"hello"),
            ("package/sub/b.txt", b"world!"),
        ]);
        let (result, image) = extract_limits(&tgz, limits(1000, 1000, 1000));
        result.expect("small archive extracts under generous limits");
        assert_eq!(std::fs::read(image.path().join("a.txt")).unwrap(), b"hello");
        assert_eq!(
            std::fs::read(image.path().join("sub/b.txt")).unwrap(),
            b"world!"
        );
    }

    #[test]
    fn entry_count_at_limit_succeeds_and_one_over_fails() {
        // Exactly two entries is allowed at max_entries = 2.
        let tgz = build_tgz_files(&[("package/a", b"x"), ("package/b", b"y")]);
        let (result, _image) = extract_limits(&tgz, limits(2, 1000, 1000));
        result.expect("exactly at the entry limit succeeds");

        // Three entries exceeds the limit.
        let tgz = build_tgz_files(&[
            ("package/a", b"x"),
            ("package/b", b"y"),
            ("package/c", b"z"),
        ]);
        let (result, _image) = extract_limits(&tgz, limits(2, 1000, 1000));
        let err = result.expect_err("one over entry limit must fail");
        let text = err.to_string();
        assert!(text.contains("entries"), "{text}");
        assert!(text.contains("2 max"), "{text}");
    }

    #[test]
    fn per_file_size_one_over_rejected_before_destination_creation() {
        // A single file with a declared size (51) one over the per-file limit
        // (50) must be rejected before its destination is created.
        let tgz = build_tgz_files(&[("package/big.bin", &[b'.'; 51])]);
        let (result, image) = extract_limits(&tgz, limits(1000, 50, 1000));
        let err = result.expect_err("per-file size over limit must fail");
        let text = err.to_string();
        assert!(text.contains("file bytes"), "{text}");
        assert!(text.contains("50 max"), "{text}");
        // No destination file was created.
        assert!(!image.path().join("big.bin").exists());
        assert!(std::fs::read_dir(image.path()).unwrap().count() == 0);
    }

    #[test]
    fn aggregate_bytes_exact_succeeds_and_one_over_fails() {
        // Two files summing exactly to the total limit (5 + 5 = 10) succeed.
        let tgz = build_tgz_files(&[("package/a", b"aaaaa"), ("package/b", b"bbbbb")]);
        let (result, _image) = extract_limits(&tgz, limits(1000, 1000, 10));
        result.expect("aggregate exactly at the total limit succeeds");

        // 5 + 6 = 11 exceeds the total limit (10) on the second file.
        let tgz = build_tgz_files(&[("package/a", b"aaaaa"), ("package/b", b"bbbbbb")]);
        let (result, image) = extract_limits(&tgz, limits(1000, 1000, 10));
        let err = result.expect_err("aggregate over total limit must fail");
        let text = err.to_string();
        assert!(text.contains("total bytes"), "{text}");
        assert!(text.contains("10 max"), "{text}");
        // The first file was written before the second exceeded the budget.
        assert!(image.path().join("a").exists());
        // The oversized second file was never created.
        assert!(!image.path().join("b").exists());
    }

    #[test]
    fn budget_arithmetic_overflow_is_rejected() {
        // With file/total caps at u64::MAX, reserving a maximal declared size
        // after a small reservation overflows checked_add and is rejected
        // rather than wrapping.
        let mut budget = ExtractionBudget::new(limits(u64::MAX, u64::MAX, u64::MAX));
        budget
            .reserve_file(1, Path::new("package/a"))
            .expect("first small reservation ok");
        let err = budget
            .reserve_file(u64::MAX, Path::new("package/b"))
            .expect_err("overflow must be rejected");
        assert!(err.to_string().contains("total bytes"), "{}", err);
    }
}
