//! Deterministic, seekable package-image representation.
//!
//! The extracted directory remains the compatibility view used by the current
//! materializer. This compact sidecar is the immutable image format: it stores
//! a sorted index followed by file payloads and symlink targets, allowing tools
//! that do not need a directory walk to inspect/package contents directly.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"BPMIMG01";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    File { path: String, bytes: Vec<u8> },
    Symlink { path: String, target: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("image io error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid package image: {0}")]
    Invalid(String),
}

pub fn encode(entries: &[Entry]) -> Result<Vec<u8>, ImageError> {
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| path(a).cmp(path(b)));
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    write_u32(&mut out, sorted.len() as u32);
    for entry in sorted {
        match entry {
            Entry::File { path, bytes } => {
                out.push(0);
                write_str(&mut out, &path);
                write_bytes(&mut out, &bytes);
            }
            Entry::Symlink { path, target } => {
                out.push(1);
                write_str(&mut out, &path);
                write_str(&mut out, &target);
            }
        }
    }
    Ok(out)
}

pub fn decode(mut bytes: &[u8]) -> Result<Vec<Entry>, ImageError> {
    if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
        return Err(ImageError::Invalid("bad magic".into()));
    }
    bytes = &bytes[MAGIC.len()..];
    let count = take_u32(&mut bytes)? as usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let kind = take(&mut bytes, 1)?[0];
        let path = take_str(&mut bytes)?;
        let entry = match kind {
            0 => Entry::File {
                path,
                bytes: take_bytes(&mut bytes)?,
            },
            1 => Entry::Symlink {
                path,
                target: take_str(&mut bytes)?,
            },
            _ => return Err(ImageError::Invalid("unknown entry kind".into())),
        };
        entries.push(entry);
    }
    if !bytes.is_empty() {
        return Err(ImageError::Invalid("trailing bytes".into()));
    }
    Ok(entries)
}

pub fn from_directory(root: &Path) -> Result<Vec<u8>, ImageError> {
    let mut entries = Vec::new();
    collect(root, root, &mut entries)?;
    encode(&entries)
}

pub fn to_directory(bytes: &[u8], root: &Path) -> Result<(), ImageError> {
    fs::create_dir_all(root)?;
    for entry in decode(bytes)? {
        match entry {
            Entry::File { path, bytes } => {
                let dest = safe_join(root, &path)?;
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(dest, bytes)?;
            }
            Entry::Symlink { path, target } => {
                let dest = safe_join(root, &path)?;
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                create_symlink(&target, &dest)?;
            }
        }
    }
    Ok(())
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<Entry>) -> Result<(), ImageError> {
    for item in fs::read_dir(dir)? {
        let item = item?;
        let p = item.path();
        let rel = p
            .strip_prefix(root)
            .map_err(|_| ImageError::Invalid("entry outside image".into()))?
            .to_string_lossy()
            .replace('\\', "/");
        let ty = item.file_type()?;
        if ty.is_dir() {
            collect(root, &p, out)?;
        } else if ty.is_file() {
            out.push(Entry::File {
                path: rel,
                bytes: fs::read(p)?,
            });
        } else if ty.is_symlink() {
            out.push(Entry::Symlink {
                path: rel,
                target: fs::read_link(p)?.to_string_lossy().into_owned(),
            });
        }
    }
    Ok(())
}
fn path(entry: &Entry) -> &str {
    match entry {
        Entry::File { path, .. } | Entry::Symlink { path, .. } => path,
    }
}
fn safe_join(root: &Path, path: &str) -> Result<PathBuf, ImageError> {
    let p = Path::new(path);
    if p.is_absolute()
        || p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ImageError::Invalid(format!("unsafe path {path}")));
    }
    Ok(root.join(p))
}
fn write_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn write_str(out: &mut Vec<u8>, v: &str) {
    write_bytes(out, v.as_bytes());
}
fn write_bytes(out: &mut Vec<u8>, v: &[u8]) {
    write_u32(out, v.len() as u32);
    out.extend_from_slice(v);
}
fn take<'a>(input: &mut &'a [u8], n: usize) -> Result<&'a [u8], ImageError> {
    if input.len() < n {
        return Err(ImageError::Invalid("truncated image".into()));
    }
    let (a, b) = input.split_at(n);
    *input = b;
    Ok(a)
}
fn take_u32(input: &mut &[u8]) -> Result<u32, ImageError> {
    Ok(u32::from_le_bytes(take(input, 4)?.try_into().unwrap()))
}
fn take_bytes(input: &mut &[u8]) -> Result<Vec<u8>, ImageError> {
    let n = take_u32(input)? as usize;
    Ok(take(input, n)?.to_vec())
}
fn take_str(input: &mut &[u8]) -> Result<String, ImageError> {
    String::from_utf8(take_bytes(input)?).map_err(|_| ImageError::Invalid("invalid utf-8".into()))
}
#[cfg(unix)]
fn create_symlink(target: &str, path: &Path) -> Result<(), io::Error> {
    std::os::unix::fs::symlink(target, path)
}
#[cfg(windows)]
fn create_symlink(target: &str, path: &Path) -> Result<(), io::Error> {
    std::os::windows::fs::symlink_file(target, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_is_sorted() {
        let bytes = encode(&[
            Entry::File {
                path: "z".into(),
                bytes: b"z".to_vec(),
            },
            Entry::Symlink {
                path: "a".into(),
                target: "../x".into(),
            },
        ])
        .unwrap();
        let entries = decode(&bytes).unwrap();
        assert_eq!(path(&entries[0]), "a");
    }
    #[test]
    fn rejects_traversal() {
        let bytes = encode(&[Entry::File {
            path: "../x".into(),
            bytes: vec![],
        }])
        .unwrap();
        assert!(to_directory(&bytes, Path::new("/tmp/image-test")).is_err());
    }
}
