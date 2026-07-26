use serde_json::json;
use std::{collections::BTreeMap, env, fs, path::PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Severity {
    Info,
    Low,
    Moderate,
    High,
    Critical,
}

impl Severity {
    fn parse(value: &str) -> anyhow::Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "info" => Ok(Self::Info),
            "low" => Ok(Self::Low),
            "moderate" | "medium" => Ok(Self::Moderate),
            "high" => Ok(Self::High),
            "critical" => Ok(Self::Critical),
            _ => anyhow::bail!(
                "invalid audit level `{value}` (expected info, low, moderate, high, or critical)"
            ),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Low => "low",
            Self::Moderate => "moderate",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

pub(super) fn run(
    registry: Option<String>,
    json_output: bool,
    offline: bool,
    audit_level: &str,
) -> anyhow::Result<()> {
    let threshold = Severity::parse(audit_level)?;
    let cwd = env::current_dir()?;
    let root = bpm::project::find_project_root(&cwd)?;
    let body = audit_bulk_body(&root)?;
    let package_count = body.as_object().map(serde_json::Map::len).unwrap_or(0);

    if offline {
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "offline": true,
                    "auditLevel": threshold.as_str(),
                    "request": body,
                    "metadata": {"vulnerabilities": severity_zeroes()}
                }))?
            );
        } else {
            println!(
                "audit offline: normalized {package_count} package request(s); no advisory registry queried"
            );
        }
        return Ok(());
    }

    let home = env::var_os("HOME").map(PathBuf::from);
    let config = bpm::config::NpmConfig::load(&root, home.as_deref())?;
    let config = match registry {
        Some(value) => config.with_registry_override(&value)?,
        None => config,
    };
    let client = bpm::http::HttpClient::new(config.clone());
    // npm's modern audit uses the bulk advisory endpoint, NOT the legacy
    // `/-/npm/v1/security/audits` path. The bulk endpoint accepts a flat
    // `{package_name: [version, ...]}` body and returns the matched advisories.
    // Posting a lockfile-shaped body to the old `audits` path made npm respond
    // HTTP 400 Bad Request.
    let endpoint = format!("{}/-/npm/v1/security/advisories/bulk", config.registry());
    let response = client
        .post_json(&endpoint, serde_json::to_vec(&body)?.as_slice())
        .map_err(|e| anyhow::anyhow!("audit failed: {e}"))?;
    let value: serde_json::Value = serde_json::from_slice(&response)
        .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&response)}));
    let counts = severity_counts(&value);
    let total = counts.values().copied().sum::<u64>();
    let failing = counts
        .iter()
        .filter(|(severity, _)| **severity >= threshold)
        .map(|(_, count)| *count)
        .sum::<u64>();

    if json_output {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!(
            "audited {package_count} package request(s); {total} vulnerability finding(s) ({} at or above {})",
            failing,
            threshold.as_str()
        );
    }

    if failing > 0 {
        anyhow::bail!(
            "audit failed: {failing} vulnerability finding(s) at or above {}",
            threshold.as_str()
        );
    }
    Ok(())
}

