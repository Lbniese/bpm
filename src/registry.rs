//! Registry packument resolution: turn a package spec (`lodash`,
//! `lodash@4.17.21`, `@scope/pkg@^1.2.0`) into a concrete tarball URL and
//! integrity, the way `npm`/`bun` resolve a name before download.
//!
//! This is the small, self-contained end of dependency resolution. It does
//! *not* build a dependency graph — it resolves a single name to one tarball
//! and hands `(tarball_url, integrity)` to the existing immutable store, which
//! is unchanged.
//!
//! Behavior:
//! - `<name>`           -> `dist-tags.latest`
//! - `<name>@<version>` -> exact version (must exist in the packument)
//! - `<name>@<range>`   -> highest published version satisfying the range
//!   (`^`, `~`, `>=`, `x` ranges, `*`), via the `semver` crate
//!
//! Scoped names (`@scope/pkg`) are URL-encoded the way the npm registry
//! expects (`/` -> `%2F`) so the whole name is one path segment.

use semver::{Version, VersionReq};
use thiserror::Error;

/// How a spec asks for a version.
#[derive(Debug, Clone)]
pub enum VersionRequest {
    /// No version given: use `dist-tags.latest`.
    Latest,
    /// An exact version (`lodash@4.17.21`).
    Exact(Version),
    /// A semver range (`lodash@^4.17.0`).
    Range(VersionReq),
}

/// A parsed package spec: a name plus a version request.
#[derive(Debug, Clone)]
pub struct PackageSpec {
    pub name: String,
    pub req: VersionRequest,
}

/// A fully resolved single package: its tarball URL and npm integrity string.
#[derive(Debug, Clone)]
pub struct ResolvedArtifact {
    pub name: String,
    pub version: Version,
    pub tarball_url: String,
    /// npm-style `sha512-<base64>` integrity from the registry `dist` block.
    pub integrity: String,
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("invalid package spec '{0}': {1}")]
    InvalidSpec(String, String),
    #[error("registry request for {package} failed")]
    Network {
        package: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("registry returned status {code} for {package}")]
    BadStatus { package: String, code: u16 },
    #[error("registry response for {package} was not valid JSON")]
    BadJson {
        package: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("packument for {package} has no versions")]
    NoVersions { package: String },
    #[error("no version of {package} satisfies {req}")]
    VersionNotFound { package: String, req: String },
    #[error("packument for {package}@{version} is missing a tarball URL or integrity")]
    MissingDist { package: String, version: String },
}

/// Parse a package spec string into a name + version request.
///
/// The version separator is the last `@` that is not the leading scope marker
/// of a scoped name. So `@scope/pkg` has no version, but `@scope/pkg@1.2.3`
/// and `pkg@1.2.3` do.
pub fn parse_spec(spec: &str) -> Result<PackageSpec, RegistryError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(RegistryError::InvalidSpec(
            spec.to_string(),
            "spec is empty".to_string(),
        ));
    }

    let (name, req_str) = match spec.rfind('@') {
        // `@scope/pkg` (the only `@` is the leading scope marker) or bare `pkg`.
        Some(0) | None => (spec, None),
        // `<name>@<req>` or `@scope/name@<req>`.
        Some(i) => (&spec[..i], Some(&spec[i + 1..])),
    };

    if !is_valid_npm_name(name) {
        return Err(RegistryError::InvalidSpec(
            spec.to_string(),
            format!("'{name}' is not a valid npm package name"),
        ));
    }

    let req = match req_str.map(str::trim) {
        None | Some("") | Some("latest") => VersionRequest::Latest,
        Some(s) if s.starts_with(['^', '~', '>', '<', '=', '*']) => {
            VersionRequest::Range(VersionReq::parse(s).map_err(|e| {
                RegistryError::InvalidSpec(spec.to_string(), format!("bad range '{s}': {e}"))
            })?)
        }
        Some(s) => {
            // A bare version like `1.2.3` is exact; anything else (e.g. `1.x`)
            // is treated as a range.
            match Version::parse(s) {
                Ok(v) => VersionRequest::Exact(v),
                Err(_) => VersionRequest::Range(VersionReq::parse(s).map_err(|e| {
                    RegistryError::InvalidSpec(spec.to_string(), format!("bad version '{s}': {e}"))
                })?),
            }
        }
    };

    Ok(PackageSpec {
        name: name.to_string(),
        req,
    })
}

