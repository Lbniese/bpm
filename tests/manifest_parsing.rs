//! Integration tests for `package.json` parsing.
//!
//! These exercise [`bpm::manifest::PackageManifest`] through the public API
//! (round-trip from a real file on disk), complementing the unit tests that
//! parse in-memory strings.

use std::fs;
use std::path::Path;

use bpm::manifest::{ManifestError, PackageManifest};
use tempfile::tempdir;

fn write(dir: &Path, json: &str) -> PathBuf {
    let p = dir.join("package.json");
    fs::write(&p, json).unwrap();
    p
}

use std::path::PathBuf;

#[test]
fn reads_real_manifest_file() {
    let dir = tempdir().unwrap();
    let p = write(
        dir.path(),
        r#"{"name":"@scope/app","version":"3.1.4","private":true,"type":"module",
        "dependencies":{"react":"^18.0.0"},
        "devDependencies":{"jest":"^29.0.0"},
        "scripts":{"build":"vite build","test":"jest"},
        "bin":{"app":"./bin/app.js"},
        "engines":{"node":">=18"}}"#,
    );
    let m = PackageManifest::from_path(&p).unwrap();
    assert_eq!(m.name.as_deref(), Some("@scope/app"));
    assert_eq!(m.version.as_deref(), Some("3.1.4"));
    assert_eq!(m.private, Some(true));
    assert_eq!(m.module_type.as_deref(), Some("module"));
    assert_eq!(m.dependency_count(), 2);
    assert_eq!(m.bin_count(), 1);
    assert_eq!(m.scripts.len(), 2);
    assert_eq!(m.engines.get("node").unwrap(), ">=18");
}

#[test]
fn missing_file_is_read_error() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("package.json");
    let err = PackageManifest::from_path(&missing).expect_err("missing file");
    assert!(matches!(err, ManifestError::Read { .. }));
}

#[test]
fn malformed_file_is_parse_error() {
    let dir = tempdir().unwrap();
    let p = write(dir.path(), "{ \"name\": ");
    let err = PackageManifest::from_path(&p).expect_err("bad json");
    assert!(matches!(err, ManifestError::Parse { .. }));
}

#[test]
fn dependency_keys_are_stably_ordered() {
    let dir = tempdir().unwrap();
    let p = write(
        dir.path(),
        r#"{"name":"a","dependencies":{"zoo":"^1.0.0","apple":"^2.0.0","mango":"^3.0.0"}}"#,
    );
    let m = PackageManifest::from_path(&p).unwrap();
    let keys: Vec<&str> = m.dependencies.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["apple", "mango", "zoo"]);
}
