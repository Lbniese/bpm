use std::collections::BTreeSet;
use std::{env, fs, path::PathBuf};

use base64::Engine;
use flate2::{write::GzEncoder, Compression};
use serde_json::json;
use sha2::{Digest, Sha512};

pub(super) fn run(
    registry: Option<String>,
    access: Option<String>,
    otp: Option<String>,
    provenance: bool,
) -> anyhow::Result<()> {
    let root = bpm::project::find_project_root(&env::current_dir()?)?;
    let manifest_path = root.join("package.json");
    let manifest_text = fs::read_to_string(&manifest_path)?;
    let manifest_json: serde_json::Value = serde_json::from_str(&manifest_text)?;
    let manifest = bpm::manifest::PackageManifest::from_json(&manifest_text, &manifest_path)?;
    let name = manifest
        .name
        .clone()
        .ok_or_else(|| anyhow::anyhow!("package.json requires a name for publish"))?;
    let version = manifest
        .version
        .clone()
        .ok_or_else(|| anyhow::anyhow!("package.json requires a version for publish"))?;
    let home = env::var_os("HOME").map(PathBuf::from);
    let config = bpm::config::NpmConfig::load(&root, home.as_deref())?;
    let config = match registry {
        Some(value) => config.with_registry_override(&value)?,
        None => config,
    };
    let client = bpm::http::HttpClient::new(config.clone());
    let files = package_file_list(&root, &manifest_json)?;
    let tarball = pack(&root, &files)?;
    let filename = format!(
        "{}-{}.tgz",
        name.rsplit('/').next().unwrap_or(&name),
        version
    );
    let mut hash = Sha512::new();
    hash.update(&tarball);
    let integrity = format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(hash.finalize())
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(&tarball);
    let mut body = json!({
        "name": name,
        "_id": format!("{name}@{version}"),
        "versions": {
            version.clone(): {
                "name": name,
                "version": version,
                "dist": {
                    "integrity": integrity,
                    "tarball": format!("{}/{}/-/{}", config.registry(), name.replace('/', "%2f"), filename)
                }
            }
        },
        "access": access.unwrap_or_else(|| "restricted".into()),
        "dist-tags": {"latest": version},
        "_attachments": {
            filename: {
                "content_type": "application/octet-stream",
                "data": encoded,
                "length": tarball.len()
            }
        }
    });
    if provenance {
        body["bpmProvenance"] = json!({
            "builder": "bpm",
            "packageManager": concat!("bpm@", env!("CARGO_PKG_VERSION")),
            "source": env::var("GITHUB_REPOSITORY").ok(),
            "commit": env::var("GITHUB_SHA").ok(),
        });
    }
    let url = format!("{}/{}", config.registry(), name.replace('/', "%2f"));
    let body_bytes = serde_json::to_vec(&body)?;
    let headers = otp
        .as_deref()
        .map(|otp| vec![("npm-otp", otp)])
        .unwrap_or_default();
    client
        .put_json_with_headers(&url, body_bytes.as_slice(), &headers)
        .map_err(|e| {
            let message = e.to_string();
            if message.contains("status 409") {
                anyhow::anyhow!(
                    "publish failed: {name}@{version} already exists on the registry (HTTP 409)"
                )
            } else if message.contains("status 401") && otp.is_none() {
                anyhow::anyhow!(
                    "publish failed: registry requires authentication or two-factor OTP; retry with --otp <code> if 2FA is enabled"
                )
            } else {
                anyhow::anyhow!("publish failed: {message}")
            }
        })?;
    println!(
        "published {name}@{version} ({} file(s), {} bytes)",
        files.len(),
        tarball.len()
    );
    Ok(())
}

fn pack(root: &std::path::Path, files: &[String]) -> anyhow::Result<Vec<u8>> {
    let mut out = GzEncoder::new(Vec::new(), Compression::default());
    let mut tar = tar::Builder::new(&mut out);
    for file in files {
        tar.append_path_with_name(root.join(file), format!("package/{file}"))?;
    }
    tar.finish()?;
    drop(tar);
    Ok(out.finish()?)
}

fn package_file_list(
    root: &std::path::Path,
    manifest_json: &serde_json::Value,
) -> anyhow::Result<Vec<String>> {
    let declared_files = manifest_json
        .get("files")
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str())
                .map(normalize_manifest_path)
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let ignore_patterns = load_ignore_patterns(root)?;
    let mut files = Vec::new();
    let root_real = root.canonicalize()?;
    collect_files(root, &root_real, root, &mut files)?;
    files.retain(|path| should_publish(path, &declared_files, &ignore_patterns));
    let mut set = files.into_iter().collect::<BTreeSet<_>>();
    for always in always_include(root) {
        set.insert(always);
    }
    Ok(set.into_iter().collect())
}

