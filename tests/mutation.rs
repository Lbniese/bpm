//! End-to-end CLI tests for local dependency mutation: `bpm add` /
//! `bpm install <pkg>` and `bpm remove`. Fully offline: a local mock registry
//! serves packuments and tarballs. The npm interoperability case invokes the
//! real `npm ci` when npm is on PATH (skipped with a clear message otherwise).

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use common::{build_tgz, integrity_of, MiniServer, RouteBody};

fn bin() -> String {
    std::env::var("CARGO_BIN_EXE_bpm").expect("CARGO_BIN_EXE_bpm")
}

fn package_json(
    name: &str,
    version: &str,
    deps: &BTreeMap<String, String>,
    bin: Option<&str>,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("name".into(), serde_json::json!(name));
    obj.insert("version".into(), serde_json::json!(version));
    if !deps.is_empty() {
        obj.insert("dependencies".into(), serde_json::json!(deps));
    }
    if let Some(path) = bin {
        obj.insert("bin".into(), serde_json::json!(path));
    }
    serde_json::Value::Object(obj)
}

/// Build a registry tarball. `deps` becomes the package's `dependencies`, so
/// the resolver will fetch each declared transitive package too.
fn pkg_tarball(
    name: &str,
    version: &str,
    deps: &BTreeMap<String, String>,
    bin: Option<&str>,
) -> Vec<u8> {
    build_tgz(|builder| {
        common::add_file(
            builder,
            "package.json",
            0o644,
            serde_json::to_vec(&package_json(name, version, deps, bin))
                .unwrap()
                .as_slice(),
        );
        if let Some(path) = bin {
            common::add_file(
                builder,
                path.trim_start_matches("./"),
                0o755,
                b"#!/usr/bin/env node\nconsole.log('demo');\n",
            );
        }
    })
}

/// A local mock registry serving any number of packages. Each registered
/// package serves its packument at `/<name>` (scoped names URL-encoded) and
/// its tarball at `/tarballs/<name>-<version>.tgz`.
#[allow(dead_code)]
struct MockRegistry {
    server: MiniServer,
    base: String,
}

struct Package {
    name: String,
    version: String,
    deps: BTreeMap<String, String>,
    tarball: Vec<u8>,
}

impl MockRegistry {
    #[allow(clippy::type_complexity)]
    fn new(packages: Vec<Package>) -> Self {
        // Move the package table into the responder closure.
        let table: Arc<Vec<(String, String, BTreeMap<String, String>, Vec<u8>, String)>> = Arc::new(
            packages
                .iter()
                .map(|package| {
                    let integrity = integrity_of(&package.tarball);
                    (
                        package.name.clone(),
                        package.version.clone(),
                        package.deps.clone(),
                        package.tarball.clone(),
                        integrity,
                    )
                })
                .collect(),
        );
        let table_for_metadata = table.clone();
        let base = Arc::new(std::sync::Mutex::new(String::new()));
        let base_for_metadata = base.clone();
        let server = MiniServer::start_keep_alive_routed(move |path| {
            // Tarball request: /tarballs/<name>-<version>.tgz (checked first so
            // a package literally named "tarballs" cannot shadow it).
            for (name, version, _deps, tarball, _integrity) in table_for_metadata.iter() {
                let expected = format!("/tarballs/{}-{}.tgz", name, version);
                if path == expected {
                    return Some(RouteBody(tarball.clone(), "application/gzip"));
                }
            }
            // Metadata requests. /<name> returns the full packument;
            // /<name>/<version> returns that version's metadata (npm's
            // version-specific endpoint, used for exact-version resolution).
            for (name, version, deps, _tarball, integrity) in table_for_metadata.iter() {
                let encoded = name.replace('/', "%2F");
                let packument_path = format!("/{encoded}");
                let version_path = format!("/{encoded}/{version}");
                let base_url = base_for_metadata.lock().unwrap().clone();
                let tarball_url = format!(
                    "{}/tarballs/{}-{}.tgz",
                    base_url.trim_end_matches('/'),
                    name,
                    version
                );
                if path == packument_path {
                    return Some(RouteBody(
                        serde_json::to_vec(&packument(
                            version,
                            tarball_url,
                            integrity.clone(),
                            deps,
                        ))
                        .unwrap(),
                        "application/json",
                    ));
                }
                if path == version_path {
                    let mut dist = serde_json::Map::new();
                    dist.insert("tarball".into(), serde_json::json!(tarball_url));
                    dist.insert("integrity".into(), serde_json::json!(integrity));
                    let mut entry = serde_json::Map::new();
                    entry.insert("dist".into(), serde_json::Value::Object(dist));
                    if !deps.is_empty() {
                        entry.insert("dependencies".into(), serde_json::json!(deps));
                    }
                    return Some(RouteBody(
                        serde_json::to_vec(&serde_json::Value::Object(entry)).unwrap(),
                        "application/json",
                    ));
                }
            }
            None
        });
        let base_url = server.url("");
        *base.lock().unwrap() = base_url.clone();
        Self {
            server,
            base: base_url,
        }
    }

