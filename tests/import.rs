//! End-to-end import tests: npm `package-lock.json` v3 -> canonical `bpm.lock`,
//! including determinism and a real-fixture import.

use std::collections::BTreeMap;

use bpm::lockfile::Lockfile;
use bpm::npm_lock::{import, package_name_from_path, NpmLockError};

const REAL_V3: &str = r#"{
  "name": "app",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "app",
      "version": "1.0.0",
      "dependencies": { "left-pad": "^1.3.0", "@scope/bar": "^1.0.0" }
    },
    "node_modules/left-pad": {
      "version": "1.3.0",
      "resolved": "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz",
      "integrity": "sha512-AAAA",
      "bin": { "left-pad": "./bin.js" }
    },
    "node_modules/@scope/bar": {
      "version": "1.0.0",
      "resolved": "https://registry.npmjs.org/@scope/bar/-/bar-1.0.0.tgz",
      "integrity": "sha512-BBBB",
      "dependencies": { "left-pad": "^1.0.0" }
    }
  }
}"#;

#[test]
fn import_writes_canonical_lockfile_and_roundtrips() {
    let report = import(REAL_V3).unwrap();
    let json = report.lockfile.to_json().unwrap();
    let back = Lockfile::from_json(&json).unwrap();
    assert_eq!(report.lockfile, back, "bpm.lock must roundtrip");

    // Root carries the project name from the top-level field.
    assert_eq!(report.lockfile.root.name.as_deref(), Some("app"));
    assert_eq!(report.lockfile.root.version.as_deref(), Some("1.0.0"));
    assert_eq!(
        report
            .lockfile
            .root
            .dependencies
            .get("@scope/bar")
            .map(|s| s.as_str()),
        Some("^1.0.0")
    );

    // Two registry packages, sorted by path.
    let paths: Vec<&str> = report
        .lockfile
        .packages
        .iter()
        .map(|p| p.path.as_str())
        .collect();
    assert_eq!(
        paths,
        vec!["node_modules/@scope/bar", "node_modules/left-pad"]
    );

    let lp = report
        .lockfile
        .packages
        .iter()
        .find(|p| p.name == "left-pad")
        .unwrap();
    assert_eq!(lp.version, "1.3.0");
    assert_eq!(lp.bin.get("left-pad").map(|s| s.as_str()), Some("./bin.js"));
}

const REAL_V3_REVERSED_KEYS: &str = r#"{
  "name": "app",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "node_modules/@scope/bar": {
      "version": "1.0.0",
      "resolved": "https://registry.npmjs.org/@scope/bar/-/bar-1.0.0.tgz",
      "integrity": "sha512-BBBB",
      "dependencies": { "left-pad": "^1.0.0" }
    },
    "node_modules/left-pad": {
      "version": "1.3.0",
      "resolved": "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz",
      "integrity": "sha512-AAAA",
      "bin": { "left-pad": "./bin.js" }
    },
    "": {
      "name": "app",
      "version": "1.0.0",
      "dependencies": { "left-pad": "^1.3.0", "@scope/bar": "^1.0.0" }
    }
  }
}"#;

#[test]
fn lockfile_output_is_byte_stable_across_runs() {
    // Parse twice from the same input; the serialized bpm.lock must be
    // byte-identical (determinism regression test, §2).
    let a = import(REAL_V3).unwrap().lockfile.to_json().unwrap();
    let b = import(REAL_V3).unwrap().lockfile.to_json().unwrap();
    assert_eq!(a, b);

    // And independent of the *insertion order* of the JSON object keys.
    let c = import(REAL_V3_REVERSED_KEYS)
        .unwrap()
        .lockfile
        .to_json()
        .unwrap();
    assert_eq!(a, c, "input key order leaked into bpm.lock");
}

#[test]
fn unsupported_version_is_a_clear_error() {
    let v1 = REAL_V3.replace("\"lockfileVersion\": 3", "\"lockfileVersion\": 1");
    let err = import(&v1).unwrap_err();
    assert!(
        matches!(err, NpmLockError::UnsupportedVersion(1)),
        "{err:?}"
    );
}

#[test]
fn missing_packages_table_is_a_clear_error() {
    let err = import(r#"{ "lockfileVersion": 3 }"#).unwrap_err();
    assert!(matches!(err, NpmLockError::NoPackages), "{err:?}");
}

#[test]
fn reports_link_and_platform_constructs_with_codes() {
    let report = import(
        r#"{
          "lockfileVersion": 3,
          "packages": {
            "": { "version": "1.0.0" },
            "node_modules/native": {
              "version": "1.0.0",
              "resolved": "https://example/native.tgz",
              "integrity": "sha512-N",
              "os": ["linux"], "cpu": ["x64"]
            },
            "apps/widget": { "version": "1.0.0", "link": true }
          }
        }"#,
    )
    .unwrap();
    let codes: Vec<&str> = report.diagnostics.iter().map(|d| d.code).collect();
    assert!(codes.contains(&"PLATFORM_CONSTRAINT"));
    assert!(codes.contains(&"LINK_PACKAGE_UNSUPPORTED"));
}

#[test]
fn nested_package_name_resolution() {
    // Nested node_modules copies resolve to the inner name.
    assert_eq!(package_name_from_path("node_modules/a/node_modules/b"), "b");
    assert_eq!(package_name_from_path("node_modules/@scope/x"), "@scope/x");
}

#[test]
fn bin_as_string_uses_package_name() {
    let report = import(
        r#"{
          "lockfileVersion": 3,
          "packages": {
            "": { "version": "1.0.0" },
            "node_modules/onebin": {
              "version": "1.0.0",
              "resolved": "https://example/onebin.tgz",
              "integrity": "sha512-Z",
              "bin": "./cli.js"
            }
          }
        }"#,
    )
    .unwrap();
    let p = report
        .lockfile
        .packages
        .iter()
        .find(|p| p.name == "onebin")
        .unwrap();
    let bin: BTreeMap<&str, &str> = p
        .bin
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(bin.get("onebin").copied(), Some("./cli.js"));
}
