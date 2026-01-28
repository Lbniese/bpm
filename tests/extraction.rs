//! Archive extraction security tests (AGENTS "Archive extraction changes").
//!
//! Each test builds a hostile or edge-case gzip tar in memory, extracts it, and
//! asserts the archive layer rejects unsafe constructs and handles legal ones.

mod common;

use std::fs;
use std::path::Path;

use bpm::archive::extract;

use common::{add_dir, add_file, add_symlink, build_raw_tgz, build_tgz, RawEntry};

/// Write `bytes` to a uniquely-named temp archive file and extract into
/// `image_root` (created here). A unique name is required because extraction
/// tests run in parallel and would otherwise overwrite each other's archive.
fn extract_tgz(tgz: &[u8], image_root: &Path) -> Result<(), bpm::archive::ExtractError> {
    fs::create_dir_all(image_root).ok();
    let archive = tempfile::NamedTempFile::new().unwrap();
    fs::write(archive.path(), tgz).unwrap();
    extract(archive.path(), image_root)
}

/// Extract and assert rejection, returning the error Display.
fn must_reject(tgz: &[u8]) -> String {
    let image = tempfile::tempdir().unwrap();
    match extract_tgz(tgz, image.path()) {
        Err(e) => format!("{e}"),
        Ok(()) => panic!("extraction should have failed"),
    }
}

#[test]
fn rejects_path_traversal() {
    let _image = tempfile::tempdir().unwrap();
    let tgz = build_raw_tgz(&[RawEntry {
        name: b"package/../../etc/evil",
        typeflag: b'0',
        linkname: b"",
        mode: 0o644,
        data: b"pwned",
    }]);
    let image_root = _image.path().to_path_buf();
    extract_tgz(&tgz, &image_root).expect_err("traversal must be rejected");
    // No partial write should have escaped outside the image root.
    assert!(!image_root.join("etc/evil").exists());
}

#[test]
fn rejects_absolute_path() {
    let tgz = build_raw_tgz(&[RawEntry {
        name: b"/etc/passwd",
        typeflag: b'0',
        linkname: b"",
        mode: 0o644,
        data: b"root",
    }]);
    let msg = must_reject(&tgz);
    assert!(msg.contains("absolute"), "got: {msg}");
}

#[test]
fn rejects_symlink_escaping_root() {
    let tgz = build_tgz(|b| {
        add_symlink(b, "package/link", "../../etc/evil");
    });
    let msg = must_reject(&tgz);
    assert!(msg.contains("unsafe symlink"), "got: {msg}");
}

#[test]
fn allows_symlink_staying_within_root() {
    let tmp = tempfile::tempdir().unwrap();
    let tgz = build_tgz(|b| {
        add_dir(b, "package/a", 0o755);
        add_dir(b, "package/b", 0o755);
        add_file(b, "package/b/target.js", 0o644, b"module.exports = 1;");
        add_symlink(b, "package/a/link", "../b/target.js");
    });
    extract_archive(&tgz, tmp.path()).unwrap();
    let link = tmp.path().join("a/link");
    assert!(link.is_symlink(), "symlink should exist");
    // The resolved target must stay inside the image.
    let target = fs::read_link(&link).unwrap();
    assert!(target == std::path::Path::new("../b/target.js"));
}

#[test]
fn preserves_executable_permissions() {
    let tmp = tempfile::tempdir().unwrap();
    let tgz = build_tgz(|b| {
        add_file(b, "package/bin/cli.js", 0o755, b"#!/usr/bin/env node\n");
    });
    extract_archive(&tgz, tmp.path()).unwrap();
    let mode = fs::metadata(tmp.path().join("bin/cli.js"))
        .unwrap()
        .permissions()
        .mode();
    assert!(mode & 0o111 != 0, "executable bit not preserved: {mode:o}");
    // World-write must be stripped.
    assert!(mode & 0o002 == 0, "world-write not stripped: {mode:o}");
}

#[test]
fn rejects_malformed_archive() {
    let tgz = b"this is definitely not gzip";
    let msg = must_reject(tgz);
    assert!(msg.contains("valid gzip"), "got: {msg}");
}

#[test]
fn rejects_duplicate_entries() {
    let tgz = build_tgz(|b| {
        add_file(b, "package/x.js", 0o644, b"first");
        add_file(b, "package/x.js", 0o644, b"second");
    });
    let msg = must_reject(&tgz);
    assert!(msg.contains("duplicate"), "got: {msg}");
}

#[test]
fn rejects_unsupported_entry_type() {
    // Hardlink is an unusual entry type for npm tarballs and is rejected.
    let mut header = tar::Header::new_gnu();
    header.set_path("package/h.js").unwrap();
    header.set_entry_type(tar::EntryType::hard_link());
    header.set_link_name("package/other.js").unwrap();
    header.set_size(0);
    header.set_mode(0o644);
    header.set_cksum();
    let tgz = build_tgz(|b| {
        b.append(&header, &[][..]).unwrap();
    });
    let msg = must_reject(&tgz);
    assert!(msg.contains("unsupported entry type"), "got: {msg}");
}

#[test]
fn strips_package_prefix_and_normalizes_root() {
    let tmp = tempfile::tempdir().unwrap();
    let tgz = build_tgz(|b| {
        add_dir(b, "package", 0o755);
        add_file(b, "package/package.json", 0o644, br#"{"name":"app"}"#);
        add_file(b, "package/sub/a.js", 0o644, b"module.exports = 1;");
    });
    extract_archive(&tgz, tmp.path()).unwrap();
    assert!(tmp.path().join("package.json").is_file());
    assert!(tmp.path().join("sub/a.js").is_file());
    assert!(!tmp.path().join("package/package.json").exists());
}

/// Extract a tgz into `image_root` via a uniquely-named temp archive.
fn extract_archive(tgz: &[u8], image_root: &Path) -> Result<(), bpm::archive::ExtractError> {
    extract_tgz(tgz, image_root)
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