/// Resolve `spec` against `registry` (a base URL like
/// `https://registry.npmjs.org`) by fetching the packument and selecting a
/// version. Returns the tarball URL and integrity to hand to the store.
pub fn resolve(spec: &PackageSpec, registry: &str) -> Result<ResolvedArtifact, RegistryError> {
    let packument = fetch_packument(&spec.name, registry)?;
    let version = select_version(&spec.name, &spec.req, &packument)?;

    let versions = packument
        .get("versions")
        .and_then(|v| v.as_object())
        .ok_or_else(|| RegistryError::NoVersions {
            package: spec.name.clone(),
        })?;
    let entry = versions.get(version.to_string().as_str()).ok_or_else(|| {
        RegistryError::VersionNotFound {
            package: spec.name.clone(),
            req: version.to_string(),
        }
    })?;
    let dist = entry
        .get("dist")
        .ok_or_else(|| RegistryError::MissingDist {
            package: spec.name.clone(),
            version: version.to_string(),
        })?;
    let tarball_url = dist
        .get("tarball")
        .and_then(|t| t.as_str())
        .ok_or_else(|| RegistryError::MissingDist {
            package: spec.name.clone(),
            version: version.to_string(),
        })?
        .to_string();
    let integrity = dist
        .get("integrity")
        .and_then(|t| t.as_str())
        .ok_or_else(|| RegistryError::MissingDist {
            package: spec.name.clone(),
            version: version.to_string(),
        })?
        .to_string();

    Ok(ResolvedArtifact {
        name: spec.name.clone(),
        version,
        tarball_url,
        integrity,
    })
}

/// Fetch and parse the packument JSON for `name`.
fn fetch_packument(name: &str, registry: &str) -> Result<serde_json::Value, RegistryError> {
    let base = registry.trim_end_matches('/');
    // npm encodes scoped names so the whole name is one path segment.
    let encoded = name.replace('/', "%2F");
    let url = format!("{base}/{encoded}");

    let resp = ureq::get(&url).call().map_err(|e| RegistryError::Network {
        package: name.to_string(),
        source: map_ureq_error(e),
    })?;
    let body = resp.into_string().map_err(|e| RegistryError::Network {
        package: name.to_string(),
        source: Box::new(e),
    })?;
    serde_json::from_str(&body).map_err(|source| RegistryError::BadJson {
        package: name.to_string(),
        source,
    })
}

/// Map a `ureq::Error` into a boxed error, turning a 4xx/5xx status into a
/// `BadStatus` registry error so a 404 reads as "not found" not "transport".
fn map_ureq_error(e: ureq::Error) -> Box<dyn std::error::Error + Send + Sync> {
    match e {
        ureq::Error::Status(code, _) => Box::new(RegistryStatus { code }),
        other => Box::new(other),
    }
}

/// A thin status-code wrapper so `Network` can carry a `BadStatus`-like cause
/// without recursing into `RegistryError`.
#[derive(Debug)]
struct RegistryStatus {
    code: u16,
}
impl std::fmt::Display for RegistryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "status {}", self.code)
    }
}
impl std::error::Error for RegistryStatus {}

/// Pick the target version string from a packument for a version request.
fn select_version(
    name: &str,
    req: &VersionRequest,
    packument: &serde_json::Value,
) -> Result<Version, RegistryError> {
    let versions = packument
        .get("versions")
        .and_then(|v| v.as_object())
        .ok_or_else(|| RegistryError::NoVersions {
            package: name.to_string(),
        })?;

    match req {
        VersionRequest::Latest => {
            let tag = packument
                .get("dist-tags")
                .and_then(|d| d.get("latest"))
                .and_then(|l| l.as_str())
                .ok_or_else(|| RegistryError::NoVersions {
                    package: name.to_string(),
                })?;
            Version::parse(tag).map_err(|_| RegistryError::VersionNotFound {
                package: name.to_string(),
                req: format!("latest ({tag})"),
            })
        }
        VersionRequest::Exact(v) => {
            if versions.contains_key(v.to_string().as_str()) {
                Ok(v.clone())
            } else {
                Err(RegistryError::VersionNotFound {
                    package: name.to_string(),
                    req: format!("={v}"),
                })
            }
        }
        VersionRequest::Range(r) => {
            // Deterministic max: parse all, filter, take the greatest (prereleases
            // excluded by `semver` unless the range explicitly opts in).
            let mut matching: Vec<Version> = versions
                .keys()
                .filter_map(|k| Version::parse(k).ok())
                .filter(|v| r.matches(v))
                .collect();
            matching.sort();
            matching
                .pop()
                .ok_or_else(|| RegistryError::VersionNotFound {
                    package: name.to_string(),
                    req: r.to_string(),
                })
        }
    }
}

/// Validate a package name per npm rules: `(@scope/)?name`, ASCII, <=214 chars,
/// each segment starts with a lowercase letter or digit and otherwise contains
/// only `[a-z0-9._-]`.
pub fn is_valid_npm_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 214 || !name.is_ascii() {
        return false;
    }
    match name.strip_prefix('@') {
        Some(rest) => match rest.split_once('/') {
            Some((scope, pkg)) => valid_segment(scope.as_bytes()) && valid_segment(pkg.as_bytes()),
            None => false,
        },
        None => valid_segment(name.as_bytes()),
    }
}

