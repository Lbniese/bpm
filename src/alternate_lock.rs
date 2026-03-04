//! Deterministic import adapters for common non-npm lockfiles.
//!
//! These adapters are intentionally conservative text parsers: they recover the
//! package name, version, resolved tarball URL, integrity, and dependency edges
//! that BPM can install without trying to preserve every manager-specific knob.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::lockfile::{Lockfile, PackageEntry};

#[derive(Debug, thiserror::Error)]
pub enum AlternateLockError {
    #[error("cannot read lockfile {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("unsupported or malformed alternate lockfile: {0}")]
    Parse(String),
}

#[derive(Debug, Default, Clone)]
struct AltPackage {
    name: String,
    version: String,
    spec: String,
    resolved: String,
    integrity: Option<String>,
    dependencies: BTreeMap<String, String>,
}

pub fn import(path: &Path) -> Result<Lockfile, AlternateLockError> {
    let bytes = fs::read(path).map_err(|source| AlternateLockError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or_default();
    if name == "bun.lockb" {
        return Err(AlternateLockError::Parse(
            "binary bun.lockb files are not supported; use Bun's text bun.lock format".into(),
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| AlternateLockError::Parse("lockfile is not valid UTF-8 text".into()))?;
    if name == "yarn.lock" || text.lines().any(|line| line.starts_with("__metadata:")) {
        return import_yarn(&text);
    }
    if name.starts_with("pnpm-lock") || text.contains("lockfileVersion:") {
        return import_pnpm(&text);
    }
    if name == "bun.lock" {
        return import_bun(&text);
    }
    Err(AlternateLockError::Parse(format!(
        "unrecognized lockfile {}",
        path.display()
    )))
}

fn import_yarn(text: &str) -> Result<Lockfile, AlternateLockError> {
    let mut lock = Lockfile::new("bpm-yarn-import");
    let mut current: Option<AltPackage> = None;
    let mut in_dependencies = false;
    for raw in text.lines().chain(std::iter::once("")) {
        let line = raw.trim_end();
        if is_yarn_entry_header(line) {
            flush_package(&mut lock, current.take());
            in_dependencies = false;
            let selector = line.trim_end_matches(':');
            let first = selector.split(',').next().unwrap_or(selector).trim();
            let (name, spec) = parse_selector(first)?;
            current = Some(AltPackage {
                name,
                spec,
                ..AltPackage::default()
            });
            continue;
        }
        let Some(package) = current.as_mut() else {
            continue;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            in_dependencies = false;
            continue;
        }
        if trimmed == "dependencies:" {
            in_dependencies = true;
            continue;
        }
        if in_dependencies && raw.starts_with("    ") {
            if let Some((name, spec)) = parse_dependency_line(trimmed) {
                package.dependencies.insert(name, spec);
            }
            continue;
        }
        in_dependencies = false;
        if let Some(value) = trimmed.strip_prefix("version ") {
            package.version = unquote(value).to_owned();
        } else if let Some(value) = trimmed.strip_prefix("resolved ") {
            package.resolved = unquote(value)
                .split('#')
                .next()
                .unwrap_or_default()
                .to_owned();
        } else if let Some(value) = trimmed.strip_prefix("integrity ") {
            package.integrity = Some(unquote(value).to_owned());
        }
    }
    finish(lock, "yarn.lock contains no packages")
}

fn import_pnpm(text: &str) -> Result<Lockfile, AlternateLockError> {
    let mut lock = Lockfile::new("bpm-pnpm-import");
    let mut root_specs: BTreeMap<String, String> = BTreeMap::new();
    let mut packages = BTreeMap::<String, AltPackage>::new();
    let mut current_key: Option<String> = None;
    let mut in_packages = false;
    let mut in_importer_deps = false;
    let mut in_deps = false;

    for raw in text.lines() {
        let indent = raw.chars().take_while(|c| *c == ' ').count();
        let trimmed = raw.trim();
        if trimmed == "packages:" || trimmed == "snapshots:" {
            in_packages = true;
            in_importer_deps = false;
            current_key = None;
            continue;
        }
        if trimmed == "dependencies:" && !in_packages {
            in_importer_deps = true;
            continue;
        }
        if in_importer_deps {
            if indent <= 2 && !trimmed.starts_with("dependencies:") {
                in_importer_deps = false;
            } else if indent == 4 && trimmed.ends_with(':') {
                let dep = trimmed.trim_end_matches(':').to_owned();
                root_specs.entry(dep).or_default();
            } else if indent >= 6 && trimmed.starts_with("specifier:") {
                if let Some((last, value)) = root_specs.iter_mut().next_back() {
                    if value.is_empty() {
                        *value =
                            strip_yaml_value(trimmed.trim_start_matches("specifier:")).to_owned();
                    } else {
                        let _ = last;
                    }
                }
            }
        }
        if !in_packages {
            continue;
        }
        if indent == 2 && trimmed.ends_with(':') {
            let selector = trimmed
                .trim_end_matches(':')
                .trim_matches('"')
                .trim_start_matches('/');
            let (name, version) = parse_pnpm_package_key(selector)?;
            current_key = Some(format!("{name}@{version}"));
            packages
                .entry(format!("{name}@{version}"))
                .or_insert_with(|| AltPackage {
                    name,
                    version: version.clone(),
                    spec: version,
                    ..AltPackage::default()
                });
            in_deps = false;
            continue;
        }
        let Some(key) = current_key.as_ref() else {
            continue;
        };
        let package = packages.get_mut(key).expect("current package exists");
        if indent == 4 && trimmed == "dependencies:" {
            in_deps = true;
            continue;
        }
        if indent <= 4 && trimmed != "dependencies:" {
            in_deps = false;
        }
        if in_deps && indent >= 6 {
            if let Some((name, spec)) = parse_yaml_pair(trimmed) {
                package.dependencies.insert(name, spec);
            }
        } else if let Some(value) = trimmed.strip_prefix("integrity:") {
            package.integrity = Some(strip_yaml_value(value).to_owned());
        } else if let Some(value) = trimmed.strip_prefix("tarball:") {
            package.resolved = strip_yaml_value(value).to_owned();
        } else if let Some(value) = trimmed.strip_prefix("resolution:") {
            let inline = strip_yaml_value(value);
            if let Some(integrity) = inline_field(inline, "integrity") {
                package.integrity = Some(integrity);
            }
            if let Some(tarball) = inline_field(inline, "tarball") {
                package.resolved = tarball;
            }
        }
    }
    for package in packages.into_values() {
        push_alt_package(&mut lock, package);
    }
    for (name, spec) in root_specs {
        if !spec.is_empty() {
            lock.root.dependencies.insert(name, spec);
        }
    }
    finish(lock, "pnpm lockfile contains no package snapshots")
}

fn import_bun(text: &str) -> Result<Lockfile, AlternateLockError> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        return import_bun_json(&value);
    }
    let mut lock = Lockfile::new("bpm-bun-import");
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.contains('@') || trimmed.ends_with(':') || trimmed.starts_with('#') {
            continue;
        }
        let parts = trimmed.split_whitespace().collect::<Vec<_>>();
        if parts.len() >= 2 {
            let (name, version) = parse_bun_key(parts[0])?;
            let mut package = AltPackage {
                name,
                version: version.clone(),
                spec: version,
                ..AltPackage::default()
            };
            for part in &parts[1..] {
                if part.starts_with("sha512-") {
                    package.integrity = Some((*part).to_owned());
                } else if part.starts_with("http://") || part.starts_with("https://") {
                    package.resolved = (*part).to_owned();
                }
            }
            push_alt_package(&mut lock, package);
        }
    }
    finish(lock, "bun lockfile contains no packages")
}