fn collect_files(
    root: &std::path::Path,
    root_real: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<String>,
) -> anyhow::Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(name.as_ref(), "node_modules" | ".git" | ".bpm" | "target") {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_files(root, root_real, &path, out)?;
        } else if entry.file_type()?.is_file() {
            let rel = path
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            out.push(rel);
        } else if entry.file_type()?.is_symlink() {
            let rel = path
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            validate_publish_symlink(root_real, &path, &rel)?;
        }
    }
    Ok(())
}

fn validate_publish_symlink(
    root_real: &std::path::Path,
    path: &std::path::Path,
    rel: &str,
) -> anyhow::Result<()> {
    let target = fs::read_link(path)
        .map_err(|error| anyhow::anyhow!("failed to inspect symlink target for {rel}: {error}"))?;
    let absolute_target = if target.is_absolute() {
        target
    } else {
        path.parent().unwrap_or(root_real).join(target)
    };

    let absolute_target = absolute_target
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("publish rejected dangling symlink target for {rel}"))?;

    if !absolute_target.starts_with(root_real) {
        anyhow::bail!("publish rejected symlink {rel} outside project root");
    }

    anyhow::bail!("publish does not support symlink entries: {rel}")
}

fn should_publish(path: &str, declared_files: &[String], ignore_patterns: &[String]) -> bool {
    if is_always_include(path) {
        return true;
    }
    if is_default_exclude(path) {
        return false;
    }
    if !declared_files.is_empty()
        && !declared_files
            .iter()
            .any(|pattern| path == pattern || path.starts_with(&format!("{}/", pattern)))
    {
        return false;
    }
    !ignore_patterns
        .iter()
        .any(|pattern| ignore_match(path, pattern))
}

fn always_include(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in fs::read_dir(root).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let lower = name.to_ascii_lowercase();
        if (lower == "package.json" || lower.starts_with("readme") || lower.starts_with("license"))
            && entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
        {
            out.push(name);
        }
    }
    out
}

fn is_always_include(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower == "package.json" || lower.starts_with("readme") || lower.starts_with("license")
}

fn is_default_exclude(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower == "bpm.lock"
        || lower == "package-lock.json"
        || lower == "yarn.lock"
        || lower == "pnpm-lock.yaml"
        || lower == "bun.lockb"
        || lower.ends_with(".tmp")
        || lower.starts_with(".git/")
        || lower.starts_with("node_modules/")
        || lower.starts_with("target/")
}

fn load_ignore_patterns(root: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let path = if root.join(".npmignore").is_file() {
        root.join(".npmignore")
    } else {
        root.join(".gitignore")
    };
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let patterns = fs::read_to_string(path)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with('!'))
        .map(normalize_manifest_path)
        .collect();
    Ok(patterns)
}

fn ignore_match(path: &str, pattern: &str) -> bool {
    let pattern = pattern.trim_end_matches('/');
    if pattern.is_empty() {
        return false;
    }
    path == pattern
        || path.starts_with(&format!("{pattern}/"))
        || path.rsplit('/').next().is_some_and(|name| name == pattern)
}

fn normalize_manifest_path(value: &str) -> String {
    value
        .trim()
        .trim_start_matches("./")
        .trim_matches('/')
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_file_list_honors_files_and_ignore_with_npm_always_includes() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("package.json"),
            r#"{"name":"p","version":"1.0.0","files":["dist"]}"#,
        )
        .unwrap();
        fs::write(root.path().join("README.md"), "readme").unwrap();
        fs::write(root.path().join("secret.txt"), "secret").unwrap();
        fs::write(root.path().join(".npmignore"), "dist/private.txt\n").unwrap();
        fs::create_dir_all(root.path().join("dist")).unwrap();
        fs::write(root.path().join("dist/index.js"), "ok").unwrap();
        fs::write(root.path().join("dist/private.txt"), "no").unwrap();
        fs::create_dir_all(root.path().join("node_modules/pkg")).unwrap();
        fs::write(root.path().join("node_modules/pkg/index.js"), "no").unwrap();

        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(root.path().join("package.json")).unwrap())
                .unwrap();
        let files = package_file_list(root.path(), &manifest).unwrap();
        assert_eq!(files, ["README.md", "dist/index.js", "package.json"]);
    }
}