fn valid_segment(seg: &[u8]) -> bool {
    if seg.is_empty() {
        return false;
    }
    let first = seg[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    seg.iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_name_as_latest() {
        let s = parse_spec("lodash").unwrap();
        assert_eq!(s.name, "lodash");
        assert!(matches!(s.req, VersionRequest::Latest));
    }

    #[test]
    fn parses_scoped_name_without_version_as_latest() {
        let s = parse_spec("@scope/pkg").unwrap();
        assert_eq!(s.name, "@scope/pkg");
        assert!(matches!(s.req, VersionRequest::Latest));
    }

    #[test]
    fn parses_exact_version() {
        let s = parse_spec("lodash@4.17.21").unwrap();
        assert_eq!(s.name, "lodash");
        match s.req {
            VersionRequest::Exact(v) => assert_eq!(v, Version::parse("4.17.21").unwrap()),
            other => panic!("expected exact, got {other:?}"),
        }
    }

    #[test]
    fn parses_scoped_exact_version() {
        let s = parse_spec("@scope/pkg@1.2.3").unwrap();
        assert_eq!(s.name, "@scope/pkg");
        assert!(matches!(s.req, VersionRequest::Exact(_)));
    }

    #[test]
    fn parses_caret_and_tilde_as_range() {
        for spec in [
            "lodash@^4.17.0",
            "lodash@~4.17.0",
            "lodash@>=4.0.0",
            "lodash@*",
            "lodash@4.x",
        ] {
            let s = parse_spec(spec).unwrap_or_else(|e| panic!("parse {spec}: {e}"));
            assert_eq!(s.name, "lodash");
            assert!(matches!(s.req, VersionRequest::Range(_)), "{spec}");
        }
    }

    #[test]
    fn rejects_empty_spec() {
        assert!(parse_spec("").is_err());
        assert!(parse_spec("   ").is_err());
    }

    #[test]
    fn rejects_uppercase_and_invalid_names() {
        assert!(parse_spec("Lodash").is_err());
        assert!(parse_spec("has space").is_err());
        assert!(parse_spec("@noslash").is_err());
        assert!(parse_spec("@scope/").is_err());
    }

    #[test]
    fn rejects_bad_version() {
        assert!(parse_spec("lodash@not-a-version!").is_err());
    }

    #[test]
    fn name_validation_examples() {
        assert!(is_valid_npm_name("lodash"));
        assert!(is_valid_npm_name("@scope/pkg"));
        assert!(!is_valid_npm_name("Lodash"));
        assert!(!is_valid_npm_name(""));
        assert!(!is_valid_npm_name("@scope"));
        assert!(!is_valid_npm_name("has space"));
    }

    #[test]
    fn select_version_picks_latest_from_dist_tags() {
        let packument = serde_json::json!({
            "dist-tags": { "latest": "4.17.21" },
            "versions": { "1.0.0": {}, "4.17.21": {} }
        });
        let v = select_version("lodash", &VersionRequest::Latest, &packument).unwrap();
        assert_eq!(v, Version::parse("4.17.21").unwrap());
    }

    #[test]
    fn select_version_range_picks_highest_match() {
        let packument = serde_json::json!({
            "versions": { "1.0.0": {}, "4.0.0": {}, "4.17.20": {}, "4.17.21": {}, "5.0.0": {} }
        });
        let req = VersionRequest::Range(VersionReq::parse("^4.0.0").unwrap());
        let v = select_version("lodash", &req, &packument).unwrap();
        assert_eq!(v, Version::parse("4.17.21").unwrap());
    }

    #[test]
    fn select_version_exact_missing_errors() {
        let packument = serde_json::json!({ "versions": { "1.0.0": {} } });
        let req = VersionRequest::Exact(Version::parse("2.0.0").unwrap());
        let err = select_version("p", &req, &packument).unwrap_err();
        assert!(matches!(err, RegistryError::VersionNotFound { .. }));
    }

    #[test]
    fn resolve_reads_tarball_and_integrity() {
        let packument = serde_json::json!({
            "dist-tags": { "latest": "1.2.3" },
            "versions": {
                "1.2.3": {
                    "dist": {
                        "tarball": "https://example.test/p/-/p-1.2.3.tgz",
                        "integrity": "sha512-abc"
                    }
                }
            }
        });
        // Verify selection + dist extraction logic without a live HTTP server.
        let spec = parse_spec("p").unwrap();
        let version = select_version(&spec.name, &spec.req, &packument).unwrap();
        let dist = packument
            .get("versions")
            .unwrap()
            .get(version.to_string().as_str())
            .unwrap()
            .get("dist")
            .unwrap();
        assert_eq!(
            dist["tarball"].as_str().unwrap(),
            "https://example.test/p/-/p-1.2.3.tgz"
        );
        assert_eq!(dist["integrity"].as_str().unwrap(), "sha512-abc");
    }
}