fn import_bun_json(value: &serde_json::Value) -> Result<Lockfile, AlternateLockError> {
    let mut lock = Lockfile::new("bpm-bun-import");
    let packages = value
        .get("packages")
        .and_then(|v| v.as_object())
        .ok_or_else(|| AlternateLockError::Parse("bun lockfile has no packages object".into()))?;
    for (key, value) in packages {
        let (name, version) = parse_bun_key(key)?;
        let mut package = AltPackage {
            name,
            version: version.clone(),
            spec: value
                .get("specifier")
                .and_then(|v| v.as_str())
                .unwrap_or(&version)
                .to_owned(),
            resolved: value
                .get("resolved")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned(),
            integrity: value
                .get("integrity")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            dependencies: BTreeMap::new(),
        };
        if let Some(deps) = value.get("dependencies").and_then(|v| v.as_object()) {
            for (name, spec) in deps {
                package
                    .dependencies
                    .insert(name.clone(), spec.as_str().unwrap_or("*").to_owned());
            }
        }
        push_alt_package(&mut lock, package);
    }
    finish(lock, "bun lockfile contains no packages")
}

fn finish(mut lock: Lockfile, empty: &str) -> Result<Lockfile, AlternateLockError> {
    lock.sort_packages();
    if lock.packages.is_empty() {
        return Err(AlternateLockError::Parse(empty.into()));
    }
    Ok(lock)
}

fn push_alt_package(lock: &mut Lockfile, package: AltPackage) {
    if package.name.is_empty()
        || package.version.is_empty()
        || lock
            .packages
            .iter()
            .any(|p| p.name == package.name && p.version == package.version)
    {
        return;
    }
    lock.root
        .dependencies
        .entry(package.name.clone())
        .or_insert_with(|| package.spec.clone());
    lock.packages.push(PackageEntry {
        path: format!("node_modules/{}", package.name),
        name: package.name,
        version: package.version,
        resolved: package.resolved.clone(),
        workspace_target: None,
        integrity: package.integrity,
        link: package.resolved.is_empty(),
        dev: false,
        optional: false,
        os: Vec::new(),
        cpu: Vec::new(),
        bin: BTreeMap::new(),
        dependencies: package.dependencies,
    });
}

