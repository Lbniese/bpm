//! Import of npm `package-lock.json` (lockfileVersion 3) into a BPM lockfile.
//!
//! npm v3 stores a *flat* package table keyed by the package's
//! `node_modules/...` path. The layout of the tree (including hoisting and
//! nested copies) is fully encoded by those path keys, so the importer is a
//! normalization pass: it reads the v3 table, validates it, and emits a
//! canonical [`crate::lockfile::Lockfile`] plus structured diagnostics for
//! anything BPM does not yet honor. The source lockfile is never modified
//! (§10).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::diagnostic::{Diagnostic, Severity};
use crate::lockfile::{LockSource, Lockfile, PackageEntry, RootEntry};
use crate::manifest::PackageManifest;
use crate::resolver::overrides::{OverrideOrigin, OverrideSet};

/// The npm lockfile version this importer supports.
pub const SUPPORTED_LOCKFILE_VERSION: u32 = 3;

/// Result of importing a package-lock.
#[derive(Debug)]
pub struct ImportReport {
    /// The canonical `bpm.lock` to be written.
    pub lockfile: Lockfile,
    /// Diagnostics for constructs that were recorded but not fully honored.
    pub diagnostics: Vec<Diagnostic>,
}

/// Errors importing a package-lock.
#[derive(Debug, Error)]
pub enum NpmLockError {
    #[error("failed to parse package-lock.json: {0}")]
    Parse(#[from] serde_json::Error),
    #[error(
        "unsupported lockfileVersion {0}: only version {SUPPORTED_LOCKFILE_VERSION} is supported"
    )]
    UnsupportedVersion(u32),
    #[error("package-lock.json has no \"packages\" table")]
    NoPackages,
    #[error("package \"{path}\" has invalid \"bin\": {reason}")]
    InvalidBin { path: String, reason: String },
    #[error("cannot record package.json root resolution metadata: {0}")]
    ManifestMetadata(String),
    #[error("package-lock.json contains constructs unsupported for direct install: {0}")]
    DirectInstallUnsupported(String),
    #[error("cannot export package-lock v3: unsupported package layouts: {0}")]
    ExportUnsupported(String),
}

#[derive(Debug, Default, Deserialize)]
struct RawLock {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "lockfileVersion")]
    lockfile_version: Option<u32>,
    #[serde(default)]
    packages: BTreeMap<String, RawPkg>,
}

#[derive(Debug, Default, Deserialize)]
struct RawPkg {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    resolved: Option<String>,
    #[serde(default)]
    integrity: Option<String>,
    #[serde(default)]
    link: Option<bool>,
    #[serde(default)]
    dev: Option<bool>,
    #[serde(default)]
    optional: Option<bool>,
    #[serde(default)]
    bin: serde_json::Value,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    /// Root-only: npm's package-lock v3 records the project's devDependencies
    /// under the root `""` entry. We merge these into the lockfile's root
    /// `dependencies` so the frozen installer can detect drift across both
    /// production and dev declarations.
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "peerDependencies")]
    peer_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    os: Vec<String>,
    #[serde(default)]
    cpu: Vec<String>,
}

/// Enrich an imported lockfile with root metadata that npm stores in the
/// project's `package.json` rather than in `package-lock.json`.
///
/// Without this step, `bpm import` produces a lockfile whose root dependency
/// keys may be correct but whose v2 resolution metadata is empty. A subsequent
/// `bpm ci` then (correctly) sees dev/optional/override declarations missing
/// from the lockfile and rejects the file that import just generated.
pub fn apply_manifest_root_metadata(
    lockfile: &mut Lockfile,
    manifest: &PackageManifest,
) -> Result<(), NpmLockError> {
    let root_declarations = manifest.root_dependency_declarations();
    let overrides = OverrideSet::from_manifest(
        &manifest.overrides,
        &root_declarations,
        OverrideOrigin::Root,
    )
    .map_err(|error| NpmLockError::ManifestMetadata(error.to_string()))?;

    // The imported npm table remains authoritative for root declarations,
    // physical placements, and exact artifacts. Do not replace its dependency
    // map with the manifest: preserving that map lets `bpm ci` detect stale or
    // incomplete package-lock input. The manifest supplies the v2 metadata
    // fields that npm stores outside the lockfile.
    lockfile.resolution.root.dev_dependencies = manifest.dev_dependencies.clone();
    lockfile.resolution.root.optional_dependencies = manifest.optional_dependencies.clone();
    lockfile.resolution.root.overrides = overrides.as_map().clone();
    Ok(())
}

