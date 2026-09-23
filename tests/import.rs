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
        .arg("import")
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

#[cfg(unix)]
#[test]
fn cli_import_accepts_renamed_npm_locks_and_preserves_diagnostics() {
    for content in [
        REAL_V3.to_owned(),
        real_v2_with_conflicting_legacy_dependencies(),
    ] {
        let mut value: serde_json::Value = serde_json::from_str(&content).unwrap();
        value["packages"]["node_modules/left-pad"]["os"] = serde_json::json!(["linux"]);
        let content = serde_json::to_vec(&value).unwrap();
        for absolute in [false, true] {
            for json in [false, true] {
                let project = tempdir().unwrap();
                let input_dir = project.path().join("input");
                fs::create_dir(&input_dir).unwrap();
                let input = input_dir.join("yarn.lock");
                fs::write(&input, &content).unwrap();
                fs::write(
                    input_dir.join("package.json"),
                    r#"{"devDependencies":{"left-pad":"^1.3.0"}}"#,
                )
                .unwrap();
                let output = if json {
                    project.path().join("converted.lock")
                } else {
                    input_dir.join("bpm.lock")
                };
                let mut command = Command::new(env!("CARGO_BIN_EXE_bpm"));
                command
                    .arg("import")
                    .arg(if absolute {
                        input.as_path()
                    } else {
                        std::path::Path::new("input/yarn.lock")
                    })
                    .current_dir(project.path());
                if json {
                    command.arg("--json").arg("--out").arg(&output);
                }
                let result = command.output().unwrap();
                assert!(
                    result.status.success(),
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
                let imported = Lockfile::from_path(&output).unwrap();
                assert_eq!(imported.packages.len(), 2);
                assert_eq!(
                    imported.resolution.root.dev_dependencies["left-pad"],
                    "^1.3.0"
                );
                assert_eq!(
                    imported
                        .packages
                        .iter()
                        .find(|p| p.name == "left-pad")
                        .unwrap()
                        .version,
                    "1.3.0"
                );
                assert_eq!(fs::read(&input).unwrap(), content);
                if json {
                    let payload: serde_json::Value =
                        serde_json::from_slice(&result.stdout).unwrap();
                    assert_eq!(payload["package_count"], 2);
                    assert_eq!(payload["wrote"], output.to_str().unwrap());
                    assert_eq!(payload["diagnostics"][0]["code"], "PLATFORM_CONSTRAINT");
                    assert_eq!(
                        payload["lockfile"],
                        serde_json::to_value(&imported).unwrap()
                    );
                    assert!(!input_dir.join("bpm.lock").exists());
                } else {
                    assert!(String::from_utf8_lossy(&result.stdout)
                        .contains("imported 2 packages into "));
                    assert!(String::from_utf8_lossy(&result.stderr)
                        .contains("info[PLATFORM_CONSTRAINT]"));
                }
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn alternate_lock_inputs_fail_without_output() {
    let cases = [
        ("yarn.lock", "left-pad@^1.3.0:\n  version \"1.3.0\"\n  resolved \"https://registry/left-pad.tgz\"\n\nrepeat-string@^1.0.0:\n  version \"1.6.1\"\n  resolved \"https://registry/repeat-string.tgz\"\n", "failed to parse package-lock.json"),
        ("pnpm-lock.yaml", "lockfileVersion: '9.0'\npackages:\n  /foo@1.0.0:\n    resolution: {tarball: https://registry/foo.tgz}\n", "failed to parse package-lock.json"),
        ("bun.lock", r#"{"packages":{"foo@1.0.0":{"resolved":"https://registry/foo.tgz"}}}"#, "unsupported lockfileVersion 0"),
        ("unknown.lock", "not a lockfile", "failed to parse package-lock.json"),
        ("malformed.json", "{", "failed to parse package-lock.json"),
        ("missing-version.json", r#"{"packages":{}}"#, "unsupported lockfileVersion 0"),
        ("package-lock.json", r#"{"lockfileVersion":1,"packages":{}}"#, "unsupported lockfileVersion 1"),
        ("future.json", r#"{"lockfileVersion":4,"packages":{}}"#, "unsupported lockfileVersion 4"),
    ];
    for (name, content, reason) in cases {
        for explicit_out in [false, true] {
            for existing_out in [false, true] {
                let project = tempdir().unwrap();
                let input = project.path().join(name);
                fs::write(&input, content).unwrap();
                let outputs = [
                    project.path().join("bpm.lock"),
                    project.path().join("converted.lock"),
                ];
                let original = existing_out.then_some(b"preserve existing output".as_slice());
                if let Some(bytes) = original {
                    for output in &outputs {
                        fs::write(output, bytes).unwrap();
                    }
                }
                let mut command = Command::new(env!("CARGO_BIN_EXE_bpm"));
                command.args(["import", name]).current_dir(project.path());
                if explicit_out {
                    command.arg("--out").arg(&outputs[1]);
                }
                let result = command.output().unwrap();
                let stderr = String::from_utf8_lossy(&result.stderr);
                assert!(
                    !result.status.success(),
                    "{name} unexpectedly imported: {stderr}"
                );
                assert!(
                    stderr.contains("bpm import accepts only npm package-lock.json v2/v3"),
                    "{name}: {stderr}"
                );
                assert!(stderr.contains(reason), "{name}: {stderr}");
                assert!(result.stdout.is_empty());
                assert_eq!(fs::read(input).unwrap(), content.as_bytes());
                for output in outputs {
                    assert_eq!(
                        fs::read(&output).ok().as_deref(),
                        original,
                        "{name}: {}",
                        output.display()
                    );
                }
            }
        }
    }
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

#[test]
fn import_derives_urls_for_npm11_lockfiles_and_repairs_aliases() {
    // npm 11+ omits `resolved` for standard-registry packages, records
    // bundled deps without standalone artifacts, and alias entries can carry
    // a non-existent tarball URL under the alias path. The importer derives
    // repairs all three instead of marking the packages unresolvable.
    let json = r#"{
            "name": "app",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {"name": "app", "version": "1.0.0"},
                "node_modules/pkg": {"version": "1.2.3"},
                "node_modules/@scope/other": {"version": "2.0.0", "resolved": "https://registry.npmjs.org/@scope/other/node_modules/@scope/other/-/other-2.0.0.tgz", "integrity": "sha512-AAAA"},
                "node_modules/alias": {"name": "real-pkg", "version": "3.1.0", "resolved": "https://registry.npmjs.org/alias/-/alias-3.1.0.tgz", "integrity": "sha512-BBBB"},
                "node_modules/bundled-parent/node_modules/bundled-dep": {"version": "0.5.0", "inBundle": true}
            }
        }"#;

    let report = import(json).expect("import should succeed");
    let lockfile = &report.lockfile;

    let entries: Vec<_> = lockfile
        .packages
        .iter()
        .map(|p| (p.path.clone(), p.resolved.clone()))
        .collect();

    let find = |path: &str| {
        entries
            .iter()
            .find(|(p, _)| p.as_str() == path)
            .unwrap_or_else(|| panic!("missing entry for {path}"))
            .1
            .clone()
    };

    // npm-11 entry without `resolved` derives the standard tarball URL.
    assert_eq!(
        find("node_modules/pkg"),
        "https://registry.npmjs.org/pkg/-/pkg-1.2.3.tgz"
    );
    // Nested bundled-style URL is flattened.
    assert_eq!(
        find("node_modules/@scope/other"),
        "https://registry.npmjs.org/@scope/other/-/other-2.0.0.tgz"
    );
    // Alias entry points at the real package's tarball.
    assert_eq!(
        find("node_modules/alias"),
        "https://registry.npmjs.org/real-pkg/-/real-pkg-3.1.0.tgz"
    );
    // Bundled dependencies are not installable packages.
    assert!(
        !lockfile
            .packages
            .iter()
            .any(|p| p.path.ends_with("bundled-dep")),
        "bundled deps must be skipped"
    );
}