fn flush_package(lock: &mut Lockfile, package: Option<AltPackage>) {
    if let Some(package) = package {
        push_alt_package(lock, package);
    }
}

fn is_yarn_entry_header(line: &str) -> bool {
    !line.is_empty() && !line.starts_with('#') && !line.starts_with(' ') && line.ends_with(':')
}

fn parse_selector(selector: &str) -> Result<(String, String), AlternateLockError> {
    let selector = selector.trim().trim_matches('"').trim_matches('\'');
    let Some(index) = selector.rfind('@') else {
        return Err(AlternateLockError::Parse(format!(
            "invalid package selector {selector}"
        )));
    };
    if index == 0 {
        let Some(second) = selector[1..].rfind('@') else {
            return Err(AlternateLockError::Parse(format!(
                "invalid scoped selector {selector}"
            )));
        };
        let split = second + 1;
        return Ok((
            selector[..split].to_owned(),
            selector[split + 1..].to_owned(),
        ));
    }
    Ok((
        selector[..index].to_owned(),
        selector[index + 1..].to_owned(),
    ))
}

fn parse_dependency_line(line: &str) -> Option<(String, String)> {
    if let Some((name, spec)) = parse_yaml_pair(line) {
        return Some((name, spec));
    }
    let mut parts = line.split_whitespace();
    Some((parts.next()?.to_owned(), unquote(parts.next()?).to_owned()))
}

fn parse_yaml_pair(line: &str) -> Option<(String, String)> {
    let (name, value) = line.split_once(':')?;
    Some((
        name.trim().trim_matches('"').to_owned(),
        strip_yaml_value(value).to_owned(),
    ))
}

fn strip_yaml_value(value: &str) -> &str {
    unquote(value.trim().trim_start_matches(' ').trim_matches(','))
}

fn unquote(value: &str) -> &str {
    value.trim().trim_matches('"').trim_matches('\'')
}

fn inline_field(inline: &str, field: &str) -> Option<String> {
    let inner = inline.trim().trim_start_matches('{').trim_end_matches('}');
    for part in inner.split(',') {
        let (key, value) = part.split_once(':')?;
        if key.trim() == field {
            return Some(strip_yaml_value(value).to_owned());
        }
    }
    None
}

fn parse_pnpm_package_key(selector: &str) -> Result<(String, String), AlternateLockError> {
    let selector = selector.split('(').next().unwrap_or(selector);
    if selector.starts_with('@') {
        let slash = selector.find('/').ok_or_else(|| {
            AlternateLockError::Parse(format!("invalid pnpm package key {selector}"))
        })?;
        let at = selector[slash + 1..].rfind('@').ok_or_else(|| {
            AlternateLockError::Parse(format!("invalid pnpm package key {selector}"))
        })? + slash
            + 1;
        return Ok((selector[..at].to_owned(), selector[at + 1..].to_owned()));
    }
    selector
        .rsplit_once('@')
        .map(|(name, version)| (name.to_owned(), version.to_owned()))
        .ok_or_else(|| AlternateLockError::Parse(format!("invalid pnpm package key {selector}")))
}

fn parse_bun_key(key: &str) -> Result<(String, String), AlternateLockError> {
    let (name, version) = parse_selector(key)?;
    Ok((name, version.trim_start_matches('v').to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yarn_import_recovers_resolved_integrity_and_dependencies() {
        let lock = import_yarn(
            r#"left-pad@^1.3.0:
  version "1.3.0"
  resolved "https://registry/left-pad.tgz#abc"
  integrity sha512-abc
  dependencies:
    repeat-string "^1.0.0"

repeat-string@^1.0.0:
  version "1.6.1"
  resolved "https://registry/repeat-string.tgz"
  integrity sha512-def
"#,
        )
        .unwrap();
        let left = lock.packages.iter().find(|p| p.name == "left-pad").unwrap();
        assert_eq!(left.resolved, "https://registry/left-pad.tgz");
        assert_eq!(left.integrity.as_deref(), Some("sha512-abc"));
        assert_eq!(left.dependencies["repeat-string"], "^1.0.0");
        assert!(!left.link);
    }

    #[test]
    fn pnpm_import_recovers_snapshot_edges() {
        let lock = import_pnpm(
            r#"lockfileVersion: '9.0'
packages:
  /foo@1.0.0:
    resolution: {integrity: sha512-foo, tarball: https://registry/foo.tgz}
    dependencies:
      bar: 2.0.0
  /bar@2.0.0:
    resolution: {integrity: sha512-bar, tarball: https://registry/bar.tgz}
"#,
        )
        .unwrap();
        let foo = lock.packages.iter().find(|p| p.name == "foo").unwrap();
        assert_eq!(foo.resolved, "https://registry/foo.tgz");
        assert_eq!(foo.integrity.as_deref(), Some("sha512-foo"));
        assert_eq!(foo.dependencies["bar"], "2.0.0");
    }
}