/// Derive a package name from its `node_modules/...` path key.
///
/// The name is everything after the last `node_modules/` segment, so scoped
/// names (`@scope/name`) and deeply nested copies are handled.
pub fn package_name_from_path(path: &str) -> String {
    match path.rfind("node_modules/") {
        Some(i) => path[i + "node_modules/".len()..].to_string(),
        // Non-node_modules keys (e.g. workspace directories): use the last
        // path segment as the name.
        None => path.rsplit('/').next().unwrap_or(path).to_string(),
    }
}

fn parse_bin(
    path: &str,
    name: &str,
    value: &serde_json::Value,
) -> Result<BTreeMap<String, String>, NpmLockError> {
    match value {
        serde_json::Value::Null => Ok(BTreeMap::new()),
        serde_json::Value::String(s) => Ok(BTreeMap::from([(name.to_string(), s.clone())])),
        serde_json::Value::Object(map) => {
            let mut out = BTreeMap::new();
            for (k, v) in map {
                let s = v.as_str().ok_or_else(|| NpmLockError::InvalidBin {
                    path: path.to_string(),
                    reason: format!("bin entry \"{k}\" must be a string"),
                })?;
                out.insert(k.clone(), s.to_string());
            }
            Ok(out)
        }
        other => Err(NpmLockError::InvalidBin {
            path: path.to_string(),
            reason: format!("expected string or object, got {}", other_type(other)),
        }),
    }
}

