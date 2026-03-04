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
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(root.join("package.json"))?)?;
    let body = normalized_audit_body(&root, &manifest)?;

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
            let requires = body
                .get("requires")
                .and_then(|value| value.as_object())
                .map_or(0, serde_json::Map::len);
            println!(
                "audit offline: normalized {requires} package request(s); no advisory registry queried"
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
    let endpoint = format!("{}/-/npm/v1/security/audits", config.registry());
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
        let requires = body
            .get("requires")
            .and_then(|value| value.as_object())
            .map_or(0, serde_json::Map::len);
        println!(
            "audited {requires} package requests; {total} vulnerability finding(s) ({} at or above {})",
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

fn normalized_audit_body(
    root: &std::path::Path,
    manifest: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let requires = manifest_requires(manifest);
    let bpm_lock = root.join(bpm::lockfile::BPM_LOCK_FILE);
    let install = if bpm_lock.is_file() {
        normalize_bpm_lock(&bpm::lockfile::Lockfile::from_path(&bpm_lock)?)
    } else {
        let package_lock = root.join("package-lock.json");
        fs::read_to_string(&package_lock)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .unwrap_or_else(|| json!({"lockfileVersion": 0, "packages": {}}))
    };
    Ok(json!({"requires": requires, "install": install}))
}

fn manifest_requires(manifest: &serde_json::Value) -> BTreeMap<String, String> {
    let mut requires = BTreeMap::new();
    for group in ["dependencies", "devDependencies", "optionalDependencies"] {
        if let Some(values) = manifest.get(group).and_then(|v| v.as_object()) {
            for (name, spec) in values {
                requires.insert(name.clone(), spec.as_str().unwrap_or("*").to_string());
            }
        }
    }
    requires
}

fn normalize_bpm_lock(lockfile: &bpm::lockfile::Lockfile) -> serde_json::Value {
    let mut packages = serde_json::Map::new();
    packages.insert(
        "".into(),
        json!({
            "name": lockfile.root.name,
            "version": lockfile.root.version,
            "dependencies": lockfile.root.dependencies,
        }),
    );
    for package in &lockfile.packages {
        let mut value = json!({
            "name": package.name,
            "version": package.version,
            "resolved": package.resolved,
            "integrity": package.integrity,
            "dependencies": package.dependencies,
            "dev": package.dev,
            "optional": package.optional,
            "link": package.link,
        });
        if let Some(object) = value.as_object_mut() {
            object.retain(|_, value| !value.is_null());
        }
        packages.insert(package.path.clone(), value);
    }
    json!({
        "name": lockfile.root.name,
        "version": lockfile.root.version,
        "lockfileVersion": 3,
        "requires": true,
        "packages": packages,
    })
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

fn severity_counts(value: &serde_json::Value) -> BTreeMap<Severity, u64> {
    let mut counts = BTreeMap::from([
        (Severity::Info, 0),
        (Severity::Low, 0),
        (Severity::Moderate, 0),
        (Severity::High, 0),
        (Severity::Critical, 0),
    ]);
    if let Some(vulnerabilities) = value
        .get("metadata")
        .and_then(|v| v.get("vulnerabilities"))
        .and_then(|v| v.as_object())
    {
        for (severity, count) in vulnerabilities {
            if severity == "total" {
                continue;
            }
            if let (Ok(severity), Some(count)) = (Severity::parse(severity), count.as_u64()) {
                *counts.entry(severity).or_default() += count;
            }
        }
    }
    if counts.values().all(|count| *count == 0) {
        // Legacy audit responses can expose advisory objects instead of the
        // metadata aggregate. Count each advisory by its severity.
        if let Some(advisories) = value.get("advisories").and_then(|v| v.as_object()) {
            for advisory in advisories.values() {
                if let Some(severity) = advisory.get("severity").and_then(|v| v.as_str()) {
                    if let Ok(severity) = Severity::parse(severity) {
                        *counts.entry(severity).or_default() += 1;
                    }
                }
            }
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_threshold_counts_at_or_above_level() {
        let value = json!({"metadata":{"vulnerabilities":{"low":2,"moderate":1,"high":1,"critical":0,"total":4}}});
        let counts = severity_counts(&value);
        let threshold = Severity::Moderate;
        let failing = counts
            .iter()
            .filter(|(severity, _)| **severity >= threshold)
            .map(|(_, count)| *count)
            .sum::<u64>();
        assert_eq!(failing, 2);
    }

    #[test]
    fn manifest_requires_is_deterministic_across_dependency_groups() {
        let manifest = json!({
            "optionalDependencies": {"c":"3"},
            "dependencies": {"a":"1"},
            "devDependencies": {"b":"2"}
        });
        assert_eq!(
            manifest_requires(&manifest)
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }
}
