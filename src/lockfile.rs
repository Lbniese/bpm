//! BPM lockfile (`bpm.lock`) — the authoritative, reviewable record of a
//! resolved dependency graph.
//!
//! `bpm.lock` is canonical JSON: packages are sorted by their `node_modules`
//! path and every dependency map is a `BTreeMap`, so serialization is stable
//! across hash-map iteration order, locale, and machine. The format is
//! produced by [`crate::npm_lock`] import and consumed by the frozen installer.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// `bpm.lock` schema version this implementation writes.
pub const BPM_LOCK_VERSION: u32 = 2;
const LEGACY_LOCK_VERSION: u32 = 1;
/// Default output filename, written next to the imported lockfile.
pub const BPM_LOCK_FILE: &str = "bpm.lock";

/// A complete resolved lockfile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Lockfile {
    pub lockfile_version: u32,
    pub generator: String,
    pub root: RootEntry,
    pub packages: Vec<PackageEntry>,
    /// Resolver-only semantics added in v2. Keeping these values in a keyed
    /// section lets existing import/materialization callers continue building
    /// the stable v1 package records while the native resolver supplies the
    /// additional graph identity facts.
    #[serde(default, skip_serializing_if = "ResolutionMetadata::is_empty")]
    pub resolution: ResolutionMetadata,
}

/// Canonical resolver metadata that cannot be reconstructed from tarballs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResolutionMetadata {
    #[serde(default)]
    pub root: RootResolution,
    /// Package path to resolver semantics. `BTreeMap` fixes output order.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub packages: BTreeMap<String, PackageResolution>,
}

impl ResolutionMetadata {
    fn is_empty(&self) -> bool {
        self.root == RootResolution::default() && self.packages.is_empty()
    }
}

/// Root resolution inputs retained separately rather than flattening npm's
/// dependency groups into the compatibility `RootEntry.dependencies` map.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RootResolution {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dev_dependencies: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub optional_dependencies: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overrides: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_patterns: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_index_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<LockTarget>,
    #[serde(default)]
    pub peer_mode: PeerMode,
}

/// npm platform identity used for resolution, not Rust target spelling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockTarget {
    pub os: String,
    pub cpu: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub libc: Option<String>,
}

/// Peer dependency behavior is part of graph identity.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PeerMode {
    #[default]
    Strict,
    LegacyIgnore,
}

/// Source of a physical package placement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "type"
)]
pub enum LockSource {
    Registry {
        registry: String,
    },
    Workspace {
        relative_path: String,
    },
    File {
        path: String,
    },
    Tarball {
        url: String,
    },
    Git {
        url: String,
        reference: Option<String>,
    },
    Patch {
        source: Box<LockSource>,
        patch: String,
    },
}

/// Exact visible provider bound to a peer name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PeerProvider {
    pub name: String,
    pub version: String,
    pub source: LockSource,
    pub path: String,
}

/// One effective dependency request and its physical placement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LockDependency {
    pub spec: String,
    pub target: String,
}

/// Resolver semantics for one `PackageEntry`, keyed by that entry's path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PackageResolution {
    pub source: LockSource,
    #[serde(default, skip_serializing_if = "is_false")]
    pub dev_optional: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub peer: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub libc: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, LockDependency>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub optional_dependencies: BTreeMap<String, LockDependency>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub peer_dependencies: BTreeMap<String, LockDependency>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub optional_peers: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub peer_context: BTreeMap<String, PeerProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_target: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub has_install_script: bool,
}

/// The project root entry (the `""` package in npm v3 terminology).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RootEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Declared root dependency specs (`name -> semver range`), sorted.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, String>,
}

/// A single resolved package placement in the `node_modules` tree.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PackageEntry {
    /// Full `node_modules/...` path (the npm v3 package key).
    pub path: String,
    /// Package name (`@scope/name` or `name`).
    pub name: String,
    pub version: String,
    /// Registry tarball URL; empty for link/workspace entries.
    #[serde(default)]
    pub resolved: String,
    /// Project-relative target for a workspace link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_target: Option<String>,
    /// npm integrity string (`sha512-...`) when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,
    /// `true` for symlink/workspace/file entries (not yet materialized).
    #[serde(default, skip_serializing_if = "is_false")]
    pub link: bool,
    /// `true` for dev-only packages.
    #[serde(default, skip_serializing_if = "is_false")]
    pub dev: bool,
    /// `true` for optional packages.
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
    /// `os` constraints, if declared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub os: Vec<String>,
    /// `cpu` constraints, if declared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cpu: Vec<String>,
    /// Declared executables (`bin name -> relative path within package`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bin: BTreeMap<String, String>,
    /// Declared dependency specs (`name -> semver range`), sorted.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, String>,
}