fn other_type(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn warn(code: &'static str, package: impl Into<String>, message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(Severity::Warning, code, message).with_package(package)
}

fn info(code: &'static str, package: impl Into<String>, message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(Severity::Info, code, message).with_package(package)
}

/// Import a `package-lock.json` document into a canonical [`Lockfile`].
///
/// Never panics on missing fields; records warnings for unsupported constructs.
pub fn import(json: &str) -> Result<ImportReport, NpmLockError> {
    let raw: RawLock = serde_json::from_str(json)?;
    let version = raw.lockfile_version.unwrap_or(0);
    if version != SUPPORTED_LOCKFILE_VERSION {
        return Err(NpmLockError::UnsupportedVersion(version));
    }
    if raw.packages.is_empty() {
        return Err(NpmLockError::NoPackages);
    }

    let mut diagnostics = Vec::new();
    let mut lockfile = Lockfile::new("bpm");

    // Root entry lives under the "" key.
    if let Some(root_raw) = raw.packages.get("") {
        // Merge devDependencies into the root's declared dependencies so the
        // frozen installer's drift check covers both production and dev deps.
        // (A name declared in both resolves to its `dependencies` spec.)
        let mut root_deps = root_raw.dev_dependencies.clone();
        for (name, spec) in &root_raw.dependencies {
            root_deps.insert(name.clone(), spec.clone());
        }
        for (name, spec) in &root_raw.optional_dependencies {
            root_deps.insert(name.clone(), spec.clone());
        }
        for (name, spec) in &root_raw.peer_dependencies {
            root_deps
                .entry(name.clone())
                .or_insert_with(|| spec.clone());
        }
        lockfile.root = RootEntry {
            name: raw.name.clone(),
            version: root_raw.version.clone(),
            dependencies: root_deps,
        };
    }

    for (path, pkg) in raw.packages.iter() {
        if path.is_empty() {
            continue; // root handled above
        }
        let name = package_name_from_path(path);
        let version = pkg.version.clone().unwrap_or_default();
        let link = pkg.link.unwrap_or(false);

        if link {
            diagnostics.push(warn(
                "LINK_PACKAGE_UNSUPPORTED",
                name.clone(),
                format!(
                    "package \"{name}\" at {path} is a link/workspace entry; \
                     BPM has not materialized it yet"
                ),
            ));
        }

        if !pkg.os.is_empty() || !pkg.cpu.is_empty() {
            diagnostics.push(info(
                "PLATFORM_CONSTRAINT",
                name.clone(),
                format!(
                    "package \"{name}\" declares os/cpu constraints ({}); \
                     BPM records and enforces them during installation",
                    format_constraints(&pkg.os, &pkg.cpu)
                ),
            ));
        }

        let resolved = pkg.resolved.clone().unwrap_or_default();
        let integrity = pkg.integrity.clone();
        if !link && resolved.is_empty() {
            diagnostics.push(warn(
                "MISSING_RESOLVED",
                name.clone(),
                format!(
                    "package \"{name}\" at {path} has no resolved URL; \
                     it cannot be installed from a registry"
                ),
            ));
        }

        let bin = parse_bin(path, &name, &pkg.bin)?;

        lockfile.packages.push(PackageEntry {
            path: path.clone(),
            name,
            version,
            resolved,
            workspace_target: None,
            integrity,
            link,
            dev: pkg.dev.unwrap_or(false),
            optional: pkg.optional.unwrap_or(false),
            os: pkg.os.clone(),
            cpu: pkg.cpu.clone(),
            bin,
            dependencies: {
                let mut dependencies = pkg.dependencies.clone();
                for (name, spec) in &pkg.peer_dependencies {
                    dependencies
                        .entry(name.clone())
                        .or_insert_with(|| spec.clone());
                }
                dependencies
            },
        });
    }

    lockfile.sort_packages();
    crate::diagnostic::sort_diagnostics(&mut diagnostics);
    Ok(ImportReport {
        lockfile,
        diagnostics,
    })
}

fn format_constraints(os: &[String], cpu: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !os.is_empty() {
        parts.push(format!("os=[{}]", os.join(",")));
    }
    if !cpu.is_empty() {
        parts.push(format!("cpu=[{}]", cpu.join(",")));
    }
    parts.join(" ")
}

// ===========================================================================
// Export: canonical BPM lockfile -> npm `package-lock.json` (lockfileVersion 3)
// ===========================================================================
//
// The exporter is the inverse of [`import`] for the narrow registry-only
// corpus this milestone supports. It rebuilds npm's flat `packages` table from
// a resolved [`Lockfile`], taking the root dependency groups from the edited
// manifest (where npm stores them) and physical placements from the lock.
//
// A strict support gate rejects graphs BPM cannot represent safely before
// emitting a single byte: workspace links, Git/File/Tarball/Patch sources,
// and packages missing `resolved` or `integrity`. Returning all unsupported
// paths in one error lets `bpm add` fail fast with an actionable list instead
// of writing a plausible-but-lossy lock.

/// Why a single package cannot be exported to npm v3 in this milestone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsupportedReason {
    /// `link: true` workspace/symlink entry; npm records it but BPM has not
    /// materialized it and cannot represent its target deterministically.
    Link,
    /// Registry tarball URL is absent.
    MissingResolved,
    /// npm integrity string is absent; a published v3 lock must verify.
    MissingIntegrity,
    /// Source is Git/File/Tarball/Patch rather than a registry tarball.
    NonRegistrySource,
}

impl UnsupportedReason {
    fn label(&self) -> &'static str {
        match self {
            UnsupportedReason::Link => "workspace link",
            UnsupportedReason::MissingResolved => "missing resolved URL",
            UnsupportedReason::MissingIntegrity => "missing integrity",
            UnsupportedReason::NonRegistrySource => "non-registry source",
        }
    }
}