/// Build the npm bulk-audit request body: a flat JSON object mapping each
/// dependency package **name** to the distinct set of resolved **versions**
/// present in the tree, e.g. `{"react": ["19.2.8"], "@types/unist": ["3.0.3", "2.0.11"]}`.
///
/// This is exactly what `npm audit` POSTs to
/// `/-/npm/v1/security/advisories/bulk`. The registry matches each
/// `name@version` against its advisory database and returns the per-package
/// advisory lists. Integrity hashes and the dependency graph are not required
/// for the lookup.
fn audit_bulk_body(root: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    let mut groups: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();

    let bpm_lock = root.join(bpm::lockfile::BPM_LOCK_FILE);
    if bpm_lock.is_file() {
        let lockfile = bpm::lockfile::Lockfile::from_path(&bpm_lock)?;
        groups = bulk_groups_from_lockfile(&lockfile);
    } else {
        // Fallback: derive name -> versions from an npm package-lock.json when
        // no bpm.lock exists. package-lock v3 keys are `node_modules/...` paths;
        // the package name is the final path segment (handling `@scope/name`).
        let package_lock = root.join("package-lock.json");
        if let Ok(text) = fs::read_to_string(&package_lock) {
            if let Ok(lock) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(packages) = lock.get("packages").and_then(|v| v.as_object()) {
                    for (path, entry) in packages {
                        if let Some(name) = package_name_from_lock_path(path) {
                            if let Some(version) = entry.get("version").and_then(|v| v.as_str()) {
                                groups.entry(name).or_default().insert(version.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    let body: serde_json::Map<String, serde_json::Value> = groups
        .into_iter()
        .map(|(name, versions)| {
            (
                name,
                serde_json::Value::Array(
                    versions.into_iter().map(serde_json::Value::from).collect(),
                ),
            )
        })
        .collect();
    Ok(serde_json::Value::Object(body))
}

/// Group every resolved package in a bpm lockfile by name, collecting the
/// distinct set of versions installed for each (so a package present at the
/// same version in multiple locations contributes one entry).
fn bulk_groups_from_lockfile(
    lockfile: &bpm::lockfile::Lockfile,
) -> BTreeMap<String, std::collections::BTreeSet<String>> {
    let mut groups: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    for package in &lockfile.packages {
        // Skip workspace/file links and entries without a resolved version.
        if package.link || package.version.is_empty() {
            continue;
        }
        groups
            .entry(package.name.clone())
            .or_default()
            .insert(package.version.clone());
    }
    groups
}

/// Derive a package name from a package-lock v3 `packages` key
/// (`node_modules/...` path). Takes the segment after the last
/// `node_modules/` so scoped (`@scope/name`) and nested installs resolve to
/// the installed package's name.
fn package_name_from_lock_path(path: &str) -> Option<String> {
    let tail = path.rsplit("node_modules/").next()?;
    if tail.is_empty() {
        None
    } else {
        Some(tail.to_string())
    }
}

fn severity_zeroes() -> BTreeMap<&'static str, u64> {
    BTreeMap::from([
        ("info", 0),
        ("low", 0),
        ("moderate", 0),
        ("high", 0),
        ("critical", 0),
    ])
}

/// Count advisory severities from a `/-/npm/v1/security/advisories/bulk`
/// response, which maps each package name to an array of advisory objects,
/// each carrying a `severity` field.
///
/// A single advisory (identified by its numeric `id`) can affect several
/// packages in the tree and is therefore repeated under each affected package
/// name. Count each distinct `id` once, matching `npm audit`. An advisory
/// object without a parseable `id` is counted once per occurrence (preserving
/// the previous behavior for malformed entries so they are not silently
/// hidden).
fn severity_counts(value: &serde_json::Value) -> BTreeMap<Severity, u64> {
    let mut counts = BTreeMap::from([
        (Severity::Info, 0),
        (Severity::Low, 0),
        (Severity::Moderate, 0),
        (Severity::High, 0),
        (Severity::Critical, 0),
    ]);
    // Distinct advisory `id`s already counted. Advisories without a numeric
    // `id` are counted per-occurrence (see doc comment) and are not added
    // here, so two id-less entries do not collapse into one.
    let mut seen_ids: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    if let Some(map) = value.as_object() {
        for advisories in map.values() {
            if let Some(arr) = advisories.as_array() {
                for advisory in arr {
                    let Some(severity) = advisory.get("severity").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let Ok(severity) = Severity::parse(severity) else {
                        continue;
                    };
                    // If the advisory carries a numeric id we have already
                    // counted, skip the duplicate occurrence.
                    if let Some(id) = advisory.get("id").and_then(|v| v.as_u64()) {
                        if !seen_ids.insert(id) {
                            continue;
                        }
                    }
                    *counts.entry(severity).or_default() += 1;
                }
            }
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_lockfile() -> bpm::lockfile::Lockfile {
        use bpm::lockfile::{Lockfile, PackageEntry, RootEntry};
        use std::collections::BTreeMap;
        let mut root_deps = BTreeMap::new();
        root_deps.insert("react".to_string(), "^19.0.0".to_string());
        // Two distinct react placements at the SAME version (nested + hoisted)
        // to prove the version set is deduped.
        let react_hoisted = PackageEntry {
            path: "node_modules/react".into(),
            name: "react".into(),
            version: "19.2.8".into(),
            resolved: "https://registry.npmjs.org/react/-/react-19.2.8.tgz".into(),
            integrity: Some("sha512-PROD".into()),
            ..Default::default()
        };
        let react_nested = PackageEntry {
            path: "node_modules/foo/node_modules/react".into(),
            name: "react".into(),
            version: "19.2.8".into(),
            resolved: "https://registry.npmjs.org/react/-/react-19.2.8.tgz".into(),
            integrity: Some("sha512-PROD".into()),
            ..Default::default()
        };
        let jest = PackageEntry {
            path: "node_modules/jest".into(),
            name: "jest".into(),
            version: "29.0.0".into(),
            resolved: "https://registry.npmjs.org/jest/-/jest-29.0.0.tgz".into(),
            integrity: Some("sha512-DEV".into()),
            dev: true,
            ..Default::default()
        };
        // A workspace link entry with no resolved version; must be skipped.
        let linked = PackageEntry {
            path: "node_modules/my-workspace".into(),
            name: "my-workspace".into(),
            version: String::new(),
            link: true,
            ..Default::default()
        };
        Lockfile {
            lockfile_version: 3,
            generator: "bpm-test".into(),
            root: RootEntry {
                name: Some("demo".into()),
                version: Some("1.0.0".into()),
                dependencies: root_deps,
            },
            packages: vec![react_hoisted, react_nested, jest, linked],
            resolution: Default::default(),
        }
    }

    #[test]
    fn audit_bulk_body_groups_and_dedupes_versions_by_name() {
        let groups = bulk_groups_from_lockfile(&sample_lockfile());
        let body: serde_json::Map<String, serde_json::Value> = groups
            .into_iter()
            .map(|(name, versions)| {
                (
                    name,
                    serde_json::Value::Array(
                        versions.into_iter().map(serde_json::Value::from).collect(),
                    ),
                )
            })
            .collect();
        let body = serde_json::Value::Object(body);
        // Flat {name: [versions]} shape — no lockfile wrappers.
        assert!(body.get("packages").is_none());
        assert!(body.get("requires").is_none());
        assert!(body.get("metadata").is_none());
        // react deduped to a single version despite two placements.
        assert_eq!(body["react"].as_array().unwrap(), &vec![json!("19.2.8")]);
        // jest kept, link-without-version skipped.
        assert_eq!(body["jest"].as_array().unwrap(), &vec![json!("29.0.0")]);
        assert!(body.get("my-workspace").is_none());
        assert_eq!(body.as_object().unwrap().len(), 2);
    }

    #[test]
    fn package_name_from_lock_path_handles_scoped_and_nested() {
        assert_eq!(
            package_name_from_lock_path("node_modules/@scope/pkg").as_deref(),
            Some("@scope/pkg")
        );
        assert_eq!(
            package_name_from_lock_path("node_modules/foo").as_deref(),
            Some("foo")
        );
        assert_eq!(
            package_name_from_lock_path("node_modules/a/node_modules/b").as_deref(),
            Some("b")
        );
        assert_eq!(package_name_from_lock_path(""), None);
    }

    #[test]
    fn severity_counts_parses_bulk_response() {
        let value = json!({
            "@hono/node-server": [
                {"id": 1124006, "severity": "moderate", "title": "path traversal"},
                {"id": 1124007, "severity": "high"}
            ],
            "lodash": [
                {"id": 100, "severity": "low"}
            ],
            "clean-pkg": []
        });
        let counts = severity_counts(&value);
        let failing_at_moderate = counts
            .iter()
            .filter(|(severity, _)| **severity >= Severity::Moderate)
            .map(|(_, count)| *count)
            .sum::<u64>();
        // moderate(1) + high(1) = 2 at or above moderate.
        assert_eq!(failing_at_moderate, 2);
        // total across all severities = 3.
        assert_eq!(counts.values().copied().sum::<u64>(), 3);
    }

    #[test]
    fn empty_bulk_response_counts_zero() {
        let value = json!({});
        let counts = severity_counts(&value);
        assert_eq!(counts.values().copied().sum::<u64>(), 0);
    }

    #[test]
    fn severity_counts_dedupes_shared_advisory_id_across_packages() {
        // The same advisory id (42) appears under two package names. It must be
        // counted once, as `high`.
        let value = json!({
            "left-pad": [{"id": 42, "severity": "high", "title": "x", "url": "https://e.test/x"}],
            "right-pad": [{"id": 42, "severity": "high", "title": "x", "url": "https://e.test/x"}]
        });
        let counts = severity_counts(&value);
        assert_eq!(counts[&Severity::High], 1);
        let total: u64 = counts.values().copied().sum();
        assert_eq!(total, 1, "shared advisory id counted once overall");
    }

    #[test]
    fn severity_counts_distinct_ids_count_independently() {
        // Two different advisory ids under the same and different packages.
        let value = json!({
            "left-pad": [
                {"id": 1, "severity": "high", "title": "a", "url": "https://e.test/a"},
                {"id": 2, "severity": "low",  "title": "b", "url": "https://e.test/b"}
            ],
            "right-pad": [{"id": 2, "severity": "low", "title": "b", "url": "https://e.test/b"}]
        });
        let counts = severity_counts(&value);
        // id 2 appears under two packages but counts once.
        assert_eq!(counts[&Severity::Low], 1);
        assert_eq!(counts[&Severity::High], 1);
    }
}
