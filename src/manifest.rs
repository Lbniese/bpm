//! `package.json` parsing.
//!
//! Parses a project manifest into typed, deterministic structures. Dependency
//! maps use `BTreeMap` so iteration order and serialization are stable.
//!
//! Parsing is permissive: many real manifests (notably workspace roots) omit
//! `name`/`version`. Missing or unusual fields are reported as diagnostics by
//! `bpm doctor` rather than rejected here. Only unrecoverable IO or JSON syntax
//! errors are returned from [`PackageManifest::from_path`].

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer};
use serde_json::Value;
use thiserror::Error;

/// Error while reading or parsing a manifest. Carries the source path so the
/// caller can produce an actionable message without re-deriving it.
#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("cannot read package.json at {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid JSON in package.json at {path}: {source}")]
    Parse {
        path: String,
        source: serde_json::Error,
    },
}

/// `bin` may be a single path string or a map of name -> path.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum BinField {
    Map(BTreeMap<String, String>),
    One(String),
}

/// `workspaces` may be an array of globs or an object with `packages`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Workspaces {
    Patterns(Vec<String>),
    Config {
        packages: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        nohoist: Option<Vec<String>>,
    },
}

impl Workspaces {
    /// The resolved package-glob patterns, regardless of declaration shape.
    pub fn patterns(&self) -> &[String] {
        match self {
            Workspaces::Patterns(p) => p,
            Workspaces::Config { packages, .. } => packages,
        }
    }
}

/// A dependency request using BPM's explicit `workspace:` protocol extension.
///
/// npm workspace declarations and ordinary dependency ranges remain strings in
/// the manifest. Resolver code can use this type to distinguish the supported
/// local-workspace requests without re-parsing protocol spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceSpec {
    Any,
    Caret,
    Tilde,
    Range(String),
}

impl WorkspaceSpec {
    /// Parse a `workspace:` dependency request, returning `None` for other
    /// dependency protocols.
    pub fn parse(spec: &str) -> Option<Self> {
        let payload = spec.strip_prefix("workspace:")?;
        Some(match payload {
            "*" => Self::Any,
            "^" => Self::Caret,
            "~" => Self::Tilde,
            range => Self::Range(range.to_owned()),
        })
    }
}

/// A parsed `package.json`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PackageManifest {
    #[serde(skip)]
    pub source_dir: Option<PathBuf>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub private: Option<bool>,
    #[serde(default, rename = "type")]
    pub module_type: Option<String>,
    #[serde(default)]
    pub bin: Option<BinField>,
    #[serde(default)]
    pub scripts: BTreeMap<String, String>,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    pub dev_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "peerDependencies")]
    pub peer_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    pub optional_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "peerDependenciesMeta")]
    pub peer_dependencies_meta: BTreeMap<String, PeerMeta>,
    #[serde(default)]
    pub workspaces: Option<Workspaces>,
    #[serde(default)]
    pub engines: BTreeMap<String, String>,
    #[serde(default)]
    pub overrides: BTreeMap<String, Value>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub os: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub cpu: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub libc: Vec<String>,
}

/// `peerDependenciesMeta` entry. Only `optional` is tracked for this milestone.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PeerMeta {
    #[serde(default)]
    pub optional: bool,
}

/// Deserialize npm platform constraints from either the documented array form
/// or the string form found in registry metadata, then canonicalize ordering.
fn deserialize_string_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringList {
        One(String),
        Many(Vec<String>),
    }

    let mut values = match StringList::deserialize(deserializer)? {
        StringList::One(value) => vec![value],
        StringList::Many(values) => values,
    };
    values.sort();
    values.dedup();
    Ok(values)
}

