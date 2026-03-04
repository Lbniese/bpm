//! Minimal deterministic patch protocol support.
//!
//! BPM accepts dependency specs of the form `patch:<source>#<patch-file>` and
//! applies a unified diff to the resolved package tarball during native
//! resolution. The resulting patched tarball is content-addressed and installed
//! through the normal immutable artifact pipeline.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PatchError {
    #[error("patch does not contain any file diffs")]
    Empty,
    #[error("malformed patch: {0}")]
    Malformed(String),
    #[error("patch target {0:?} was not found in the package tarball")]
    MissingTarget(String),
    #[error("hunk for {path:?} did not match at line {line}")]
    HunkMismatch { path: String, line: usize },
    #[error("tarball read failed: {0}")]
    Read(String),
    #[error("tarball write failed: {0}")]
    Write(String),
    #[error("patched file {0:?} is not valid UTF-8")]
    Utf8(String),
}

#[derive(Debug, Clone)]
struct TarEntry {
    path: String,
    mode: u32,
    data: Vec<u8>,
}

#[derive(Debug, Clone)]
struct FilePatch {
    target: String,
    hunks: Vec<Hunk>,
}

#[derive(Debug, Clone)]
struct Hunk {
    old_start: usize,
    lines: Vec<HunkLine>,
}

#[derive(Debug, Clone)]
enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

/// Apply a unified diff to an npm-style gzip tarball and return patched bytes.
pub fn apply_unified_patch_to_tgz(tarball: &[u8], patch_text: &str) -> Result<Vec<u8>, PatchError> {
    let patches = parse_patch(patch_text)?;
    let mut entries = read_entries(tarball)?;
    for patch in patches {
        let index = find_target(&entries, &patch.target)
            .ok_or_else(|| PatchError::MissingTarget(patch.target.clone()))?;
        let original = String::from_utf8(entries[index].data.clone())
            .map_err(|_| PatchError::Utf8(entries[index].path.clone()))?;
        let patched = apply_file_patch(&entries[index].path, &original, &patch)?;
        entries[index].data = patched.into_bytes();
    }
    write_entries(&entries)
}

fn read_entries(tarball: &[u8]) -> Result<Vec<TarEntry>, PatchError> {
    let gz = flate2::read::GzDecoder::new(Cursor::new(tarball));
    let mut archive = tar::Archive::new(gz);
    let mut entries = Vec::new();
    let archive_entries = archive
        .entries()
        .map_err(|error| PatchError::Read(error.to_string()))?;
    for entry in archive_entries {
        let mut entry = entry.map_err(|error| PatchError::Read(error.to_string()))?;
        let entry_type = entry.header().entry_type();
        if !entry_type.is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|error| PatchError::Read(error.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        let mode = entry.header().mode().unwrap_or(0o644);
        let mut data = Vec::new();
        entry
            .read_to_end(&mut data)
            .map_err(|error| PatchError::Read(error.to_string()))?;
        entries.push(TarEntry { path, mode, data });
    }
    Ok(entries)
}

fn write_entries(entries: &[TarEntry]) -> Result<Vec<u8>, PatchError> {
    let mut out = Vec::new();
    {
        let enc = flate2::write::GzEncoder::new(&mut out, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);
        for entry in entries {
            let mut header = tar::Header::new_gnu();
            header
                .set_path(&entry.path)
                .map_err(|error| PatchError::Write(error.to_string()))?;
            header.set_mode(entry.mode);
            header.set_size(entry.data.len() as u64);
            header.set_cksum();
            tar.append(&header, Cursor::new(&entry.data))
                .map_err(|error| PatchError::Write(error.to_string()))?;
        }
        let enc = tar
            .into_inner()
            .map_err(|error| PatchError::Write(error.to_string()))?;
        enc.finish()
            .map_err(|error| PatchError::Write(error.to_string()))?;
    }
    Ok(out)
}

fn parse_patch(text: &str) -> Result<Vec<FilePatch>, PatchError> {
    let lines = text.lines().collect::<Vec<_>>();
    let mut index = 0;
    let mut patches = Vec::new();
    while index < lines.len() {
        if !lines[index].starts_with("--- ") {
            index += 1;
            continue;
        }
        index += 1;
        if index >= lines.len() || !lines[index].starts_with("+++ ") {
            return Err(PatchError::Malformed("expected +++ after ---".into()));
        }
        let target = normalize_patch_path(lines[index].trim_start_matches("+++ "))?;
        index += 1;
        let mut hunks = Vec::new();
        while index < lines.len() {
            if lines[index].starts_with("--- ") {
                break;
            }
            if !lines[index].starts_with("@@ ") {
                index += 1;
                continue;
            }
            let old_start = parse_old_start(lines[index])?;
            index += 1;
            let mut hunk_lines = Vec::new();
            while index < lines.len()
                && !lines[index].starts_with("@@ ")
                && !lines[index].starts_with("--- ")
            {
                let line = lines[index];
                if line == r"\ No newline at end of file" {
                    index += 1;
                    continue;
                }
                let (kind, value) = line.split_at(1);
                let value = format!("{}\n", value);
                match kind {
                    " " => hunk_lines.push(HunkLine::Context(value)),
                    "-" => hunk_lines.push(HunkLine::Remove(value)),
                    "+" => hunk_lines.push(HunkLine::Add(value)),
                    _ => return Err(PatchError::Malformed(format!("bad hunk line {line:?}"))),
                }
                index += 1;
            }
            hunks.push(Hunk {
                old_start,
                lines: hunk_lines,
            });
        }
        patches.push(FilePatch { target, hunks });
    }
    if patches.is_empty() {
        return Err(PatchError::Empty);
    }
    Ok(patches)
}

fn normalize_patch_path(value: &str) -> Result<String, PatchError> {
    let path = value.split_whitespace().next().unwrap_or(value);
    if path == "/dev/null" {
        return Err(PatchError::Malformed(
            "creating or deleting files is not supported yet".into(),
        ));
    }
    let path = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .trim_start_matches("./")
        .to_owned();
    if path.is_empty() || path.contains("..") {
        return Err(PatchError::Malformed(format!("unsafe patch path {path:?}")));
    }
    Ok(path)
}

fn parse_old_start(header: &str) -> Result<usize, PatchError> {
    let old = header
        .split_whitespace()
        .find(|part| part.starts_with('-'))
        .ok_or_else(|| PatchError::Malformed(format!("bad hunk header {header:?}")))?;
    old.trim_start_matches('-')
        .split(',')
        .next()
        .unwrap_or("1")
        .parse::<usize>()
        .map_err(|_| PatchError::Malformed(format!("bad hunk header {header:?}")))
}

fn find_target(entries: &[TarEntry], target: &str) -> Option<usize> {
    let mut candidates = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        candidates.insert(entry.path.as_str(), index);
    }
    if let Some(index) = candidates.get(target) {
        return Some(*index);
    }
    let package_target = format!("package/{target}");
    if let Some(index) = candidates.get(package_target.as_str()) {
        return Some(*index);
    }
    entries.iter().position(|entry| {
        entry
            .path
            .split_once('/')
            .is_some_and(|(_, rest)| rest == target)
    })
}

