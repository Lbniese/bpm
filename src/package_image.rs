//! Deterministic, seekable package-image representation.
//!
//! The extracted directory remains the compatibility view used by the current
//! materializer. This compact sidecar is the immutable image format: it stores
//! a sorted index followed by file payloads and symlink targets, allowing tools
//! that do not need a directory walk to inspect/package contents directly.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"BPMIMG01";

/// Maximum number of entries accepted by [`decode_index`]. Generous relative
/// to any real package (npm packages have at most tens of thousands of files)
/// but bounded so a corrupt `u32` count cannot drive unbounded work.
const MAX_INDEX_ENTRIES: usize = 4_000_000;
/// Maximum byte length of a single path or symlink target accepted by
/// [`decode_index`]. Well above any valid path, bounded so a corrupt length
/// cannot drive a huge allocation.
const MAX_INDEX_STRING: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    File { path: String, bytes: Vec<u8> },
    Symlink { path: String, target: String },
}

/// Metadata-only view of a package-image [`Entry`]: file entries omit their
/// payload bytes (only the path is kept); symlink entries keep path + target.
/// Use [`decode_index`] to parse this view from a seekable source without
/// reading file payloads into memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexEntry {
    File { path: String },
    Symlink { path: String, target: String },
}

impl IndexEntry {
    pub fn path(&self) -> &str {
        match self {
            IndexEntry::File { path } | IndexEntry::Symlink { path, .. } => path,
        }
    }
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

/// Decode only the metadata (paths and symlink targets) of a [`MAGIC`]-prefixed
/// package image from a `Read + Seek` source, **skipping file payloads with
/// `Seek`** rather than reading them into memory.
///
/// Hardlink/reflink materialization needs only paths and symlink targets — the
/// file bytes come from the extracted immutable image — so this avoids reading
/// and copying every payload. The format is fully validated: magic and entry
/// count, every kind/path/target, payload extents must lie within the recorded
/// stream end (a seek past EOF alone is not accepted), and trailing bytes are
/// rejected. Use [`decode`] when the actual file payloads are needed.
pub fn decode_index<R: Read + Seek>(reader: &mut R) -> Result<Vec<IndexEntry>, ImageError> {
    // Determine the stream extent without assuming the start is at offset zero.
    let start = reader.stream_position()?;
    let end = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(start))?;

    let mut magic = [0u8; MAGIC.len()];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(ImageError::Invalid("bad magic".into()));
    }
    let count = read_u32_reader(reader)? as usize;
    if count > MAX_INDEX_ENTRIES {
        return Err(ImageError::Invalid(format!(
            "entry count {count} exceeds maximum {MAX_INDEX_ENTRIES}"
        )));
    }
    // Each entry needs at least one kind byte, a path length, and one further
    // length field (file payload length or symlink target length) — 9 bytes.
    // Reject a corrupt count up front rather than discovering truncation deep in
    // the loop; payload bytes are counted in `remaining` so this never rejects a
    // valid image.
    let header_end = reader.stream_position()?;
    let remaining = end
        .checked_sub(header_end)
        .ok_or_else(|| ImageError::Invalid("stream end precedes header end".into()))?;
    let min_body = (count as u64).saturating_mul(9);
    if min_body > remaining {
        return Err(ImageError::Invalid(format!(
            "entry count {count} implies {min_body} bytes but only {remaining} remain"
        )));
    }

    let mut entries = Vec::new();
    for _ in 0..count {
        let kind = read_u8_reader(reader)?;
        let path = read_bounded_string(reader)?;
        let entry = match kind {
            0 => {
                let payload_len = read_u32_reader(reader)? as u64;
                let pos = reader.stream_position()?;
                let after_payload = pos.checked_add(payload_len).ok_or_else(|| {
                    ImageError::Invalid("file payload length overflows stream position".into())
                })?;
                // Seeking past EOF may succeed on a regular file, so validate
                // the extent against the recorded stream end explicitly.
                if after_payload > end {
                    return Err(ImageError::Invalid(
                        "file payload extent ends beyond stream end".into(),
                    ));
                }
                reader.seek(SeekFrom::Start(after_payload))?;
                IndexEntry::File { path }
            }
            1 => {
                let target = read_bounded_string(reader)?;
                IndexEntry::Symlink { path, target }
            }
            _ => return Err(ImageError::Invalid("unknown entry kind".into())),
        };
        entries.push(entry);
    }

    let pos = reader.stream_position()?;
    if pos != end {
        return Err(ImageError::Invalid("trailing bytes".into()));
    }
    Ok(entries)
}

fn read_u32_reader<R: Read>(reader: &mut R) -> Result<u32, ImageError> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u8_reader<R: Read>(reader: &mut R) -> Result<u8, ImageError> {
    let mut buf = [0u8; 1];
    reader.read_exact(&mut buf)?;
    Ok(buf[0])
}