/// Whether `name` is a valid npm package name (scoped or unscoped).
///
/// Accepts an optional `@scope/` prefix followed by one or more segments
/// where each segment matches `[a-z0-9._-]+`. This is a deliberate subset of
/// npm's rules; exotic historical names are out of scope for the first
/// milestone and will be reported as diagnostics by `bpm doctor`.
pub fn is_valid_package_name(name: &str) -> bool {
    if name.is_empty() || name.starts_with('.') || name.starts_with('_') {
        return false;
    }
    let (scope, rest) = match name.split_once('@') {
        // No '@' anywhere: a plain unscoped name.
        None => return segment_ok(name),
        // Leading '@' introduces a scope.
        Some(("", rest)) => (Some(()), rest),
        // '@' inside the name is invalid.
        _ => return false,
    };
    let Some((scope_name, package_name)) = rest.split_once('/') else {
        return false;
    };
    scope.is_some() && segment_ok(scope_name) && segment_ok(package_name)
}

/// A single name segment: non-empty, lowercase ASCII alphanumeric plus `-`, `_`, `.`.
fn segment_ok(seg: &str) -> bool {
    !seg.is_empty()
        && seg.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_')
        })
}

impl PackageManifest {
    /// Parse a manifest from a file path.
    pub fn from_path(path: &Path) -> Result<Self, ManifestError> {
        let path_str = path.display().to_string();
        let contents = fs::read_to_string(path).map_err(|source| ManifestError::Read {
            path: path_str.clone(),
            source,
        })?;
        Self::from_json(&contents, path)
    }

    /// Parse a manifest from a JSON string, tagging errors with `path`.
    pub fn from_json(contents: &str, path: &Path) -> Result<Self, ManifestError> {
        let mut manifest: Self =
            serde_json::from_str(contents).map_err(|source| ManifestError::Parse {
                path: path.display().to_string(),
                source,
            })?;
        manifest.source_dir = path.parent().map(Path::to_path_buf);
        Ok(manifest)
    }

    /// Merge all root dependency declarations using npm's precedence rules.
    /// Optional dependencies replace a same-name regular dependency; peer and
    /// dev declarations only fill names not already declared by a stronger
    /// root dependency group. This is the canonical root declaration set used
    /// by native resolution, import enrichment, and frozen drift validation.
    pub fn root_dependency_declarations(&self) -> BTreeMap<String, String> {
        let mut declarations = self.dependencies.clone();
        for (name, spec) in &self.optional_dependencies {
            declarations.insert(name.clone(), spec.clone());
        }
        for (name, spec) in &self.peer_dependencies {
            declarations
                .entry(name.clone())
                .or_insert_with(|| spec.clone());
        }
        for (name, spec) in &self.dev_dependencies {
            declarations
                .entry(name.clone())
                .or_insert_with(|| spec.clone());
        }
        declarations
    }

    /// Total count of non-workspace dependencies across all sections.
    pub fn dependency_count(&self) -> usize {
        self.dependencies.len()
            + self.dev_dependencies.len()
            + self.peer_dependencies.len()
            + self.optional_dependencies.len()
    }