    fn url(&self) -> &str {
        &self.base
    }
}

fn packument(
    version: &str,
    tarball_url: String,
    integrity: String,
    deps: &BTreeMap<String, String>,
) -> serde_json::Value {
    let mut versions = serde_json::Map::new();
    let mut dist = serde_json::Map::new();
    dist.insert("tarball".into(), serde_json::json!(tarball_url));
    dist.insert("integrity".into(), serde_json::json!(integrity));
    let mut entry = serde_json::Map::new();
    entry.insert("dist".into(), serde_json::Value::Object(dist));
    if !deps.is_empty() {
        entry.insert("dependencies".into(), serde_json::json!(deps));
    }
    versions.insert(version.to_string(), serde_json::Value::Object(entry));

    let mut root = serde_json::Map::new();
    let mut tags = serde_json::Map::new();
    tags.insert("latest".into(), serde_json::json!(version));
    root.insert("dist-tags".into(), serde_json::Value::Object(tags));
    root.insert("versions".into(), serde_json::Value::Object(versions));
    serde_json::Value::Object(root)
}

fn write_manifest(project: &Path, json: &str) {
    fs::write(project.join("package.json"), json).unwrap();
}

fn run_bpm(args: &[&str], cwd: &Path, store: &Path) -> (bool, String, String) {
    let output = Command::new(bin())
        .args(args)
        .current_dir(cwd)
        .env("BPM_STORE", store)
        .env("BPM_STREAM_INSTALL", "0")
        .output()
        .expect("run bpm");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn read_manifest(project: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(project.join("package.json")).unwrap()).unwrap()
}

