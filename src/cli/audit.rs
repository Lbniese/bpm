use serde_json::json;
use std::{collections::BTreeMap, env, path::PathBuf};

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
    // Fail closed: a malformed or wrong-shaped advisory response is an error,
    // never a silent zero-vulnerability success. Do not echo an invalid body
    // back as valid JSON in `--json` mode.
    let value: serde_json::Value = serde_json::from_slice(&response).map_err(|err| {
        anyhow::anyhow!("audit endpoint {endpoint} returned malformed JSON: {err}")
    })?;
    let counts = severity_counts(&value)?;
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
    let bpm_lock = root.join(bpm::lockfile::BPM_LOCK_FILE);
    let groups = if bpm_lock.is_file() {
        let lockfile = bpm::lockfile::Lockfile::from_path(&bpm_lock).map_err(|err| {
            anyhow::anyhow!(
                "failed to read bpm lockfile at {}: {err}",
                bpm_lock.display()
            )
        })?;
        bulk_groups_from_lockfile(&lockfile)
    } else {
        // Fail closed: when there is no bpm.lock, audit requires a valid npm
        // package-lock.json with resolved versions. Declaration-only
        // `package.json` data is not auditable because it lacks resolved
        // versions. A missing, unreadable, malformed, or unsupported npm lock
        // is a hard error rather than an empty inventory.
        bulk_groups_from_npm_lock(root)?
    };

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
    // A dependency-free (root-only) lock legitimately yields an empty object.
    Ok(serde_json::Value::Object(body))
}