    /// Number of executable `bin` entries, if any.
    pub fn bin_count(&self) -> usize {
        match &self.bin {
            None => 0,
            Some(BinField::One(_)) => 1,
            Some(BinField::Map(map)) => map.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST_PATH: &str = "package.json";

    fn parse(json: &str) -> PackageManifest {
        PackageManifest::from_json(json, Path::new(MANIFEST_PATH)).expect("valid manifest")
    }

    #[test]
    fn parses_minimal_manifest() {
        let m = parse(r#"{"name":"app","version":"1.2.3"}"#);
        assert_eq!(m.name.as_deref(), Some("app"));
        assert_eq!(m.version.as_deref(), Some("1.2.3"));
        assert!(m.dependencies.is_empty());
    }

    #[test]
    fn empty_manifest_is_valid() {
        // A workspace root may legitimately have no name/version.
        let m = parse("{}");
        assert!(m.name.is_none());
        assert!(m.version.is_none());
        assert_eq!(m.dependency_count(), 0);
    }

    #[test]
    fn parses_dependencies_in_stable_order() {
        let m = parse(
            r#"{"name":"app","dependencies":{"zebra":"^1.0.0","apple":"^2.0.0"},
            "devDependencies":{"dev-x":"^9.0.0"},
            "peerDependencies":{"peer":"^1.0.0"},
            "optionalDependencies":{"opt":"^1.0.0"}}"#,
        );
        assert_eq!(
            m.dependencies.keys().collect::<Vec<_>>(),
            vec!["apple", "zebra"]
        );
        assert_eq!(m.dependency_count(), 5);
    }

    #[test]
    fn parses_bin_as_string_or_map() {
        let one = parse(r#"{"name":"a","bin":"./cli.js"}"#);
        assert_eq!(one.bin_count(), 1);

        let map = parse(r#"{"name":"a","bin":{"a":"./a.js","b":"./b.js"}}"#);
        assert_eq!(map.bin_count(), 2);
    }

    #[test]
    fn parses_workspaces_patterns_and_config() {
        let patterns = parse(r#"{"name":"root","workspaces":["packages/*"]}"#);
        assert_eq!(patterns.workspaces.unwrap().patterns(), ["packages/*"]);

        let config = parse(r#"{"name":"root","workspaces":{"packages":["apps/*","libs/*"]}}"#);
        assert_eq!(config.workspaces.unwrap().patterns(), ["apps/*", "libs/*"]);
    }

    #[test]
    fn parses_scripts_and_engines() {
        let m = parse(
            r#"{"name":"a","scripts":{"build":"tsc","test":"jest"},
            "engines":{"node":">=18","npm":">=9"}}"#,
        );
        assert_eq!(m.scripts.keys().collect::<Vec<_>>(), vec!["build", "test"]);
        assert_eq!(m.engines.get("node").unwrap(), ">=18");
    }

    #[test]
    fn parses_overrides_and_peer_meta() {
        let m = parse(
            r#"{"name":"a","overrides":{"lodash":"^4.0.0","foo":{"bar":"2"}},
            "peerDependenciesMeta":{"react":{"optional":true},"react-dom":{}}}"#,
        );
        assert_eq!(m.overrides.get("lodash"), Some(&Value::from("^4.0.0")));
        assert_eq!(m.overrides["foo"]["bar"], Value::from("2"));
        assert!(m.peer_dependencies_meta.get("react").unwrap().optional);
        assert!(!m.peer_dependencies_meta.get("react-dom").unwrap().optional);
    }

    #[test]
    fn parses_and_canonicalizes_platform_constraints() {
        let m = parse(
            r#"{"name":"a","os":["linux","!win32","linux"],"cpu":"arm64",
            "libc":["musl","glibc"]}"#,
        );
        assert_eq!(m.os, ["!win32", "linux"]);
        assert_eq!(m.cpu, ["arm64"]);
        assert_eq!(m.libc, ["glibc", "musl"]);
    }

    #[test]
    fn parses_workspace_dependency_specs_without_changing_other_specs() {
        assert_eq!(
            WorkspaceSpec::parse("workspace:*"),
            Some(WorkspaceSpec::Any)
        );
        assert_eq!(
            WorkspaceSpec::parse("workspace:^"),
            Some(WorkspaceSpec::Caret)
        );
        assert_eq!(
            WorkspaceSpec::parse("workspace:~"),
            Some(WorkspaceSpec::Tilde)
        );
        assert_eq!(
            WorkspaceSpec::parse("workspace:>=1 <2"),
            Some(WorkspaceSpec::Range(">=1 <2".into()))
        );
        assert_eq!(WorkspaceSpec::parse("^1.0.0"), None);
    }

    #[test]
    fn rejects_malformed_json() {
        let err = PackageManifest::from_json("{ not json", Path::new("package.json"))
            .expect_err("malformed JSON should fail");
        assert!(matches!(err, ManifestError::Parse { .. }));
    }

    #[test]
    fn validates_package_names() {
        assert!(is_valid_package_name("react"));
        assert!(is_valid_package_name("@scope/react"));
        assert!(is_valid_package_name("left-pad"));
        assert!(!is_valid_package_name(""));
        assert!(!is_valid_package_name("@scope"));
        assert!(!is_valid_package_name("Bad Name"));
        assert!(!is_valid_package_name("_secret"));
    }
}