fn apply_file_patch(path: &str, original: &str, patch: &FilePatch) -> Result<String, PatchError> {
    let original_lines = split_lines(original);
    let mut output = Vec::new();
    let mut cursor = 0usize;
    for hunk in &patch.hunks {
        let hunk_start = hunk.old_start.saturating_sub(1);
        if hunk_start < cursor || hunk_start > original_lines.len() {
            return Err(PatchError::HunkMismatch {
                path: path.to_owned(),
                line: hunk.old_start,
            });
        }
        output.extend_from_slice(&original_lines[cursor..hunk_start]);
        cursor = hunk_start;
        for line in &hunk.lines {
            match line {
                HunkLine::Context(value) => {
                    expect_line(path, &original_lines, cursor, value)?;
                    output.push(value.clone());
                    cursor += 1;
                }
                HunkLine::Remove(value) => {
                    expect_line(path, &original_lines, cursor, value)?;
                    cursor += 1;
                }
                HunkLine::Add(value) => output.push(value.clone()),
            }
        }
    }
    output.extend_from_slice(&original_lines[cursor..]);
    Ok(output.concat())
}

fn split_lines(value: &str) -> Vec<String> {
    if value.is_empty() {
        return Vec::new();
    }
    value.split_inclusive('\n').map(str::to_owned).collect()
}

fn expect_line(
    path: &str,
    original: &[String],
    cursor: usize,
    expected: &str,
) -> Result<(), PatchError> {
    if original.get(cursor).is_some_and(|line| line == expected) {
        return Ok(());
    }
    Err(PatchError::HunkMismatch {
        path: path.to_owned(),
        line: cursor + 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn applies_unified_diff_to_package_tarball() {
        let tarball = test_tgz("old\n");
        let patched = apply_unified_patch_to_tgz(
            &tarball,
            "--- a/index.js\n+++ b/index.js\n@@ -1 +1 @@\n-old\n+new\n",
        )
        .unwrap();
        assert_ne!(patched, tarball);
        let entries = read_entries(&patched).unwrap();
        let index = entries
            .iter()
            .find(|entry| entry.path == "package/index.js")
            .unwrap();
        assert_eq!(index.data, b"new\n");
    }

    fn test_tgz(index: &str) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let enc = flate2::write::GzEncoder::new(&mut out, flate2::Compression::default());
            let mut tar = tar::Builder::new(enc);
            append(
                &mut tar,
                "package/package.json",
                br#"{"name":"p","version":"1.0.0"}"#,
            );
            append(&mut tar, "package/index.js", index.as_bytes());
            let enc = tar.into_inner().unwrap();
            enc.finish().unwrap();
        }
        out
    }

    fn append<W: Write>(tar: &mut tar::Builder<W>, path: &str, bytes: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        tar.append(&header, bytes).unwrap();
    }
}