fn npm_available() -> bool {
    Command::new("npm")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn one_package(name: &str, version: &str, deps: &BTreeMap<String, String>) -> Package {
    Package {
        name: name.to_string(),
        version: version.to_string(),
        deps: deps.clone(),
        tarball: pkg_tarball(name, version, deps, None),
    }
}

#[test]
fn add_bare_name_saves_caret_into_dependencies() {
    let registry = MockRegistry::new(vec![one_package("lodash", "4.17.21", &BTreeMap::new())]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_manifest(project.path(), r#"{"name":"app","version":"1.0.0"}"#);

    let (ok, stdout, stderr) = run_bpm(
        &["add", "lodash", "--registry", registry.url()],
        project.path(),
        store.path(),
    );
    assert!(ok, "stderr: {stderr}\nstdout: {stdout}");

    let manifest = read_manifest(project.path());
    assert_eq!(
        manifest["dependencies"]["lodash"].as_str(),
        Some("^4.17.21")
    );
    // A bpm.lock was created (no prior lock existed).
    assert!(project.path().join("bpm.lock").is_file());
}

#[test]
fn add_propagates_remote_cache_configuration_to_install() {
    let registry = MockRegistry::new(vec![one_package("lodash", "4.17.21", &BTreeMap::new())]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_manifest(project.path(), r#"{"name":"app","version":"1.0.0"}"#);

    let (ok, _stdout, stderr) = run_bpm(
        &[
            "add",
            "lodash",
            "--registry",
            registry.url(),
            "--remote-cache",
            "http://cache.invalid",
        ],
        project.path(),
        store.path(),
    );
    assert!(
        !ok,
        "invalid remote cache configuration must not be ignored"
    );
    assert!(
        stderr.contains("invalid remote cache configuration"),
        "error should identify the remote cache configuration: {stderr}"
    );
}

#[test]
fn add_install_alias_and_i_alias_both_mutate() {
    let registry = MockRegistry::new(vec![one_package("lodash", "4.17.21", &BTreeMap::new())]);
    for alias in ["add", "install", "i"] {
        let project = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        write_manifest(project.path(), r#"{"name":"app","version":"1.0.0"}"#);
        let (ok, stdout, stderr) = run_bpm(
            &[alias, "lodash", "--registry", registry.url()],
            project.path(),
            store.path(),
        );
        assert!(ok, "[{alias}] stderr: {stderr}\nstdout: {stdout}");
        assert_eq!(
            read_manifest(project.path())["dependencies"]["lodash"].as_str(),
            Some("^4.17.21"),
            "[{alias}]"
        );
    }
}

#[test]
fn add_save_exact_writes_exact_version() {
    let registry = MockRegistry::new(vec![one_package("lodash", "4.17.21", &BTreeMap::new())]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_manifest(project.path(), r#"{"name":"app","version":"1.0.0"}"#);

    let (ok, stdout, stderr) = run_bpm(
        &[
            "add",
            "--save-exact",
            "lodash",
            "--registry",
            registry.url(),
        ],
        project.path(),
        store.path(),
    );
    assert!(ok, "stderr: {stderr}\nstdout: {stdout}");
    assert_eq!(
        read_manifest(project.path())["dependencies"]["lodash"].as_str(),
        Some("4.17.21")
    );
}

#[test]
fn add_preserves_explicit_range() {
    let registry = MockRegistry::new(vec![one_package("lodash", "4.17.21", &BTreeMap::new())]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_manifest(project.path(), r#"{"name":"app","version":"1.0.0"}"#);

    let (ok, stdout, stderr) = run_bpm(
        &["add", "lodash@^4.0.0", "--registry", registry.url()],
        project.path(),
        store.path(),
    );
    assert!(ok, "stderr: {stderr}\nstdout: {stdout}");
    assert_eq!(
        read_manifest(project.path())["dependencies"]["lodash"].as_str(),
        Some("^4.0.0")
    );
}

#[test]
fn add_save_dev_moves_out_of_dependencies() {
    let registry = MockRegistry::new(vec![one_package("lodash", "4.17.21", &BTreeMap::new())]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    // Pre-existing production dependency should move to dev when added with -D.
    write_manifest(
        project.path(),
        r#"{"name":"app","version":"1.0.0","dependencies":{"lodash":"^3.0.0"}}"#,
    );

    let (ok, stdout, stderr) = run_bpm(
        &["add", "-D", "lodash", "--registry", registry.url()],
        project.path(),
        store.path(),
    );
    assert!(ok, "stderr: {stderr}\nstdout: {stdout}");
    let manifest = read_manifest(project.path());
    assert!(manifest["dependencies"].get("lodash").is_none());
    assert_eq!(
        manifest["devDependencies"]["lodash"].as_str(),
        Some("^4.17.21")
    );
}

#[test]
fn add_to_dependencies_moves_out_of_devdependencies() {
    let registry = MockRegistry::new(vec![one_package("lodash", "4.17.21", &BTreeMap::new())]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_manifest(
        project.path(),
        r#"{"name":"app","version":"1.0.0","devDependencies":{"lodash":"^3.0.0"}}"#,
    );

    let (ok, stdout, stderr) = run_bpm(
        &["add", "lodash", "--registry", registry.url()],
        project.path(),
        store.path(),
    );
    assert!(ok, "stderr: {stderr}\nstdout: {stdout}");
    let manifest = read_manifest(project.path());
    assert_eq!(
        manifest["dependencies"]["lodash"].as_str(),
        Some("^4.17.21")
    );
    assert!(manifest
        .get("devDependencies")
        .and_then(|d| d.get("lodash"))
        .is_none());
}

#[test]
fn add_preserves_unknown_manifest_fields() {
    let registry = MockRegistry::new(vec![one_package("lodash", "4.17.21", &BTreeMap::new())]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_manifest(
        project.path(),
        r#"{"name":"app","version":"1.0.0","license":"MIT",
        "publishConfig":{"access":"public"},
        "exports":{".":"./index.js"}}"#,
    );

    let (ok, stdout, stderr) = run_bpm(
        &["add", "lodash", "--registry", registry.url()],
        project.path(),
        store.path(),
    );
    assert!(ok, "stderr: {stderr}\nstdout: {stdout}");
    let manifest = read_manifest(project.path());
    assert_eq!(manifest["license"].as_str(), Some("MIT"));
    assert_eq!(manifest["publishConfig"]["access"].as_str(), Some("public"));
    assert_eq!(manifest["exports"]["."].as_str(), Some("./index.js"));
    assert_eq!(
        manifest["dependencies"]["lodash"].as_str(),
        Some("^4.17.21")
    );
}

#[test]
fn add_scoped_name_and_multiple_targets() {
    let registry = MockRegistry::new(vec![
        one_package("@scope/lib", "1.2.0", &BTreeMap::new()),
        one_package("chalk", "5.0.0", &BTreeMap::new()),
    ]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_manifest(project.path(), r#"{"name":"app","version":"1.0.0"}"#);

    let (ok, stdout, stderr) = run_bpm(
        &["add", "@scope/lib", "chalk", "--registry", registry.url()],
        project.path(),
        store.path(),
    );
    assert!(ok, "stderr: {stderr}\nstdout: {stdout}");
    let manifest = read_manifest(project.path());
    assert_eq!(
        manifest["dependencies"]["@scope/lib"].as_str(),
        Some("^1.2.0")
    );
    assert_eq!(manifest["dependencies"]["chalk"].as_str(), Some("^5.0.0"));
}

#[test]
fn add_resolves_a_transitive_dependency() {
    // `left-pad` depends on `dep-alpha`; both must be fetched and installed.
    let mut left_deps = BTreeMap::new();
    left_deps.insert("dep-alpha".to_string(), "^2.0.0".to_string());
    let registry = MockRegistry::new(vec![
        one_package("left-pad", "1.3.0", &left_deps),
        one_package("dep-alpha", "2.1.0", &BTreeMap::new()),
    ]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_manifest(project.path(), r#"{"name":"app","version":"1.0.0"}"#);

    let (ok, stdout, stderr) = run_bpm(
        &["add", "left-pad", "--registry", registry.url()],
        project.path(),
        store.path(),
    );
    assert!(ok, "stderr: {stderr}\nstdout: {stdout}");
    let lock = fs::read_to_string(project.path().join("bpm.lock")).unwrap();
    assert!(lock.contains("left-pad"), "{lock}");
    assert!(lock.contains("dep-alpha"), "{lock}");
}

#[test]
fn add_to_package_lock_project_exports_npm_v3() {
    let registry = MockRegistry::new(vec![one_package("lodash", "4.17.21", &BTreeMap::new())]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_manifest(project.path(), r#"{"name":"app","version":"1.0.0"}"#);
    // A pre-existing package-lock.json v3 makes this an npm-authority project.
    fs::write(
        project.path().join("package-lock.json"),
        r#"{"name":"app","lockfileVersion":3,"packages":{"":{"name":"app","version":"1.0.0"}}}"#,
    )
    .unwrap();

    let (ok, stdout, stderr) = run_bpm(
        &["add", "lodash", "--registry", registry.url()],
        project.path(),
        store.path(),
    );
    assert!(ok, "stderr: {stderr}\nstdout: {stdout}");

    let lock_text = fs::read_to_string(project.path().join("package-lock.json")).unwrap();
    let lock: serde_json::Value = serde_json::from_str(&lock_text).unwrap();
    assert_eq!(lock["lockfileVersion"].as_u64(), Some(3));
    assert_eq!(
        lock["packages"][""]["dependencies"]["lodash"].as_str(),
        Some("^4.17.21")
    );
    let lodash_pkg = &lock["packages"]["node_modules/lodash"];
    assert_eq!(lodash_pkg["version"].as_str(), Some("4.17.21"));
    assert!(lodash_pkg["resolved"]
        .as_str()
        .unwrap()
        .contains("lodash-4.17.21.tgz"));
    assert!(lodash_pkg["integrity"]
        .as_str()
        .unwrap()
        .starts_with("sha512-"));
    // No bpm.lock was introduced alongside the package-lock.
    assert!(!project.path().join("bpm.lock").exists());
}

#[test]
fn add_does_not_mutate_on_resolver_failure() {
    // Target resolves, but a transitive dependency has no published version:
    // resolution must fail and leave package.json byte-identical.
    let mut left_deps = BTreeMap::new();
    left_deps.insert("missing-dep".to_string(), "^9.9.9".to_string());
    let registry = MockRegistry::new(vec![one_package("left-pad", "1.3.0", &left_deps)]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let original = r#"{"name":"app","version":"1.0.0"}"#;
    write_manifest(project.path(), original);

    let (ok, _stdout, _stderr) = run_bpm(
        &["add", "left-pad", "--registry", registry.url()],
        project.path(),
        store.path(),
    );
    assert!(!ok, "expected resolution to fail");
    assert_eq!(
        fs::read_to_string(project.path().join("package.json")).unwrap(),
        original,
        "manifest must be byte-identical after a pre-publish failure"
    );
    assert!(!project.path().join("bpm.lock").exists());
}

#[test]
fn add_rejects_non_registry_target_before_mutation() {
    let registry = MockRegistry::new(vec![]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let original = r#"{"name":"app","version":"1.0.0"}"#;
    write_manifest(project.path(), original);

    let (ok, _stdout, stderr) = run_bpm(
        &["add", "./local-pkg", "--registry", registry.url()],
        project.path(),
        store.path(),
    );
    assert!(!ok);
    assert!(stderr.contains("registry package specs"), "{stderr}");
    assert_eq!(
        fs::read_to_string(project.path().join("package.json")).unwrap(),
        original
    );
}

#[test]
fn add_rejects_ambiguous_optional_dependency() {
    let registry = MockRegistry::new(vec![one_package("dual", "1.0.0", &BTreeMap::new())]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let original = r#"{"name":"app","version":"1.0.0","optionalDependencies":{"dual":"^1.0.0"}}"#;
    write_manifest(project.path(), original);

    let (ok, _stdout, stderr) = run_bpm(
        &["add", "dual", "--registry", registry.url()],
        project.path(),
        store.path(),
    );
    assert!(!ok);
    assert!(stderr.contains("optionalDependencies") || stderr.contains("peerDependencies"));
    assert_eq!(
        fs::read_to_string(project.path().join("package.json")).unwrap(),
        original
    );
}

#[test]
fn remove_strips_a_dependency_and_rewrites_lock() {
    let registry = MockRegistry::new(vec![one_package("lodash", "4.17.21", &BTreeMap::new())]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_manifest(
        project.path(),
        r#"{"name":"app","version":"1.0.0","dependencies":{"lodash":"^4.17.21"}}"#,
    );

    let (add_ok, _stdout, stderr) = run_bpm(
        &["add", "lodash", "--registry", registry.url()],
        project.path(),
        store.path(),
    );
    assert!(add_ok, "{stderr}");
    assert!(project.path().join("bpm.lock").is_file());

    let (rm_ok, _stdout, stderr) = run_bpm(
        &["remove", "lodash", "--registry", registry.url()],
        project.path(),
        store.path(),
    );
    assert!(rm_ok, "{stderr}");
    let manifest = read_manifest(project.path());
    assert!(manifest["dependencies"].get("lodash").is_none());
}

/// Read the persisted `.bpm-state` ownership list (project-relative paths).
fn read_owned_paths(project: &Path) -> Vec<String> {
    let text = fs::read_to_string(project.join(".bpm-state")).expect(".bpm-state");
    let state: serde_json::Value = serde_json::from_str(&text).expect("parse .bpm-state");
    state["owned_entries"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e["path"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Plan 011: after add, `.bpm-state` persists nonempty, sorted ownership that
/// includes the package's shallow entry; after remove, the view entry is gone,
/// the plan no longer owns it, and an unrelated user-created entry survives.
#[test]
fn remove_deletes_owned_view_entry_and_persists_ownership() {
    let registry = MockRegistry::new(vec![one_package("lodash", "4.17.21", &BTreeMap::new())]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_manifest(
        project.path(),
        r#"{"name":"app","version":"1.0.0","dependencies":{"lodash":"^4.17.21"}}"#,
    );

    let (ok, _stdout, stderr) = run_bpm(
        &["add", "lodash", "--registry", registry.url()],
        project.path(),
        store.path(),
    );
    assert!(ok, "{stderr}");
    assert!(
        project.path().join("node_modules/lodash").is_dir(),
        "lodash view entry must exist after add"
    );

    // Ownership is nonempty, sorted, and includes the shallow lodash entry.
    let owned = read_owned_paths(project.path());
    assert!(
        !owned.is_empty(),
        ".bpm-state ownership must be nonempty after add"
    );
    assert_eq!(
        owned,
        {
            let mut s = owned.clone();
            s.sort();
            s
        },
        "ownership must be sorted"
    );
    assert!(
        owned.iter().any(|p| p == "node_modules/lodash"),
        "ownership must include the shallow lodash entry"
    );

    // An unrelated user-created entry must survive the remove.
    fs::create_dir_all(project.path().join("node_modules/user-kept")).unwrap();
    fs::write(
        project.path().join("node_modules/user-kept/README"),
        b"do not touch",
    )
    .unwrap();

    let (rm_ok, _stdout, stderr) = run_bpm(
        &["remove", "lodash", "--registry", registry.url()],
        project.path(),
        store.path(),
    );
    assert!(rm_ok, "{stderr}");

    assert!(
        !project.path().join("node_modules/lodash").exists(),
        "removed package's view entry must disappear"
    );
    let owned_after = read_owned_paths(project.path());
    assert!(
        !owned_after.iter().any(|p| p == "node_modules/lodash"),
        "rewritten plan must not retain the removed owned path"
    );
    assert!(
        project
            .path()
            .join("node_modules/user-kept/README")
            .exists(),
        "unrelated user-created entry must survive reconciliation"
    );
}

/// Plan 011: a local-view (`BPM_PROJECT_VIEW=local`) remove must delete the
/// stale package directory, proving local/reflink ownership reconciliation.
fn run_bpm_with_env(
    args: &[&str],
    cwd: &Path,
    store: &Path,
    env: &[(&str, &str)],
) -> (bool, String, String) {
    let mut cmd = Command::new(bin());
    cmd.args(args)
        .current_dir(cwd)
        .env("BPM_STORE", store)
        .env("BPM_STREAM_INSTALL", "0");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("run bpm");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[test]
fn local_view_remove_deletes_stale_package_directory() {
    let registry = MockRegistry::new(vec![one_package("lodash", "4.17.21", &BTreeMap::new())]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_manifest(
        project.path(),
        r#"{"name":"app","version":"1.0.0","dependencies":{"lodash":"^4.17.21"}}"#,
    );

    let (ok, _stdout, stderr) = run_bpm_with_env(
        &["add", "lodash", "--registry", registry.url()],
        project.path(),
        store.path(),
        &[("BPM_PROJECT_VIEW", "local")],
    );
    assert!(ok, "{stderr}");
    let pkg = project.path().join("node_modules/lodash");
    assert!(pkg.is_dir(), "local view must materialize lodash");
    // A local view entry is a real directory, not a relay symlink.
    assert!(!fs::symlink_metadata(&pkg).unwrap().file_type().is_symlink());

    let (rm_ok, _stdout, stderr) = run_bpm_with_env(
        &["remove", "lodash", "--registry", registry.url()],
        project.path(),
        store.path(),
        &[("BPM_PROJECT_VIEW", "local")],
    );
    assert!(rm_ok, "{stderr}");
    assert!(
        !pkg.exists(),
        "local-view remove must delete the stale directory"
    );
}

/// Plan 011: a recorded local directory that the user replaced before the next
/// graph change must NOT be recursively deleted — BPM cannot prove it still
/// owns the mismatched tree.
#[test]
fn reconcile_refuses_to_delete_a_user_replaced_local_directory() {
    let registry = MockRegistry::new(vec![one_package("lodash", "4.17.21", &BTreeMap::new())]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_manifest(
        project.path(),
        r#"{"name":"app","version":"1.0.0","dependencies":{"lodash":"^4.17.21"}}"#,
    );

    let (ok, _stdout, stderr) = run_bpm_with_env(
        &["add", "lodash", "--registry", registry.url()],
        project.path(),
        store.path(),
        &[("BPM_PROJECT_VIEW", "local")],
    );
    assert!(ok, "{stderr}");
    let pkg = project.path().join("node_modules/lodash");
    // User replaces the recorded tree after install, so its live fingerprint no
    // longer matches the recorded identity.
    fs::write(pkg.join("INJECTED-BY-USER"), b"important").unwrap();

    let (rm_ok, stdout, stderr) = run_bpm_with_env(
        &["remove", "lodash", "--registry", registry.url()],
        project.path(),
        store.path(),
        &[("BPM_PROJECT_VIEW", "local")],
    );
    assert!(rm_ok, "remove must still succeed overall\n{stderr}");
    assert!(
        pkg.join("INJECTED-BY-USER").exists(),
        "the mismatched/replaced directory must be preserved, not deleted;\nstdout: {stdout}\nstderr: {stderr}"
    );
    let _ = stdout;
}

#[test]
fn remove_aliases_all_work() {
    let registry = MockRegistry::new(vec![one_package("lodash", "4.17.21", &BTreeMap::new())]);
    for alias in ["remove", "uninstall", "rm", "un"] {
        let project = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        write_manifest(
            project.path(),
            r#"{"name":"app","version":"1.0.0","dependencies":{"lodash":"^4.17.21"}}"#,
        );
        let (ok, _stdout, stderr) = run_bpm(
            &[alias, "lodash", "--registry", registry.url()],
            project.path(),
            store.path(),
        );
        assert!(ok, "[{alias}] {stderr}");
        assert!(read_manifest(project.path())["dependencies"]
            .get("lodash")
            .is_none());
    }
}

#[test]
fn remove_no_op_leaves_manifest_byte_identical() {
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let original = r#"{"name":"app","version":"1.0.0","dependencies":{"lodash":"^4.17.21"}}"#;
    write_manifest(project.path(), original);

    let (ok, _stdout, stderr) = run_bpm(
        &["remove", "not-present", "--registry", "http://127.0.0.1:1"],
        project.path(),
        store.path(),
    );
    assert!(ok, "{stderr}");
    assert_eq!(
        fs::read_to_string(project.path().join("package.json")).unwrap(),
        original
    );
}

#[test]
fn remove_rejects_global() {
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_manifest(project.path(), r#"{"name":"app","version":"1.0.0"}"#);
    let (ok, _stdout, stderr) = run_bpm(&["remove", "-g", "lodash"], project.path(), store.path());
    assert!(!ok);
    assert!(stderr.contains("global"), "{stderr}");
}

#[test]
fn install_without_g_and_without_target_installs_lockfile() {
    let registry = MockRegistry::new(vec![one_package("lodash", "4.17.21", &BTreeMap::new())]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_manifest(project.path(), r#"{"name":"app","version":"1.0.0"}"#);

    let (add_ok, _stdout, stderr) = run_bpm(
        &["add", "lodash", "--registry", registry.url()],
        project.path(),
        store.path(),
    );
    assert!(add_ok, "{stderr}");

    // A second plain `bpm install` must not mutate; it installs from the lock.
    let before = fs::read_to_string(project.path().join("package.json")).unwrap();
    let (ok, _stdout, stderr) = run_bpm(&["install"], project.path(), store.path());
    assert!(ok, "{stderr}");
    assert_eq!(
        fs::read_to_string(project.path().join("package.json")).unwrap(),
        before
    );
}

#[test]
fn npm_ci_accepts_exported_package_lock() {
    if !npm_available() {
        eprintln!("[mutation npm-interop] npm not on PATH; skipping");
        return;
    }
    let registry = MockRegistry::new(vec![one_package("demo-cli", "1.0.0", &BTreeMap::new())]);
    let project = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    write_manifest(project.path(), r#"{"name":"app","version":"1.0.0"}"#);
    fs::write(
        project.path().join("package-lock.json"),
        r#"{"name":"app","lockfileVersion":3,"packages":{"":{"name":"app","version":"1.0.0"}}}"#,
    )
    .unwrap();
    fs::write(
        project.path().join(".npmrc"),
        format!("registry={}\n", registry.url()),
    )
    .unwrap();

    let (ok, stdout, stderr) = run_bpm(
        &["add", "demo-cli", "--registry", registry.url()],
        project.path(),
        store.path(),
    );
    assert!(ok, "bpm add stderr: {stderr}\nstdout: {stdout}");

    // npm ci must accept BPM's exported package-lock.json and install the
    // declared top-level package. A present npm returning nonzero fails.
    let npm = Command::new("npm")
        .args(["ci", "--ignore-scripts"])
        .current_dir(project.path())
        .output()
        .expect("run npm ci");
    assert!(
        npm.status.success(),
        "npm ci failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&npm.stdout),
        String::from_utf8_lossy(&npm.stderr)
    );
    assert!(
        project.path().join("node_modules/demo-cli").is_dir(),
        "npm ci did not install demo-cli"
    );
}