/// Normalize an npm v2/v3 lock through the shared importer, then group it
/// through the same canonical-name path as a native BPM lock.
fn bulk_groups_from_npm_lock(
    root: &std::path::Path,
) -> anyhow::Result<BTreeMap<String, std::collections::BTreeSet<String>>> {
    let package_lock = root.join("package-lock.json");
    let report = bpm::project_lock::load_npm_package_lock(&package_lock).map_err(|error| {
        anyhow::anyhow!(
            "audit needs a valid bpm.lock or npm package-lock.json; failed to load {}: {error}",
            package_lock.display()
        )
    })?;
    Ok(bulk_groups_from_lockfile(&report.lockfile))
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
            .entry(lockfile.registry_name_for(package).to_string())
            .or_default()
            .insert(package.version.clone());
    }
    groups
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
fn severity_counts(value: &serde_json::Value) -> anyhow::Result<BTreeMap<Severity, u64>> {
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
    let map = value.as_object().ok_or_else(|| {
        anyhow::anyhow!(
            "audit response must be a JSON object mapping package names to advisory arrays"
        )
    })?;
    for (package, advisories) in map {
        let arr = advisories.as_array().ok_or_else(|| {
            anyhow::anyhow!("audit response package `{package}` must map to an array of advisories")
        })?;
        for advisory in arr {
            let advisory_obj = advisory.as_object().ok_or_else(|| {
                anyhow::anyhow!("audit advisory for `{package}` must be a JSON object")
            })?;
            let severity = advisory_obj
                .get("severity")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    anyhow::anyhow!("audit advisory for `{package}` is missing a string `severity`")
                })?;
            let severity = Severity::parse(severity)?;
            // If the advisory carries a numeric id we have already
            // counted, skip the duplicate occurrence.
            if let Some(id) = advisory_obj.get("id").and_then(|v| v.as_u64()) {
                if !seen_ids.insert(id) {
                    continue;
                }
            }
            *counts.entry(severity).or_default() += 1;
        }
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
    fn bulk_groups_use_canonical_alias_identity() {
        let mut lockfile = bpm::lockfile::Lockfile::new("test");
        lockfile.packages.push(bpm::lockfile::PackageEntry {
            path: "node_modules/alias".into(),
            name: "alias".into(),
            version: "1.2.3".into(),
            resolved: "https://registry.example/real-package.tgz".into(),
            ..Default::default()
        });
        lockfile
            .resolution
            .registry_names
            .insert("node_modules/alias".into(), "real-package".into());

        let groups = bulk_groups_from_lockfile(&lockfile);
        assert_eq!(
            groups["real-package"]
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["1.2.3"]
        );
        assert!(!groups.contains_key("alias"));
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
        let counts = severity_counts(&value).expect("valid bulk response");
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
        let counts = severity_counts(&value).expect("empty object is valid");
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
        let counts = severity_counts(&value).expect("valid response");
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
        let counts = severity_counts(&value).expect("valid response");
        // id 2 appears under two packages but counts once.
        assert_eq!(counts[&Severity::Low], 1);
        assert_eq!(counts[&Severity::High], 1);
    }

    #[test]
    fn severity_counts_rejects_non_object_top_level() {
        let value = json!([{"id": 1, "severity": "high"}]);
        assert!(severity_counts(&value).is_err());
    }

    #[test]
    fn severity_counts_rejects_non_array_package_value() {
        // A syntactically valid but wrong-shaped response: a package key mapped
        // to an object instead of an array.
        let value = json!({"left-pad": {"id": 1, "severity": "high"}});
        assert!(severity_counts(&value).is_err());
    }

    #[test]
    fn severity_counts_rejects_non_object_advisory() {
        let value = json!({"left-pad": ["not-an-object"]});
        assert!(severity_counts(&value).is_err());
    }

    #[test]
    fn severity_counts_rejects_missing_and_unknown_severity() {
        // Missing severity entirely.
        assert!(severity_counts(&json!({"left-pad": [{"id": 1}]})).is_err());
        // Unknown severity string.
        assert!(severity_counts(&json!({"left-pad": [{"id": 1, "severity": "boom"}]})).is_err());
    }

    #[test]
    fn severity_counts_allows_non_numeric_id_and_per_occurrence() {
        // A non-numeric id is allowed and counted per occurrence (no dedup).
        let value = json!({
            "left-pad": [{"id": "not-a-number", "severity": "high"}],
            "right-pad": [{"id": "also-not-a-number", "severity": "high"}]
        });
        let counts = severity_counts(&value).expect("non-numeric ids are allowed");
        assert_eq!(counts[&Severity::High], 2);
    }

    fn write_npm_lock(root: &std::path::Path, json: &str) {
        fs::write(root.join("package-lock.json"), json).unwrap();
    }

    #[test]
    fn npm_lock_valid_v3_groups_versions() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("package.json"),
            r#"{"name":"app","version":"1.0.0"}"#,
        )
        .unwrap();
        write_npm_lock(
            root.path(),
            r#"{"name":"app","version":"1.0.0","lockfileVersion":3,"packages":{"":{"name":"app","version":"1.0.0"},"node_modules/left-pad":{"version":"1.3.0"}}}"#,
        );
        let body = audit_bulk_body(root.path()).expect("valid v3 lock");
        assert_eq!(body["left-pad"].as_array().unwrap(), &vec![json!("1.3.0")]);
        assert_eq!(body.as_object().unwrap().len(), 1);
    }

    #[test]
    fn npm_lock_valid_v2_uses_packages_table_for_inventory() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("package.json"),
            r#"{"name":"app","version":"1.0.0"}"#,
        )
        .unwrap();
        write_npm_lock(
            root.path(),
            r#"{"name":"app","version":"1.0.0","lockfileVersion":2,"dependencies":{"left-pad":{"version":"9.9.9"}},"packages":{"":{"name":"app","version":"1.0.0"},"node_modules/left-pad":{"version":"1.3.0","resolved":"https://example/left-pad.tgz","integrity":"sha512-abc"}}}"#,
        );
        let body = audit_bulk_body(root.path()).expect("valid v2 lock");
        assert_eq!(body["left-pad"].as_array().unwrap(), &vec![json!("1.3.0")]);
    }

    #[test]
    fn npm_lock_alias_groups_by_explicit_canonical_name() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("package.json"),
            r#"{"name":"app","version":"1.0.0"}"#,
        )
        .unwrap();
        write_npm_lock(
            root.path(),
            r#"{"name":"app","lockfileVersion":3,"packages":{"":{"version":"1.0.0","dependencies":{"alias":"npm:real-package@1.2.3"}},"node_modules/alias":{"name":"real-package","version":"1.2.3","resolved":"https://example/real.tgz","integrity":"sha512-abc"}}}"#,
        );
        let body = audit_bulk_body(root.path()).expect("valid alias lock");
        assert_eq!(body["real-package"], json!(["1.2.3"]));
        assert!(body.get("alias").is_none());
    }

    #[test]
    fn npm_lock_root_only_yields_empty_inventory() {
        // A dependency-free, valid npm v3 lock is a legitimate empty inventory.
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("package.json"),
            r#"{"name":"app","version":"1.0.0"}"#,
        )
        .unwrap();
        write_npm_lock(
            root.path(),
            r#"{"name":"app","version":"1.0.0","lockfileVersion":3,"packages":{"":{"name":"app","version":"1.0.0"}}}"#,
        );
        let body = audit_bulk_body(root.path()).expect("root-only lock is valid");
        assert_eq!(body.as_object().unwrap().len(), 0);
    }

    #[test]
    fn npm_lock_missing_is_a_hard_error() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("package.json"),
            r#"{"name":"app","version":"1.0.0"}"#,
        )
        .unwrap();
        let err = audit_bulk_body(root.path()).unwrap_err().to_string();
        assert!(
            err.contains("package-lock.json") && err.contains("bpm.lock"),
            "error should name both lock sources, got: {err}"
        );
    }

    #[test]
    fn npm_lock_malformed_json_is_a_hard_error() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("package.json"),
            r#"{"name":"app","version":"1.0.0"}"#,
        )
        .unwrap();
        write_npm_lock(root.path(), "{ this is not json");
        assert!(audit_bulk_body(root.path()).is_err());
    }

    #[test]
    fn npm_lock_unsupported_version_is_a_hard_error() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("package.json"),
            r#"{"name":"app","version":"1.0.0"}"#,
        )
        .unwrap();
        write_npm_lock(
            root.path(),
            r#"{"lockfileVersion":1,"packages":{"":{"name":"app"}}}"#,
        );
        let err = audit_bulk_body(root.path()).unwrap_err().to_string();
        assert!(
            err.contains("versions 2 and 3"),
            "error should mention versions 2 and 3 support, got: {err}"
        );
    }

    #[test]
    fn npm_lock_missing_packages_is_a_hard_error() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("package.json"),
            r#"{"name":"app","version":"1.0.0"}"#,
        )
        .unwrap();
        write_npm_lock(root.path(), r#"{"lockfileVersion":3}"#);
        assert!(audit_bulk_body(root.path()).is_err());
    }
}