/// One package the exporter refuses to emit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedPackage {
    pub path: String,
    pub reason: UnsupportedReason,
}

impl UnsupportedPackage {
    fn label(&self) -> String {
        format!("{} ({})", self.path, self.reason.label())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NpmV3Lock {
    name: Option<String>,
    version: Option<String>,
    lockfile_version: u32,
    requires: bool,
    packages: BTreeMap<String, NpmV3Package>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NpmV3Package {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    integrity: Option<String>,
    #[serde(skip_serializing_if = "skip_false")]
    dev: bool,
    #[serde(skip_serializing_if = "skip_false")]
    optional: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    bin: Option<Value>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    dependencies: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    optional_dependencies: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    peer_dependencies: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    os: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cpu: Vec<String>,
}

fn skip_false(value: &bool) -> bool {
    !value
}

fn bin_value(bin: &BTreeMap<String, String>) -> Option<Value> {
    if bin.is_empty() {
        return None;
    }
    let mut map = Map::new();
    for (name, path) in bin {
        map.insert(name.clone(), Value::String(path.clone()));
    }
    Some(Value::Object(map))
}

/// Inspect one resolved package and decide whether it can be represented in
/// npm v3. Returns `Some(reason)` when it cannot.
fn unsupported_reason(package: &PackageEntry, lockfile: &Lockfile) -> Option<UnsupportedReason> {
    if package.link {
        return Some(UnsupportedReason::Link);
    }
    if package.resolved.is_empty() {
        return Some(UnsupportedReason::MissingResolved);
    }
    if package.integrity.as_deref().unwrap_or_default().is_empty() {
        return Some(UnsupportedReason::MissingIntegrity);
    }
    let source = lockfile
        .resolution
        .packages
        .get(&package.path)
        .map(|r| &r.source);
    match source {
        Some(LockSource::Registry { .. }) => None,
        Some(_) => Some(UnsupportedReason::NonRegistrySource),
        None => {
            // No resolution metadata: trust the resolved URL scheme. Registry
            // tarballs are http(s); `file:`, `git+`, `link:` and friends are
            // not exportable in this milestone.
            if package.resolved.starts_with("http://") || package.resolved.starts_with("https://") {
                None
            } else {
                Some(UnsupportedReason::NonRegistrySource)
            }
        }
    }
}

/// Export a resolved [`Lockfile`] plus its edited manifest as npm
/// `package-lock.json` (lockfileVersion 3) bytes.
///
/// The root `""` entry's dependency groups come from `manifest`; physical
/// package entries come from `lockfile`. Rejects any graph containing a
/// workspace link, a non-registry source, or a package missing `resolved` or
/// `integrity`, returning every offending path in deterministic order.
pub fn export_v3(lockfile: &Lockfile, manifest: &PackageManifest) -> Result<Vec<u8>, NpmLockError> {
    let mut unsupported: Vec<UnsupportedPackage> = lockfile
        .packages
        .iter()
        .filter(|package| !package.path.is_empty())
        .filter_map(|package| {
            unsupported_reason(package, lockfile).map(|reason| UnsupportedPackage {
                path: package.path.clone(),
                reason,
            })
        })
        .collect();
    unsupported.sort_by(|a, b| a.path.cmp(&b.path));
    if !unsupported.is_empty() {
        let summary = unsupported
            .iter()
            .map(UnsupportedPackage::label)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(NpmLockError::ExportUnsupported(summary));
    }

    let mut packages: BTreeMap<String, NpmV3Package> = BTreeMap::new();
    packages.insert(
        String::new(),
        NpmV3Package {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            resolved: None,
            integrity: None,
            dev: false,
            optional: false,
            bin: None,
            dependencies: manifest.dependencies.clone(),
            dev_dependencies: manifest.dev_dependencies.clone(),
            optional_dependencies: manifest.optional_dependencies.clone(),
            peer_dependencies: manifest.peer_dependencies.clone(),
            os: Vec::new(),
            cpu: Vec::new(),
        },
    );
    for package in lockfile.packages.iter() {
        if package.path.is_empty() {
            continue;
        }
        packages.insert(
            package.path.clone(),
            NpmV3Package {
                name: None,
                version: Some(package.version.clone()),
                resolved: Some(package.resolved.clone()),
                integrity: package.integrity.clone(),
                dev: package.dev,
                optional: package.optional,
                bin: bin_value(&package.bin),
                dependencies: package.dependencies.clone(),
                dev_dependencies: BTreeMap::new(),
                optional_dependencies: BTreeMap::new(),
                peer_dependencies: BTreeMap::new(),
                os: package.os.clone(),
                cpu: package.cpu.clone(),
            },
        );
    }

    let lock = NpmV3Lock {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        lockfile_version: SUPPORTED_LOCKFILE_VERSION,
        requires: true,
        packages,
    };
    let mut bytes = serde_json::to_vec_pretty(&lock).map_err(NpmLockError::Parse)?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn minimal_v3() -> &'static str {
        r#"{
          "name": "app",
          "version": "1.0.0",
          "lockfileVersion": 3,
          "packages": {
            "": { "version": "1.0.0", "dependencies": { "foo": "^1.0.0" } },
            "node_modules/foo": {
              "version": "1.2.3",
              "resolved": "https://example/foo-1.2.3.tgz",
              "integrity": "sha512-AAA",
              "bin": "cli.js"
            },
            "node_modules/@scope/bar": {
              "version": "4.5.6",
              "resolved": "https://example/bar-4.5.6.tgz",
              "integrity": "sha512-BBB",
              "dependencies": { "foo": "^1.0.0" }
            }
          }
        }"#
    }

    #[test]
    fn imports_names_versions_and_paths() {
        let report = import(minimal_v3()).unwrap();
        assert_eq!(report.lockfile.root.version.as_deref(), Some("1.0.0"));
        assert_eq!(
            report
                .lockfile
                .root
                .dependencies
                .get("foo")
                .map(|s| s.as_str()),
            Some("^1.0.0")
        );
        let foo = report
            .lockfile
            .packages
            .iter()
            .find(|p| p.name == "foo")
            .unwrap();
        assert_eq!(foo.version, "1.2.3");
        assert_eq!(foo.resolved, "https://example/foo-1.2.3.tgz");
        assert_eq!(foo.integrity.as_deref(), Some("sha512-AAA"));
        assert_eq!(foo.bin.get("foo").map(|s| s.as_str()), Some("cli.js"));
        let bar = report
            .lockfile
            .packages
            .iter()
            .find(|p| p.name == "@scope/bar")
            .unwrap();
        assert_eq!(bar.path, "node_modules/@scope/bar");
        assert_eq!(bar.version, "4.5.6");
    }

    #[test]
    fn packages_are_sorted_by_path_for_determinism() {
        let report = import(minimal_v3()).unwrap();
        let paths: Vec<&str> = report
            .lockfile
            .packages
            .iter()
            .map(|p| p.path.as_str())
            .collect();
        assert_eq!(
            paths,
            vec!["node_modules/@scope/bar", "node_modules/foo"],
            "must be sorted lexicographically by path"
        );
    }

    #[test]
    fn rejects_unsupported_versions() {
        let v2 = minimal_v3().replace("\"lockfileVersion\": 3", "\"lockfileVersion\": 2");
        let err = import(&v2).unwrap_err();
        assert!(
            matches!(err, NpmLockError::UnsupportedVersion(2)),
            "{err:?}"
        );
    }

    #[test]
    fn flags_link_and_platform_constructs() {
        let json = r#"{
          "lockfileVersion": 3,
          "packages": {
            "": { "version": "1.0.0" },
            "node_modules/native": {
              "version": "1.0.0",
              "resolved": "https://example/native-1.0.0.tgz",
              "integrity": "sha512-N",
              "os": ["linux"],
              "cpu": ["x64"]
            },
            "apps/widget": { "version": "1.0.0", "link": true }
          }
        }"#;
        let report = import(json).unwrap();
        let codes: Vec<&str> = report.diagnostics.iter().map(|d| d.code).collect();
        assert!(codes.contains(&"PLATFORM_CONSTRAINT"));
        assert!(codes.contains(&"LINK_PACKAGE_UNSUPPORTED"));
    }

