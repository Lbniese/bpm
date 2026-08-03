//! End-to-end import tests: npm `package-lock.json` v2/v3 -> canonical
//! `bpm.lock`, including determinism and a real-fixture import.

use std::collections::BTreeMap;

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use tempfile::tempdir;

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

fn real_v2_with_conflicting_legacy_dependencies() -> String {
    let mut value: serde_json::Value = serde_json::from_str(REAL_V3).unwrap();
    value["lockfileVersion"] = serde_json::json!(2);
    value["dependencies"] = serde_json::json!({
        "left-pad": {
            "version": "9.9.9",
            "resolved": "https://example/legacy-left-pad-9.9.9.tgz",
            "integrity": "sha512-LEGACY"
        },
        "@scope/bar": {
            "version": "8.8.8"
        }
    });
    serde_json::to_string(&value).unwrap()
}

#[test]
fn v2_packages_table_is_authoritative_and_matches_v3_normalization() {
    let v2 = real_v2_with_conflicting_legacy_dependencies();
    let v2_report = import(&v2).unwrap();
    let v3_report = import(REAL_V3).unwrap();
    assert_eq!(v2_report.lockfile, v3_report.lockfile);
    let left_pad = v2_report
        .lockfile
        .packages
        .iter()
        .find(|package| package.name == "left-pad")
        .unwrap();
    assert_eq!(left_pad.version, "1.3.0");
    assert_eq!(left_pad.integrity.as_deref(), Some("sha512-AAAA"));
}

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
    let future = REAL_V3.replace("\"lockfileVersion\": 3", "\"lockfileVersion\": 4");
    let err = import(&future).unwrap_err();
    assert!(
        matches!(err, NpmLockError::UnsupportedVersion(4)),
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
fn skips_npm_workspace_metadata_entries_outside_node_modules() {
    let report = import(
        r#"{
          "lockfileVersion": 3,
          "packages": {
            "": { "name": "app", "workspaces": ["packages/*"] },
            "node_modules/@scope/shared": {
              "resolved": "packages/shared",
              "link": true
            },
            "packages/shared": {
              "name": "@scope/shared",
              "version": "1.0.0",
              "dependencies": { "left-pad": "^1.3.0" }
            },
            "node_modules/left-pad": {
              "version": "1.3.0",
              "resolved": "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz",
              "integrity": "sha512-AAAA"
            }
          }
        }"#,
    )
    .unwrap();

    let paths: Vec<&str> = report
        .lockfile
        .packages
        .iter()
        .map(|package| package.path.as_str())
        .collect();
    assert_eq!(
        paths,
        vec!["node_modules/@scope/shared", "node_modules/left-pad"]
    );
}

#[cfg(unix)]
#[test]
fn cli_import_metadata_roundtrips_through_ci() {
    let project = tempdir().unwrap();
    let store = tempdir().unwrap();
    fs::write(
        project.path().join("package.json"),
        r#"{"name":"app","devDependencies":{"tool":"1.0.0"},"overrides":{"transitive":"^2.0.0"}}"#,
    )
    .unwrap();
    fs::write(
        project.path().join("package-lock.json"),
        r#"{"name":"app","lockfileVersion":3,"packages":{"":{"name":"app","dependencies":{"tool":"1.0.0"},"devDependencies":{"tool":"1.0.0"}}}}"#,
    )
    .unwrap();

    let import = Command::new(env!("CARGO_BIN_EXE_bpm"))
        .args(["import", "package-lock.json"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        import.status.success(),
        "import failed: {}",
        String::from_utf8_lossy(&import.stderr)
    );

    let ci = Command::new(env!("CARGO_BIN_EXE_bpm"))
        .args(["ci", "--store"])
        .arg(store.path())
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        ci.status.success(),
        "ci rejected imported lock: {}",
        String::from_utf8_lossy(&ci.stderr)
    );
}

#[cfg(unix)]
#[test]
fn cli_import_accepts_v2_packages_table() {
    let project = tempdir().unwrap();
    let v2 = real_v2_with_conflicting_legacy_dependencies();
    fs::write(project.path().join("package-lock.json"), v2).unwrap();

    let import = Command::new(env!("CARGO_BIN_EXE_bpm"))
        .args(["import", "package-lock.json"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        import.status.success(),
        "v2 import failed: {}",
        String::from_utf8_lossy(&import.stderr)
    );
    let imported = Lockfile::from_path(&project.path().join("bpm.lock")).unwrap();
    assert!(imported
        .packages
        .iter()
        .any(|package| package.name == "left-pad"));
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