fn is_false(b: &bool) -> bool {
    !b
}

/// Errors reading or writing a `bpm.lock`.
#[derive(Debug, Error)]
pub enum LockfileError {
    #[error("failed to read lockfile {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse lockfile: {0}")]
    Parse(#[from] serde_json::Error),
    #[error(
        "unsupported bpm.lock version {found}: this BPM supports versions {LEGACY_LOCK_VERSION} and {BPM_LOCK_VERSION}"
    )]
    UnsupportedVersion { found: u32 },
    #[error("invalid bpm.lock v{BPM_LOCK_VERSION}: {0}")]
    Invalid(String),
    #[error("failed to write lockfile {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Lockfile {
    /// Create an empty lockfile shell with the correct generator tag.
    pub fn new(generator: impl Into<String>) -> Self {
        Lockfile {
            lockfile_version: BPM_LOCK_VERSION,
            generator: generator.into(),
            root: RootEntry::default(),
            packages: Vec::new(),
            resolution: ResolutionMetadata::default(),
        }
    }

    /// Sort packages by path so serialization is canonical.
    pub fn sort_packages(&mut self) {
        self.packages.sort_by(|a, b| a.path.cmp(&b.path));
    }

    /// Canonical pretty-printed JSON. Deterministic regardless of insertion
    /// order: struct fields emit in declaration order, maps emit sorted-key
    /// order, and package paths/workspace patterns are normalized here.
    pub fn to_json(&self) -> Result<String, LockfileError> {
        let mut canonical = self.clone();
        canonical.sort_packages();
        canonical.root_resolution_lists();
        let json = serde_json::to_string_pretty(&canonical)?;
        Ok(json)
    }

    /// Parse canonical JSON back into a [`Lockfile`].
    pub fn from_json(json: &str) -> Result<Self, LockfileError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct VersionProbe {
            #[serde(default)]
            lockfile_version: u32,
        }

        let version = serde_json::from_str::<VersionProbe>(json)?.lockfile_version;
        if version != LEGACY_LOCK_VERSION && version != BPM_LOCK_VERSION {
            return Err(LockfileError::UnsupportedVersion { found: version });
        }

        let mut lockfile: Self = serde_json::from_str(json)?;
        if version == LEGACY_LOCK_VERSION {
            // Migration is deliberately in memory only. `from_path` never
            // writes, so frozen installs leave the legacy bytes untouched.
            lockfile.lockfile_version = BPM_LOCK_VERSION;
            lockfile.resolution = ResolutionMetadata::default();
            return Ok(lockfile);
        }

        lockfile.validate_v2()?;
        Ok(lockfile)
    }

    /// Read and parse a `bpm.lock` from disk.
    pub fn from_path(path: &Path) -> Result<Self, LockfileError> {
        let s = std::fs::read_to_string(path).map_err(|source| LockfileError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_json(&s)
    }

    /// Write canonical JSON to disk atomically: write to a sibling temp file,
    /// then rename over the destination.
    pub fn write_to(&self, path: &Path) -> Result<(), LockfileError> {
        let mut json = self.to_json()?;
        json.push('\n');
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|source| LockfileError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
        let tmp = parent.join(format!(
            ".{}.tmp",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("bpm.lock")
        ));
        std::fs::write(&tmp, json.as_bytes()).map_err(|source| LockfileError::Write {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, path).map_err(|source| LockfileError::Write {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(())
    }

    fn root_resolution_lists(&mut self) {
        self.resolution.root.workspace_patterns.sort();
        self.resolution.root.workspace_patterns.dedup();
        for package in self.resolution.packages.values_mut() {
            package.libc.sort();
            package.libc.dedup();
        }
    }

    fn validate_v2(&self) -> Result<(), LockfileError> {
        let mut previous: Option<&str> = None;
        let mut paths = BTreeSet::new();
        for package in &self.packages {
            if !legal_relative_path(&package.path) {
                return Err(LockfileError::Invalid(format!(
                    "package path {:?} must be a normalized relative path",
                    package.path
                )));
            }
            if previous.is_some_and(|value| value >= package.path.as_str()) {
                return Err(LockfileError::Invalid(format!(
                    "package paths must be unique and sorted; {:?} is out of order",
                    package.path
                )));
            }
            previous = Some(&package.path);
            paths.insert(package.path.as_str());
        }

        for (path, metadata) in &self.resolution.packages {
            let Some(package) = self.packages.iter().find(|entry| entry.path == *path) else {
                return Err(LockfileError::Invalid(format!(
                    "resolution metadata refers to unknown package path {path:?}"
                )));
            };
            match &metadata.source {
                LockSource::Registry { registry } => {
                    if registry.is_empty()
                        || package.resolved.is_empty()
                        || package.integrity.as_deref().is_none_or(str::is_empty)
                    {
                        return Err(LockfileError::Invalid(format!(
                            "registry package {path:?} requires registry, resolved, and integrity"
                        )));
                    }
                }
                LockSource::Workspace { relative_path } => {
                    if !package.link
                        || metadata.workspace_target.as_deref() != Some(relative_path.as_str())
                        || !legal_relative_path(relative_path)
                    {
                        return Err(LockfileError::Invalid(format!(
                            "workspace package {path:?} requires a matching legal workspaceTarget and link=true"
                        )));
                    }
                }
                LockSource::File { path: source_path } => {
                    if !package.link
                        || metadata.workspace_target.as_deref() != Some(source_path.as_str())
                    {
                        return Err(LockfileError::Invalid(format!(
                            "file package {path:?} requires a matching workspaceTarget and link=true"
                        )));
                    }
                }
                LockSource::Tarball { url } => {
                    if url.is_empty() || package.resolved.is_empty() {
                        return Err(LockfileError::Invalid(format!(
                            "tarball package {path:?} requires source URL and resolved tarball URL"
                        )));
                    }
                }
                LockSource::Git { url, .. } => {
                    if url.is_empty() || package.resolved.is_empty() {
                        return Err(LockfileError::Invalid(format!(
                            "git package {path:?} requires source URL and resolved tarball URL"
                        )));
                    }
                }
                LockSource::Patch { patch, .. } => {
                    if patch.is_empty()
                        || package.resolved.is_empty()
                        || package.integrity.as_deref().is_none_or(str::is_empty)
                    {
                        return Err(LockfileError::Invalid(format!(
                            "patched package {path:?} requires patch path, resolved tarball URL, and integrity"
                        )));
                    }
                }
            }
            validate_targets(path, "dependencies", &metadata.dependencies, &paths)?;
            validate_targets(
                path,
                "optionalDependencies",
                &metadata.optional_dependencies,
                &paths,
            )?;
            validate_targets(
                path,
                "peerDependencies",
                &metadata.peer_dependencies,
                &paths,
            )?;
            for (peer, provider) in &metadata.peer_context {
                if !paths.contains(provider.path.as_str()) {
                    return Err(LockfileError::Invalid(format!(
                        "peer context {peer:?} for {path:?} targets missing path {:?}",
                        provider.path
                    )));
                }
            }
        }
        Ok(())
    }
}

fn validate_targets(
    package: &str,
    group: &str,
    dependencies: &BTreeMap<String, LockDependency>,
    paths: &BTreeSet<&str>,
) -> Result<(), LockfileError> {
    for (name, dependency) in dependencies {
        if !paths.contains(dependency.target.as_str()) {
            return Err(LockfileError::Invalid(format!(
                "{group} entry {name:?} for {package:?} targets missing path {:?}",
                dependency.target
            )));
        }
    }
    Ok(())
}

fn legal_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.starts_with('\\')
        && !path.contains('\\')
        && !path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        && path.as_bytes().get(1).is_none_or(|byte| *byte != b':')
}

/// Look upward from `start` for the nearest `bpm.lock` and parse it.
pub fn find_lockfile(start: &Path) -> Result<Option<(PathBuf, Lockfile)>, LockfileError> {
    let mut dir: Option<&Path> = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(BPM_LOCK_FILE);
        if candidate.is_file() {
            let lf = Lockfile::from_path(&candidate)?;
            return Ok(Some((candidate, lf)));
        }
        dir = d.parent();
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Lockfile {
        let mut lf = Lockfile::new("bpm");
        lf.root = RootEntry {
            name: Some("app".into()),
            version: Some("1.0.0".into()),
            dependencies: BTreeMap::from([("foo".into(), "^1.0.0".into())]),
        };
        lf.packages.push(PackageEntry {
            path: "node_modules/zoo".into(),
            name: "zoo".into(),
            version: "2.0.0".into(),
            resolved: "https://example/zoo-2.0.0.tgz".into(),
            integrity: Some("sha512-abc".into()),
            ..Default::default()
        });
        lf.packages.push(PackageEntry {
            path: "node_modules/foo".into(),
            name: "foo".into(),
            version: "1.0.0".into(),
            resolved: "https://example/foo-1.0.0.tgz".into(),
            integrity: Some("sha512-def".into()),
            bin: BTreeMap::from([("foocli".into(), "./cli.js".into())]),
            dependencies: BTreeMap::from([("zoo".into(), "^2.0.0".into())]),
            ..Default::default()
        });
        lf.sort_packages();
        lf
    }

    #[test]
    fn roundtrip_is_stable() {
        let lf = sample();
        let json = lf.to_json().unwrap();
        let back = Lockfile::from_json(&json).unwrap();
        assert_eq!(lf, back);
        let json2 = back.to_json().unwrap();
        assert_eq!(json, json2, "serialization is not canonical");
    }

    #[test]
    fn packages_are_sorted_by_path() {
        let lf = sample();
        let paths: Vec<&str> = lf.packages.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(paths, vec!["node_modules/foo", "node_modules/zoo"]);
    }

    #[test]
    fn determinism_independent_of_construction_order() {
        // Same logical content, built by pushing packages in reverse order.
        let mut lf = Lockfile::new("bpm");
        lf.packages.push(PackageEntry {
            path: "node_modules/zoo".into(),
            name: "zoo".into(),
            version: "2.0.0".into(),
            ..Default::default()
        });
        lf.packages.push(PackageEntry {
            path: "node_modules/foo".into(),
            name: "foo".into(),
            version: "1.0.0".into(),
            ..Default::default()
        });
        lf.sort_packages();
        let mut other = Lockfile::new("bpm");
        other.packages.push(PackageEntry {
            path: "node_modules/foo".into(),
            name: "foo".into(),
            version: "1.0.0".into(),
            ..Default::default()
        });
        other.packages.push(PackageEntry {
            path: "node_modules/zoo".into(),
            name: "zoo".into(),
            version: "2.0.0".into(),
            ..Default::default()
        });
        other.sort_packages();
        assert_eq!(lf.to_json().unwrap(), other.to_json().unwrap());
    }

    #[test]
    fn reads_v1_into_v2_compatible_memory_without_rewriting_source() {
        let legacy = r#"{
  "lockfileVersion": 1,
  "generator": "bpm-legacy",
  "root": { "dependencies": { "foo": "1" } },
  "packages": [{
    "path": "node_modules/foo",
    "name": "foo",
    "version": "1.0.0",
    "resolved": "https://registry.example/foo.tgz",
    "integrity": "sha512-abc"
  }]
}"#;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(BPM_LOCK_FILE);
        std::fs::write(&path, legacy).unwrap();

        let lockfile = Lockfile::from_path(&path).unwrap();

        assert_eq!(lockfile.lockfile_version, BPM_LOCK_VERSION);
        assert!(lockfile.resolution.is_empty());
        assert_eq!(std::fs::read_to_string(path).unwrap(), legacy);
    }

    #[test]
    fn rejects_missing_zero_and_future_versions() {
        for (json, found) in [
            (r#"{"generator":"bpm","root":{},"packages":[]}"#, 0),
            (
                r#"{"lockfileVersion":0,"generator":"bpm","root":{},"packages":[]}"#,
                0,
            ),
            (
                r#"{"lockfileVersion":3,"generator":"bpm","root":{},"packages":[]}"#,
                3,
            ),
        ] {
            assert!(matches!(
                Lockfile::from_json(json),
                Err(LockfileError::UnsupportedVersion { found: actual }) if actual == found
            ));
        }
    }

    #[test]
    fn v2_roundtrip_preserves_every_resolver_semantic_canonically() {
        let mut lockfile = sample();
        lockfile
            .root
            .dependencies
            .insert("optional".into(), "^3".into());
        lockfile.resolution.root = RootResolution {
            dev_dependencies: BTreeMap::from([("test-runner".into(), "2".into())]),
            optional_dependencies: BTreeMap::from([("optional".into(), "^3".into())]),
            overrides: BTreeMap::from([("zoo".into(), "2.0.0".into())]),
            workspace_patterns: vec!["packages/*".into(), "apps/*".into(), "apps/*".into()],
            workspace_index_digest: Some("blake3:index".into()),
            target: Some(LockTarget {
                os: "linux".into(),
                cpu: "x64".into(),
                libc: Some("glibc".into()),
            }),
            peer_mode: PeerMode::Strict,
        };
        lockfile.resolution.packages.insert(
            "node_modules/foo".into(),
            PackageResolution {
                source: LockSource::Registry {
                    registry: "https://registry.example/".into(),
                },
                dev_optional: true,
                peer: true,
                libc: vec!["musl".into(), "glibc".into(), "glibc".into()],
                dependencies: BTreeMap::from([(
                    "zoo".into(),
                    LockDependency {
                        spec: "^2".into(),
                        target: "node_modules/zoo".into(),
                    },
                )]),
                optional_dependencies: BTreeMap::new(),
                peer_dependencies: BTreeMap::from([(
                    "zoo".into(),
                    LockDependency {
                        spec: "2.x".into(),
                        target: "node_modules/zoo".into(),
                    },
                )]),
                optional_peers: BTreeSet::from(["missing-host".into()]),
                peer_context: BTreeMap::from([(
                    "zoo".into(),
                    PeerProvider {
                        name: "zoo".into(),
                        version: "2.0.0".into(),
                        source: LockSource::Registry {
                            registry: "https://registry.example/".into(),
                        },
                        path: "node_modules/zoo".into(),
                    },
                )]),
                workspace_target: None,
                has_install_script: true,
            },
        );

        let json = lockfile.to_json().unwrap();
        let roundtrip = Lockfile::from_json(&json).unwrap();

        assert_eq!(
            roundtrip.resolution.root.workspace_patterns,
            ["apps/*", "packages/*"]
        );
        assert_eq!(
            roundtrip.resolution.packages["node_modules/foo"].libc,
            ["glibc", "musl"]
        );
        assert!(roundtrip.resolution.packages["node_modules/foo"].has_install_script);
        assert_eq!(json, roundtrip.to_json().unwrap());
    }

    #[test]
    fn v2_rejects_unsorted_paths_and_dangling_edge_targets() {
        let unsorted = r#"{
          "lockfileVersion": 2,
          "generator": "bpm",
          "root": {},
          "packages": [
            {"path":"node_modules/z","name":"z","version":"1"},
            {"path":"node_modules/a","name":"a","version":"1"}
          ]
        }"#;
        assert!(matches!(
            Lockfile::from_json(unsorted),
            Err(LockfileError::Invalid(message)) if message.contains("sorted")
        ));

        let dangling = r#"{
          "lockfileVersion": 2,
          "generator": "bpm",
          "root": {},
          "packages": [{
            "path":"node_modules/a","name":"a","version":"1",
            "resolved":"https://registry.example/a.tgz","integrity":"sha512-a"
          }],
          "resolution": {
            "packages": {
              "node_modules/a": {
                "source":{"type":"registry","registry":"https://registry.example/"},
                "dependencies":{"missing":{"spec":"1","target":"node_modules/missing"}}
              }
            }
          }
        }"#;
        assert!(matches!(
            Lockfile::from_json(dangling),
            Err(LockfileError::Invalid(message)) if message.contains("missing path")
        ));
    }

    #[test]
    fn v2_workspace_entries_require_matching_targets() {
        let invalid = r#"{
          "lockfileVersion": 2,
          "generator": "bpm",
          "root": {},
          "packages": [{
            "path":"node_modules/app","name":"app","version":"1","link":true
          }],
          "resolution": {
            "packages": {
              "node_modules/app": {
                "source":{"type":"workspace","relativePath":"packages/app"},
                "workspaceTarget":"packages/other"
              }
            }
          }
        }"#;
        assert!(matches!(
            Lockfile::from_json(invalid),
            Err(LockfileError::Invalid(message)) if message.contains("workspaceTarget")
        ));
    }
}
