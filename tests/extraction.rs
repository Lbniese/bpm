//! Archive extraction security tests (AGENTS "Archive extraction changes").
//!
//! Each test builds a hostile or edge-case gzip tar in memory, extracts it, and
//! asserts the archive layer rejects unsafe constructs and handles legal ones.

mod common;

use std::fs;
use std::path::Path;

use bpm::archive::extract;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

use common::{add_dir, add_file, add_symlink, build_raw_tgz, build_tgz, RawEntry};

const PROPERTY_CASES: u32 = 32;

fn property_runner() -> TestRunner {
    TestRunner::new(Config {
        cases: PROPERTY_CASES,
        failure_persistence: None,
        ..Config::default()
    })
}

fn raw_file_entry<'a>(name: &'a [u8], data: &'a [u8]) -> RawEntry<'a> {
    RawEntry {
        name,
        typeflag: b'0',
        linkname: b"",
        mode: 0o644,
        data,
    }
}

fn raw_symlink_entry<'a>(name: &'a [u8], linkname: &'a [u8]) -> RawEntry<'a> {
    RawEntry {
        name,
        typeflag: b'2',
        linkname,
        mode: 0o777,
        data: b"",
    }
}

fn gzip_bytes(bytes: &[u8]) -> Vec<u8> {
    use std::io::Read;

    let mut enc = flate2::read::GzEncoder::new(bytes, flate2::Compression::default());
    let mut out = Vec::new();
    enc.read_to_end(&mut out).expect("gzip encode");
    out
}

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

#[cfg(unix)]
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

#[cfg(unix)]
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
fn property_rejects_bounded_unsafe_paths_without_escape_writes() {
    let unsafe_paths = prop::sample::select(vec![
        b"package/../evil.js".to_vec(),
        b"package/sub/../../evil.js".to_vec(),
        b"package/sub/../../../tmp/evil.js".to_vec(),
        b"../evil.js".to_vec(),
        b"/tmp/evil.js".to_vec(),
        b"package//safe/../../evil.js".to_vec(),
        b"package/./safe/../../../evil.js".to_vec(),
    ]);
    let payloads = prop::collection::vec(any::<u8>(), 0..64);

    property_runner()
        .run(&(unsafe_paths, payloads), |(path, payload)| {
            let entry = raw_file_entry(&path, &payload);
            let tgz = build_raw_tgz(&[entry]);
            let image = tempfile::tempdir().unwrap();
            let err = extract_tgz(&tgz, image.path()).expect_err("unsafe path must reject");
            let msg = err.to_string();

            prop_assert!(
                msg.contains("unsafe entry path") || msg.contains("absolute"),
                "unexpected error for {path:?}: {msg}"
            );
            prop_assert!(!image.path().join("evil.js").exists());
            prop_assert!(!image.path().join("tmp/evil.js").exists());
            Ok(())
        })
        .unwrap();
}

#[test]
fn property_rejects_bounded_unsafe_symlink_targets() {
    let cases = prop::sample::select(vec![
        ("package/link", "../outside.js"),
        ("package/a/link", "../../outside.js"),
        ("package/a/b/link", "../../../outside.js"),
        ("package/link", "/tmp/outside.js"),
        ("package/a/link", "../b/../../outside.js"),
    ]);

    property_runner()
        .run(&cases, |(link, target)| {
            let tgz = build_tgz(|b| add_symlink(b, link, target));
            let msg = must_reject(&tgz);
            prop_assert!(msg.contains("unsafe symlink"), "got: {msg}");
            Ok(())
        })
        .unwrap();
}

#[test]
fn property_rejects_bounded_malformed_gzip_or_tar_streams() {
    let malformed_payloads = prop::collection::vec(any::<u8>(), 1..128).prop_map(|bytes| {
        if bytes.len() % 2 == 0 {
            bytes
        } else {
            gzip_bytes(&bytes)
        }
    });

    property_runner()
        .run(&malformed_payloads, |tgz| {
            let msg = must_reject(&tgz);
            prop_assert!(
                msg.contains("valid gzip") || msg.contains("valid gzip/tar"),
                "got: {msg}"
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn persisted_regressions_reject_prefix_escape_symlink_and_type_edges() {
    let deep_escape = build_raw_tgz(&[raw_file_entry(
        b"package/nested/dir/../../../escape.js",
        b"escape",
    )]);
    assert!(must_reject(&deep_escape).contains("unsafe entry path"));

    let absolute_symlink_target = build_raw_tgz(&[raw_symlink_entry(
        b"package/absolute-link",
        b"/tmp/escape.js",
    )]);
    assert!(must_reject(&absolute_symlink_target).contains("unsafe symlink"));

    let duplicate_dir_file = build_tgz(|b| {
        add_dir(b, "package/collide", 0o755);
        add_file(b, "package/collide", 0o644, b"file");
    });
    assert!(must_reject(&duplicate_dir_file).contains("duplicate"));
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