fn read_bounded_string<R: Read>(reader: &mut R) -> Result<String, ImageError> {
    let len = read_u32_reader(reader)? as usize;
    if len > MAX_INDEX_STRING {
        return Err(ImageError::Invalid(format!(
            "string length {len} exceeds maximum {MAX_INDEX_STRING}"
        )));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|_| ImageError::Invalid("invalid utf-8".into()))
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

    // === Plan 014: metadata-only seekable decoder ===

    #[test]
    fn decode_index_roundtrips_mixed_entries() {
        let entries = vec![
            Entry::File {
                path: "a/b".into(),
                bytes: b"hello".to_vec(),
            },
            Entry::Symlink {
                path: "a/l".into(),
                target: "../b".into(),
            },
            Entry::File {
                path: "z".into(),
                bytes: vec![],
            },
        ];
        let bytes = encode(&entries).unwrap();
        let mut cursor = io::Cursor::new(&bytes);
        let index = decode_index(&mut cursor).expect("valid image must decode");
        // encode() sorts by path, so the order is a/b, a/l, z.
        assert_eq!(index.len(), 3);
        assert_eq!(index[0].path(), "a/b");
        assert_eq!(index[1].path(), "a/l");
        assert_eq!(index[2].path(), "z");
        match &index[1] {
            IndexEntry::Symlink { target, .. } => assert_eq!(target, "../b"),
            other => panic!("expected symlink, got {other:?}"),
        }
        assert!(matches!(index[0], IndexEntry::File { .. }));
    }

    /// A `Read + Seek` wrapper that counts bytes returned by `read` (not seeks),
    /// proving `decode_index` skips file payloads rather than copying them.
    struct CountingReader<R> {
        inner: R,
        read_bytes: u64,
    }
    impl<R: Read> Read for CountingReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read_bytes += n as u64;
            Ok(n)
        }
    }
    impl<R: Seek> Seek for CountingReader<R> {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    #[test]
    fn decode_index_skips_payload_bytes() {
        // A payload far larger than the metadata, so a payload-copying decoder
        // would read ~100 KiB while a seeking decoder reads only metadata.
        let big = vec![42u8; 100_000];
        let entries = vec![
            Entry::File {
                path: "big.bin".into(),
                bytes: big.clone(),
            },
            Entry::Symlink {
                path: "link".into(),
                target: "../big.bin".into(),
            },
        ];
        let bytes = encode(&entries).unwrap();
        let metadata_budget = bytes.len() - big.len();

        let mut reader = CountingReader {
            inner: io::Cursor::new(&bytes),
            read_bytes: 0,
        };
        let index = decode_index(&mut reader).expect("valid image must decode");
        assert_eq!(index.len(), 2);
        assert_eq!(index[0].path(), "big.bin");
        // The decoder reads every metadata byte but seeks over the payload, so
        // the read volume is bounded by the metadata size and does NOT scale
        // with the payload length.
        assert!(
            reader.read_bytes <= metadata_budget as u64,
            "metadata decoder read {} bytes; metadata budget is {}; it must skip the {}-byte payload via Seek",
            reader.read_bytes,
            metadata_budget,
            big.len()
        );
        assert!(
            reader.read_bytes < big.len() as u64 / 10,
            "read volume ({}) must be far below the payload length ({})",
            reader.read_bytes,
            big.len()
        );
    }

    #[test]
    fn decode_index_rejects_bad_magic() {
        let mut cursor = io::Cursor::new(b"NOPE1234extra");
        assert!(decode_index(&mut cursor).is_err());
    }

    #[test]
    fn decode_index_rejects_payload_extent_past_end() {
        // One file entry whose declared payload length (9999) exceeds the bytes
        // actually present — seeking past EOF must not be accepted as valid.
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&1u32.to_le_bytes()); // 1 entry
        bad.push(0); // file
        bad.extend_from_slice(&1u32.to_le_bytes()); // path len
        bad.extend_from_slice(b"a"); // path
        bad.extend_from_slice(&9999u32.to_le_bytes()); // payload len beyond EOF
        bad.extend_from_slice(b"o"); // only 1 payload byte
        let mut cursor = io::Cursor::new(&bad);
        assert!(decode_index(&mut cursor).is_err());
    }

    #[test]
    fn decode_index_rejects_trailing_bytes() {
        let mut bytes = encode(&[Entry::File {
            path: "a".into(),
            bytes: b"a".to_vec(),
        }])
        .unwrap();
        bytes.push(0xff);
        let mut cursor = io::Cursor::new(&bytes);
        assert!(decode_index(&mut cursor).is_err());
    }

    #[test]
    fn decode_index_rejects_unknown_kind() {
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&1u32.to_le_bytes());
        bad.push(9); // unknown kind
        bad.extend_from_slice(&1u32.to_le_bytes());
        bad.extend_from_slice(b"a");
        let mut cursor = io::Cursor::new(&bad);
        assert!(decode_index(&mut cursor).is_err());
    }

    #[test]
    fn decode_index_rejects_excessive_entry_count() {
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&u32::MAX.to_le_bytes()); // absurd count
        let mut cursor = io::Cursor::new(&bad);
        assert!(decode_index(&mut cursor).is_err());
    }

    #[test]
    fn decode_index_rejects_excessive_string_length() {
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&1u32.to_le_bytes()); // 1 entry
        bad.push(0); // file
        bad.extend_from_slice(&u32::MAX.to_le_bytes()); // absurd path length
        let mut cursor = io::Cursor::new(&bad);
        assert!(decode_index(&mut cursor).is_err());
    }

    #[test]
    fn decode_index_rejects_truncated_header() {
        let mut cursor = io::Cursor::new(b"BPMIMG0"); // truncated magic
        assert!(decode_index(&mut cursor).is_err());
    }

    #[test]
    fn full_decode_behavior_is_unchanged() {
        let entries = vec![
            Entry::File {
                path: "f".into(),
                bytes: b"payload".to_vec(),
            },
            Entry::Symlink {
                path: "s".into(),
                target: "./f".into(),
            },
        ];
        let bytes = encode(&entries).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.len(), 2);
        match &decoded[0] {
            Entry::File { path, bytes } => {
                assert_eq!(path, "f");
                assert_eq!(bytes, b"payload");
            }
            other => panic!("expected file, got {other:?}"),
        }
    }
}