    #[test]
    fn rejects_invalid_bin() {
        let json = r#"{
          "lockfileVersion": 3,
          "packages": {
            "": { "version": "1.0.0" },
            "node_modules/bad": { "version": "1.0.0", "bin": ["nope"] }
          }
        }"#;
        let err = import(json).unwrap_err();
        assert!(matches!(err, NpmLockError::InvalidBin { .. }), "{err:?}");
    }

    #[test]
    fn package_name_handles_scope_and_nesting() {
        assert_eq!(package_name_from_path("node_modules/foo"), "foo");
        assert_eq!(
            package_name_from_path("node_modules/@scope/bar"),
            "@scope/bar"
        );
        assert_eq!(
            package_name_from_path("node_modules/foo/node_modules/bar"),
            "bar"
        );
    }

    #[test]
    fn imported_lock_can_record_manifest_metadata_for_frozen_validation() {
        let json = r#"{
          "name": "app",
          "lockfileVersion": 3,
          "packages": {
            "": {
              "version": "1.0.0",
              "dependencies": { "foo": "1.0.0", "native": "^3.0.0" },
              "devDependencies": { "tool": "^2.0.0" }
            },
            "node_modules/foo": { "version": "1.0.0", "resolved": "https://example/foo.tgz", "integrity": "sha512-A" }
          }
        }"#;
        let mut lock = import(json).unwrap().lockfile;
        let manifest = PackageManifest::from_json(
            r#"{
              "name": "app",
              "dependencies": { "foo": "1.0.0" },
              "devDependencies": { "tool": "^2.0.0" },
              "optionalDependencies": { "native": "^3.0.0" },
              "overrides": { "transitive": "^4.0.0" }
            }"#,
            Path::new("package.json"),
        )
        .unwrap();

        apply_manifest_root_metadata(&mut lock, &manifest).unwrap();

        assert_eq!(
            lock.root.dependencies.keys().collect::<Vec<_>>(),
            vec!["foo", "native", "tool"]
        );
        assert_eq!(
            lock.resolution.root.dev_dependencies.get("tool"),
            Some(&"^2.0.0".to_string())
        );
        assert_eq!(
            lock.resolution.root.optional_dependencies.get("native"),
            Some(&"^3.0.0".to_string())
        );
        assert_eq!(
            lock.resolution.root.overrides.get("transitive"),
            Some(&"^4.0.0".to_string())
        );
    }

    #[test]
    fn root_merges_dev_dependencies_into_declared_set() {
        // npm's package-lock v3 records devDependencies under the root `""`
        // entry. The frozen installer's drift check compares package.json's
        // (deps ∪ devDeps) against lockfile.root.dependencies, so the importer
        // must merge both into the root's declared dependency map.
        let json = r#"{
          "name": "app",
          "lockfileVersion": 3,
          "packages": {
            "": {
              "version": "1.0.0",
              "dependencies": { "foo": "^1.0.0" },
              "devDependencies": { "test-tool": "^9.0.0", "foo": "^1.0.0" }
            },
            "node_modules/foo": { "version": "1.0.0", "resolved": "https://example/foo.tgz", "integrity": "sha512-A" },
            "node_modules/test-tool": { "version": "9.0.0", "resolved": "https://example/t.tgz", "integrity": "sha512-B", "dev": true }
          }
        }"#;
        let report = import(json).unwrap();
        // Both production and dev deps are present in the root declared set.
        let root = &report.lockfile.root;
        assert!(root.dependencies.contains_key("foo"));
        assert!(root.dependencies.contains_key("test-tool"));
        // A name present in both resolves to its `dependencies` spec.
        assert_eq!(
            root.dependencies.get("foo").map(|s| s.as_str()),
            Some("^1.0.0")
        );
    }

    // ---- exporter ----

    use crate::lockfile::PackageEntry;

    fn registry_lockfile() -> (Lockfile, PackageManifest) {
        let mut lockfile = Lockfile::new("bpm");
        lockfile.root = RootEntry {
            name: Some("app".into()),
            version: Some("1.0.0".into()),
            dependencies: BTreeMap::from([("foo".into(), "^1.2.3".into())]),
        };
        lockfile.packages.push(PackageEntry {
            path: "node_modules/foo".into(),
            name: "foo".into(),
            version: "1.2.3".into(),
            resolved: "https://example/foo-1.2.3.tgz".into(),
            integrity: Some("sha512-AAA".into()),
            bin: BTreeMap::from([("foo".into(), "./cli.js".into())]),
            dependencies: BTreeMap::from([("bar".into(), "^4.0.0".into())]),
            ..Default::default()
        });
        lockfile.packages.push(PackageEntry {
            path: "node_modules/@scope/bar".into(),
            name: "@scope/bar".into(),
            version: "4.5.6".into(),
            resolved: "https://example/bar-4.5.6.tgz".into(),
            integrity: Some("sha512-BBB".into()),
            dev: true,
            ..Default::default()
        });
        lockfile.sort_packages();
        let manifest = PackageManifest::from_json(
            r#"{"name":"app","version":"1.0.0","dependencies":{"foo":"^1.2.3"}}"#,
            Path::new("package.json"),
        )
        .unwrap();
        (lockfile, manifest)
    }

    #[test]
    fn export_has_required_v3_top_level_fields() {
        let (lockfile, manifest) = registry_lockfile();
        let bytes = export_v3(&lockfile, &manifest).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let obj = value.as_object().unwrap();
        assert_eq!(obj.get("lockfileVersion").and_then(|v| v.as_u64()), Some(3));
        assert_eq!(obj.get("requires").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(obj.get("name").and_then(|v| v.as_str()), Some("app"));
        assert_eq!(obj.get("version").and_then(|v| v.as_str()), Some("1.0.0"));
        assert!(obj
            .get("packages")
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key(""));
        // BPM-only resolver metadata must never leak into npm v3 output.
        assert!(!obj.contains_key("resolution"));
        assert!(!obj.contains_key("generator"));
    }

    #[test]
    fn export_round_trips_through_import() {
        let (lockfile, manifest) = registry_lockfile();
        let bytes = export_v3(&lockfile, &manifest).unwrap();
        let report = import(std::str::from_utf8(&bytes).unwrap()).unwrap();
        let roundtrip = report.lockfile;

        let original_by_path: BTreeMap<&str, &PackageEntry> = lockfile
            .packages
            .iter()
            .map(|p| (p.path.as_str(), p))
            .collect();
        for package in &roundtrip.packages {
            let original = original_by_path
                .get(package.path.as_str())
                .unwrap_or_else(|| panic!("unexpected path {}", package.path));
            assert_eq!(package.name, original.name);
            assert_eq!(package.version, original.version);
            assert_eq!(package.resolved, original.resolved);
            assert_eq!(package.integrity, original.integrity);
            assert_eq!(package.bin, original.bin);
            assert_eq!(package.dev, original.dev);
            assert_eq!(package.optional, original.optional);
        }
        assert_eq!(roundtrip.packages.len(), lockfile.packages.len());
    }

    #[test]
    fn export_is_byte_stable_across_repeated_calls() {
        let (lockfile, manifest) = registry_lockfile();
        let first = export_v3(&lockfile, &manifest).unwrap();
        let second = export_v3(&lockfile, &manifest).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn export_rejects_unsupported_packages_in_sorted_order() {
        let mut lockfile = Lockfile::new("bpm");
        lockfile.packages.push(PackageEntry {
            path: "node_modules/linked".into(),
            name: "linked".into(),
            version: "1.0.0".into(),
            link: true,
            ..Default::default()
        });
        lockfile.packages.push(PackageEntry {
            path: "node_modules/missing-resolved".into(),
            name: "missing-resolved".into(),
            version: "1.0.0".into(),
            resolved: String::new(),
            integrity: Some("sha512-X".into()),
            ..Default::default()
        });
        lockfile.packages.push(PackageEntry {
            path: "node_modules/missing-integrity".into(),
            name: "missing-integrity".into(),
            version: "1.0.0".into(),
            resolved: "https://example/x.tgz".into(),
            integrity: None,
            ..Default::default()
        });
        lockfile.packages.push(PackageEntry {
            path: "node_modules/filedep".into(),
            name: "filedep".into(),
            version: "1.0.0".into(),
            resolved: "file:../x".into(),
            integrity: Some("sha512-X".into()),
            ..Default::default()
        });
        lockfile.sort_packages();
        let manifest = PackageManifest::from_json("{}", Path::new("package.json")).unwrap();

        let error = export_v3(&lockfile, &manifest).unwrap_err().to_string();

        let filedep = error.find("node_modules/filedep").unwrap();
        let link = error.find("node_modules/linked").unwrap();
        let missing_integrity = error.find("node_modules/missing-integrity").unwrap();
        let missing_resolved = error.find("node_modules/missing-resolved").unwrap();
        // Deterministic path-sorted order.
        assert!(filedep < link);
        assert!(link < missing_integrity);
        assert!(missing_integrity < missing_resolved);
        assert!(error.contains("non-registry source"));
        assert!(error.contains("workspace link"));
        assert!(error.contains("missing resolved URL"));
        assert!(error.contains("missing integrity"));
    }

    #[test]
    fn export_root_entry_includes_all_manifest_dependency_groups() {
        let mut lockfile = Lockfile::new("bpm");
        lockfile.packages.push(PackageEntry {
            path: "node_modules/foo".into(),
            name: "foo".into(),
            version: "1.0.0".into(),
            resolved: "https://example/foo.tgz".into(),
            integrity: Some("sha512-A".into()),
            ..Default::default()
        });
        let manifest = PackageManifest::from_json(
            r#"{"name":"app","version":"1.0.0",
            "dependencies":{"foo":"^1.0.0"},
            "devDependencies":{"tool":"^2.0.0"},
            "optionalDependencies":{"opt":"^3.0.0"},
            "peerDependencies":{"peer":"^4.0.0"}}"#,
            Path::new("package.json"),
        )
        .unwrap();
        let bytes = export_v3(&lockfile, &manifest).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let root = value
            .get("packages")
            .unwrap()
            .get("")
            .unwrap()
            .as_object()
            .unwrap();
        assert_eq!(
            root.get("dependencies")
                .unwrap()
                .get("foo")
                .unwrap()
                .as_str(),
            Some("^1.0.0")
        );
        assert_eq!(
            root.get("devDependencies")
                .unwrap()
                .get("tool")
                .unwrap()
                .as_str(),
            Some("^2.0.0")
        );
        assert_eq!(
            root.get("optionalDependencies")
                .unwrap()
                .get("opt")
                .unwrap()
                .as_str(),
            Some("^3.0.0")
        );
        assert_eq!(
            root.get("peerDependencies")
                .unwrap()
                .get("peer")
                .unwrap()
                .as_str(),
            Some("^4.0.0")
        );
    }
}
