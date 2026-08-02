use std::collections::BTreeSet;
use std::{env, fs, path::PathBuf};

use base64::Engine;
use flate2::{write::GzEncoder, Compression};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde_json::json;
use sha2::{Digest, Sha512};

pub(super) fn run(
    registry: Option<String>,
    access: Option<String>,
    prompt_otp: bool,
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
    if !bpm::registry::is_valid_npm_name(&name) {
        anyhow::bail!("cannot publish invalid npm package name '{name}'");
    }
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
    let tarball_url = format!(
        "{}/{}/-/{}",
        config.registry(),
        name.replace('/', "%2f"),
        filename
    );
    let version_metadata =
        build_version_metadata(&manifest_json, &name, &version, &integrity, &tarball_url)?;
    let mut body = json!({
        "name": name,
        "_id": format!("{name}@{version}"),
        "versions": {
            version.clone(): version_metadata
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
    // The OTP is a secret: resolve it from `$BPM_OTP` or an optional hidden
    // prompt before sending, never from argv.
    let otp = crate::cli::credentials::optional_otp(prompt_otp)?;
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
                    "publish failed: registry requires authentication or two-factor OTP; set $BPM_OTP or rerun with --prompt-otp if 2FA is enabled"
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

fn build_version_metadata(
    manifest_json: &serde_json::Value,
    name: &str,
    version: &str,
    integrity: &str,
    tarball_url: &str,
) -> anyhow::Result<serde_json::Value> {
    let mut metadata = manifest_json
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("package manifest must be a JSON object"))?;
    metadata.insert("name".into(), json!(name));
    metadata.insert("version".into(), json!(version));
    metadata.insert("_id".into(), json!(format!("{name}@{version}")));
    metadata.insert(
        "dist".into(),
        json!({
            "integrity": integrity,
            "tarball": tarball_url,
        }),
    );
    Ok(serde_json::Value::Object(metadata))
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
    let declared_files = build_manifest_files_matcher(root, manifest_json)?;
    let ignore_patterns = load_ignore_matcher(root)?;
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.retain(|path| should_publish(path, declared_files.as_ref(), ignore_patterns.as_ref()));
    let mut set = files.into_iter().collect::<BTreeSet<_>>();
    for always in always_include(root) {
        set.insert(always);
    }
    Ok(set.into_iter().collect())
}

fn collect_files(
    root: &std::path::Path,
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
            collect_files(root, &path, out)?;
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
            validate_publish_symlink(&rel)?;
        }
    }
    Ok(())
}

fn validate_publish_symlink(rel: &str) -> anyhow::Result<()> {
    // `bpm publish` does not currently package symlink entries. This is a
    // deliberate safe default, not a traversal guard: the previous
    // implementation computed the symlink's canonicalized target and tested
    // containment under the project root, but then rejected unconditionally —
    // making the containment check unreachable dead code. Allowing in-root
    // symlinks (to match npm) is a separate product decision. Until then,
    // every symlink is rejected here with a clear message. The message
    // mentions "outside project root" for context only — no containment check
    // is performed.
    anyhow::bail!(
        "publish does not support symlink entries (symlinks are always rejected, \
         including those that may resolve outside project root): {rel}"
    )
}

fn should_publish(
    path: &str,
    declared_files: Option<&Gitignore>,
    ignore_patterns: Option<&Gitignore>,
) -> bool {
    if is_default_exclude(path) {
        return false;
    }
    if is_always_include(path) {
        return true;
    }
    if declared_files.is_some_and(|matcher| {
        !matcher
            .matched_path_or_any_parents(std::path::Path::new(path), false)
            .is_ignore()
    }) {
        return false;
    }
    !ignore_patterns.is_some_and(|matcher| {
        matcher
            .matched_path_or_any_parents(std::path::Path::new(path), false)
            .is_ignore()
    })
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
        // npm configuration files hold registry tokens at any depth and must
        // never be publishable. Match the exact basename so a name such as
        // `.npmrc.example` is unaffected.
        || lower == ".npmrc"
        || lower.ends_with("/.npmrc")
        || lower.ends_with(".tmp")
        || lower.starts_with(".git/")
        || lower.starts_with("node_modules/")
        || lower.starts_with("target/")
}

fn build_manifest_files_matcher(
    root: &std::path::Path,
    manifest_json: &serde_json::Value,
) -> anyhow::Result<Option<Gitignore>> {
    let patterns = manifest_json
        .get("files")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(normalize_manifest_path)
        .filter(|pattern| !pattern.is_empty())
        .collect::<Vec<_>>();
    if patterns.is_empty() {
        return Ok(None);
    }

    let mut builder = GitignoreBuilder::new(root);
    for pattern in patterns {
        builder.add_line(None, &pattern).map_err(|error| {
            anyhow::anyhow!("invalid package files pattern {pattern:?}: {error}")
        })?;
    }
    Ok(Some(builder.build()?))
}

fn load_ignore_matcher(root: &std::path::Path) -> anyhow::Result<Option<Gitignore>> {
    let path = if root.join(".npmignore").is_file() {
        root.join(".npmignore")
    } else {
        root.join(".gitignore")
    };
    if !path.is_file() {
        return Ok(None);
    }

    let mut builder = GitignoreBuilder::new(root);
    if let Some(error) = builder.add(&path) {
        return Err(anyhow::anyhow!(
            "failed to parse ignore file {}: {error}",
            path.display()
        ));
    }
    Ok(Some(builder.build()?))
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
    fn version_metadata_preserves_publishable_and_extension_fields() {
        let manifest = json!({
            "name": "stale-name",
            "version": "0.0.0",
            "dependencies": {"dep": "^1"},
            "optionalDependencies": {"optional": "~2"},
            "peerDependencies": {"peer": ">=3"},
            "peerDependenciesMeta": {"peer": {"optional": true}},
            "bin": {"tool": "bin/tool.js"},
            "engines": {"node": ">=20"},
            "scripts": {"test": "node test.js"},
            "os": ["linux"],
            "cpu": ["x64"],
            "exports": {".": "./index.js"},
            "x-bpm-extension": {"enabled": true}
        });
        let original = manifest.clone();

        let metadata = build_version_metadata(
            &manifest,
            "trusted-name",
            "1.2.3",
            "sha512-placeholder",
            "https://registry.example.invalid/trusted-name/-/trusted-name-1.2.3.tgz",
        )
        .unwrap();

        for key in [
            "dependencies",
            "optionalDependencies",
            "peerDependencies",
            "peerDependenciesMeta",
            "bin",
            "engines",
            "scripts",
            "os",
            "cpu",
            "exports",
            "x-bpm-extension",
        ] {
            assert_eq!(metadata[key], original[key]);
        }
        assert_eq!(manifest, original);
    }

    #[test]
    fn version_metadata_overwrites_authoritative_fields() {
        let manifest = json!({
            "name": "hostile",
            "version": "9.9.9",
            "_id": "hostile@9.9.9",
            "dist": {"integrity": "untrusted", "tarball": "https://evil.invalid/a.tgz"}
        });
        let metadata = build_version_metadata(
            &manifest,
            "trusted",
            "1.2.3",
            "sha512-trusted",
            "https://registry.example.invalid/trusted/-/trusted-1.2.3.tgz",
        )
        .unwrap();

        assert_eq!(metadata["name"], "trusted");
        assert_eq!(metadata["version"], "1.2.3");
        assert_eq!(metadata["_id"], "trusted@1.2.3");
        assert_eq!(metadata["dist"]["integrity"], "sha512-trusted");
        assert_eq!(
            metadata["dist"]["tarball"],
            "https://registry.example.invalid/trusted/-/trusted-1.2.3.tgz"
        );
    }

    #[test]
    fn version_metadata_rejects_non_object_manifest() {
        let error = build_version_metadata(
            &json!(["not", "an", "object"]),
            "name",
            "1.0.0",
            "sha512-placeholder",
            "https://registry.example.invalid/name/-/name-1.0.0.tgz",
        )
        .unwrap_err();
        assert!(error.to_string().contains("JSON object"));
    }

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

    fn write_fixture_files(root: &std::path::Path, files: &[&str]) {
        for file in files {
            let path = root.join(file);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, file).unwrap();
        }
    }

    #[test]
    fn package_file_list_supports_manifest_glob_syntax() {
        let cases = [
            (
                "dist/*.js",
                vec!["dist/index.js", "dist/index.txt", "other.js"],
                vec!["dist/index.js", "package.json"],
            ),
            (
                "dist/**/index?.js",
                vec![
                    "dist/index1.js",
                    "dist/nested/index2.js",
                    "dist/nested/index-long.js",
                ],
                vec!["dist/index1.js", "dist/nested/index2.js", "package.json"],
            ),
            (
                "lib/[ab].js",
                vec!["lib/a.js", "lib/b.js", "lib/c.js"],
                vec!["lib/a.js", "lib/b.js", "package.json"],
            ),
        ];

        for (pattern, paths, expected) in cases {
            let root = tempfile::tempdir().unwrap();
            let manifest = json!({"name": "p", "version": "1.0.0", "files": [pattern]});
            fs::write(root.path().join("package.json"), manifest.to_string()).unwrap();
            write_fixture_files(root.path(), &paths);
            assert_eq!(
                package_file_list(root.path(), &manifest).unwrap(),
                expected,
                "pattern {pattern:?}"
            );
        }
    }

    #[test]
    fn package_file_list_honors_ordered_ignore_negation_and_precedence() {
        for ignore_name in [".npmignore", ".gitignore"] {
            let root = tempfile::tempdir().unwrap();
            let manifest = json!({"name": "p", "version": "1.0.0", "files": ["dist"]});
            fs::write(root.path().join("package.json"), manifest.to_string()).unwrap();
            fs::write(root.path().join(ignore_name), "dist/*\n!dist/keep.js\n").unwrap();
            write_fixture_files(root.path(), &["dist/drop.js", "dist/keep.js"]);
            assert_eq!(
                package_file_list(root.path(), &manifest).unwrap(),
                ["dist/keep.js", "package.json"]
            );
        }

        let root = tempfile::tempdir().unwrap();
        let manifest = json!({"name": "p", "version": "1.0.0", "files": ["dist"]});
        fs::write(root.path().join("package.json"), manifest.to_string()).unwrap();
        fs::write(root.path().join(".npmignore"), "dist/drop.js\n").unwrap();
        fs::write(root.path().join(".gitignore"), "dist/keep.js\n").unwrap();
        write_fixture_files(root.path(), &["dist/drop.js", "dist/keep.js"]);
        assert_eq!(
            package_file_list(root.path(), &manifest).unwrap(),
            ["dist/keep.js", "package.json"]
        );
    }

    #[test]
    fn ignore_negation_cannot_restore_hard_exclusions() {
        let root = tempfile::tempdir().unwrap();
        let manifest = json!({"name": "p", "version": "1.0.0"});
        fs::write(root.path().join("package.json"), manifest.to_string()).unwrap();
        fs::write(
            root.path().join(".npmignore"),
            "*\n!.npmrc\n!nested/.npmrc\n!package-lock.json\n!node_modules/pkg/index.js\n!.git/config\n!target/output\n!safe.js\n",
        )
        .unwrap();
        write_fixture_files(
            root.path(),
            &[
                ".npmrc",
                "nested/.npmrc",
                "package-lock.json",
                "node_modules/pkg/index.js",
                ".git/config",
                "target/output",
                "safe.js",
            ],
        );

        assert_eq!(
            package_file_list(root.path(), &manifest).unwrap(),
            ["package.json", "safe.js"]
        );
    }

    #[test]
    fn root_always_includes_override_allowlist_and_ignore_rules() {
        let root = tempfile::tempdir().unwrap();
        let manifest = json!({"name": "p", "version": "1.0.0", "files": ["dist/*.js"]});
        fs::write(root.path().join("package.json"), manifest.to_string()).unwrap();
        fs::write(root.path().join(".npmignore"), "*\n").unwrap();
        write_fixture_files(root.path(), &["README.md", "LICENSE", "dist/index.js"]);

        assert_eq!(
            package_file_list(root.path(), &manifest).unwrap(),
            ["LICENSE", "README.md", "package.json"]
        );
    }

    #[test]
    fn package_file_list_always_excludes_npmrc() {
        // A project-level `.npmrc` can carry a registry token. It must never
        // be publishable at any depth, even when the manifest `files` array
        // explicitly names it and neither `.npmignore` nor `.gitignore`
        // excludes it. The fixture text is an obvious non-secret placeholder.
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("package.json"),
            r#"{"name":"p","version":"1.0.0","files":[".npmrc","config/.npmrc","index.js"]}"#,
        )
        .unwrap();
        fs::write(
            root.path().join(".npmrc"),
            "registry=https://example.invalid/\n",
        )
        .unwrap();
        fs::create_dir_all(root.path().join("config")).unwrap();
        fs::write(
            root.path().join("config/.npmrc"),
            "//example.invalid/:_authToken=placeholder-not-a-real-secret\n",
        )
        .unwrap();
        fs::write(root.path().join("index.js"), "module.exports = {};\n").unwrap();

        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(root.path().join("package.json")).unwrap())
                .unwrap();
        let files = package_file_list(root.path(), &manifest).unwrap();

        assert!(
            !files.iter().any(|f| f == ".npmrc"),
            "root .npmrc must never be published, got: {files:?}"
        );
        assert!(
            !files.iter().any(|f| f == "config/.npmrc"),
            "nested .npmrc must never be published, got: {files:?}"
        );
        assert!(
            files.contains(&"index.js".to_string()),
            "ordinary file must remain publishable, got: {files:?}"
        );
        assert!(
            files.contains(&"package.json".to_string()),
            "package.json must remain present via always-include, got: {files:?}"
        );
    }

    #[test]
    fn is_default_exclude_rejects_npmrc_without_touching_examples() {
        assert!(is_default_exclude(".npmrc"));
        assert!(is_default_exclude(".NPMRC"));
        assert!(is_default_exclude("config/.npmrc"));
        assert!(is_default_exclude("deep/nested/path/.npmrc"));
        // A template is not a real configuration file and must stay publishable
        // (its eventual inclusion still depends on the `files` allowlist).
        assert!(!is_default_exclude(".npmrc.example"));
        assert!(!is_default_exclude("config/.npmrc.example"));
    }

    #[test]
    fn package_file_list_honors_globbed_npmignore() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("package.json"),
            r#"{"name":"p","version":"1.0.0"}"#,
        )
        .unwrap();
        fs::write(root.path().join("README.md"), "readme").unwrap();
        fs::write(root.path().join("index.js"), "ok").unwrap();
        // A glob pattern that MUST exclude both top-level and nested .log files.
        fs::write(root.path().join(".npmignore"), "*.log\n").unwrap();
        fs::write(root.path().join("debug.log"), "no").unwrap();
        fs::create_dir_all(root.path().join("logs")).unwrap();
        fs::write(root.path().join("logs/run.log"), "no").unwrap();

        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(root.path().join("package.json")).unwrap())
                .unwrap();
        let files = package_file_list(root.path(), &manifest).unwrap();
        assert!(
            !files.iter().any(|f| f.ends_with(".log")),
            "globbed .npmignore must exclude all .log files, got: {files:?}"
        );
        assert!(files.contains(&"index.js".to_string()));
        assert!(files.contains(&"README.md".to_string()));
        assert!(files.contains(&"package.json".to_string()));
    }
}
