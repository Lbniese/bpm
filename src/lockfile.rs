//! BPM lockfile (`bpm.lock`) — the authoritative, reviewable record of a
//! resolved dependency graph.
//!
//! `bpm.lock` is canonical JSON: packages are sorted by their `node_modules`
//! path and every dependency map is a `BTreeMap`, so serialization is stable
//! across hash-map iteration order, locale, and machine. The format is
//! produced by [`crate::npm_lock`] import and consumed by the frozen installer.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// `bpm.lock` schema version this implementation writes and reads.
pub const BPM_LOCK_VERSION: u32 = 1;
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
        }
    }

    /// Sort packages by path so serialization is canonical.
    pub fn sort_packages(&mut self) {
        self.packages.sort_by(|a, b| a.path.cmp(&b.path));
    }

    /// Canonical pretty-printed JSON. Deterministic regardless of insertion
    /// order: struct fields emit in declaration order and `BTreeMap` values
    /// emit sorted-key order, and [`Self::sort_packages`] must be called first.
    pub fn to_json(&self) -> Result<String, LockfileError> {
        let json = serde_json::to_string_pretty(self)?;
        Ok(json)
    }

    /// Parse canonical JSON back into a [`Lockfile`].
    pub fn from_json(json: &str) -> Result<Self, LockfileError> {
        Ok(serde_json::from_str(json)?)
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
}
