//! `bpm init` — scaffold a `package.json` (npm `init` compatibility).
//!
//! Resolves each field from an explicit flag, a default (`--yes`), or an
//! interactive prompt (empty input keeps the default), validates the package
//! name with the shared manifest rules, and writes a minimal `package.json`.

use std::io::{self, BufRead, Write};
use std::path::Path;

use serde::Serialize;

use bpm::manifest::is_valid_package_name;

const PACKAGE_JSON: &str = "package.json";
const DEFAULT_VERSION: &str = "1.0.0";
const DEFAULT_ENTRY: &str = "index.js";
const DEFAULT_LICENSE: &str = "MIT";
const DEFAULT_TEST: &str = r#"echo "Error: no test specified" && exit 1"#;
const FALLBACK_NAME: &str = "untitled";

/// Resolved options forwarded from the CLI dispatcher.
pub(super) struct Options {
    pub yes: bool,
    pub force: bool,
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub entry: Option<String>,
    pub license: Option<String>,
    pub author: Option<String>,
    pub repository: Option<String>,
    pub test_script: Option<String>,
}

/// A `package.json` skeleton with npm-conventional field ordering.
///
/// Optional fields are omitted entirely when unset so `bpm init -y` produces a
/// minimal file.
#[derive(Debug, Serialize)]
struct InitManifest {
    name: String,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    main: String,
    scripts: Scripts,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    license: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<String>,
}

#[derive(Debug, Serialize)]
struct Scripts {
    test: String,
}

pub(super) fn run(opts: Options) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let pkg_path = cwd.join(PACKAGE_JSON);
    if pkg_path.exists() && !opts.force {
        anyhow::bail!(
            "{PACKAGE_JSON} already exists in {}; pass --force to overwrite",
            cwd.display()
        );
    }

    let default_name = derive_default_name(&cwd);
    let mut stdin = io::BufReader::new(io::stdin());

    let name = resolve_field(
        opts.name,
        &default_name,
        "package name",
        opts.yes,
        &mut stdin,
    )?;
    if !is_valid_package_name(&name) {
        anyhow::bail!(
            "invalid package name `{name}`: use lowercase letters, digits, `.`, `-`, `_` \
             (optionally `@scope/name`)"
        );
    }
    let version = resolve_field(
        opts.version,
        DEFAULT_VERSION,
        "version",
        opts.yes,
        &mut stdin,
    )?;
    let description = resolve_field(opts.description, "", "description", opts.yes, &mut stdin)?;
    let entry = resolve_field(
        opts.entry,
        DEFAULT_ENTRY,
        "entry point",
        opts.yes,
        &mut stdin,
    )?;
    let test_script = resolve_field(
        opts.test_script,
        DEFAULT_TEST,
        "test command",
        opts.yes,
        &mut stdin,
    )?;
    let repository = resolve_field(opts.repository, "", "git repository", opts.yes, &mut stdin)?;
    let author = resolve_field(opts.author, "", "author", opts.yes, &mut stdin)?;
    let license = resolve_field(
        opts.license,
        DEFAULT_LICENSE,
        "license",
        opts.yes,
        &mut stdin,
    )?;

    let manifest = InitManifest {
        name,
        version,
        description: opt(description),
        main: entry,
        scripts: Scripts { test: test_script },
        author: opt(author),
        license,
        repository: opt(repository),
    };

    if !opts.yes {
        let preview = serde_json::to_string_pretty(&manifest)
            .map_err(|e| anyhow::anyhow!("failed to serialize package.json preview: {e}"))?;
        println!("About to write to {}:\n\n{preview}\n", pkg_path.display());
        let answer = resolve_field(None, "yes", "Is this OK?", false, &mut stdin)?;
        let confirmed = answer.is_empty()
            || answer.eq_ignore_ascii_case("yes")
            || answer.eq_ignore_ascii_case("y");
        if !confirmed {
            anyhow::bail!("aborted; no file written");
        }
    }

    let mut bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| anyhow::anyhow!("failed to serialize package.json: {e}"))?;
    // npm writes a trailing newline; match that.
    bytes.push(b'\n');
    std::fs::write(&pkg_path, &bytes)?;
    println!("Wrote {} ({} bytes)", pkg_path.display(), bytes.len());
    Ok(())
}

/// Resolve a single field: an explicit flag wins; otherwise the default when
/// `--yes`, or an interactive prompt (empty input keeps the default).
fn resolve_field(
    flag: Option<String>,
    default: &str,
    label: &str,
    yes: bool,
    stdin: &mut io::BufReader<io::Stdin>,
) -> io::Result<String> {
    if let Some(value) = flag {
        return Ok(value);
    }
    if yes {
        return Ok(default.to_string());
    }
    print!("{label} ({default}): ");
    io::stdout().flush()?;
    let mut line = String::new();
    stdin.read_line(&mut line)?;
    let trimmed = line.trim();
    Ok(if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    })
}

/// Treat an empty string as "unset" so optional fields are omitted.
fn opt(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Derive a default package name from a directory: lowercase and replace
/// non-npm characters with `-`, then strip a leading `.` or `_` (rejected by
/// `is_valid_package_name`). Falls back to `untitled` when nothing remains.
fn derive_default_name(dir: &Path) -> String {
    let Some(raw) = dir.file_name().and_then(|n| n.to_str()) else {
        return FALLBACK_NAME.to_string();
    };
    let mut sanitized: String = raw
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    sanitized = sanitized.trim_start_matches(['.', '_']).to_string();
    if sanitized.is_empty() {
        FALLBACK_NAME.to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_name_sanitizes_directory() {
        assert_eq!(
            derive_default_name(Path::new("/home/me/My Cool Proj")),
            "my-cool-proj"
        );
        assert_eq!(
            derive_default_name(Path::new("/home/me/foo.bar_baz")),
            "foo.bar_baz"
        );
    }

    #[test]
    fn default_name_strips_leading_dot_or_underscore() {
        // `is_valid_package_name` rejects leading `.` or `_`.
        let name = derive_default_name(Path::new("/tmp/.hidden-start"));
        assert!(is_valid_package_name(&name), "{name} should be valid");
        assert_eq!(name, "hidden-start");
    }

    #[test]
    fn default_name_falls_back_when_unparsable() {
        assert_eq!(derive_default_name(Path::new("/")), FALLBACK_NAME);
    }

    #[test]
    fn manifest_omits_unset_optional_fields_and_preserves_key_order() {
        let manifest = InitManifest {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            description: None,
            main: "index.js".to_string(),
            scripts: Scripts {
                test: DEFAULT_TEST.to_string(),
            },
            author: None,
            license: "MIT".to_string(),
            repository: None,
        };
        let json = serde_json::to_string(&manifest).unwrap();

        // Required fields present, optional ones omitted.
        for key in ["name", "version", "main", "scripts", "license"] {
            assert!(json.contains(&format!("\"{key}\"")), "missing key {key}");
        }
        for key in ["description", "author", "repository"] {
            assert!(
                !json.contains(&format!("\"{key}\"")),
                "unexpected key {key}"
            );
        }

        // npm-conventional ordering (assert on the raw string; serde_json::Value
        // would re-sort keys alphabetically).
        let pos = |key: &str| json.find(&format!("\"{key}\":")).unwrap();
        assert!(pos("name") < pos("version"));
        assert!(pos("version") < pos("main"));
        assert!(pos("main") < pos("scripts"));
        assert!(pos("scripts") < pos("license"));

        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["name"], "demo");
        assert_eq!(value["scripts"]["test"], DEFAULT_TEST);
    }

    #[test]
    fn manifest_includes_optional_fields_when_set() {
        let manifest = InitManifest {
            name: "demo".to_string(),
            version: "2.5.0".to_string(),
            description: Some("a thing".to_string()),
            main: "lib/index.js".to_string(),
            scripts: Scripts {
                test: "node --test".to_string(),
            },
            author: Some("Jane <jane@example.com>".to_string()),
            license: "Apache-2.0".to_string(),
            repository: Some("jane/demo".to_string()),
        };
        let json = serde_json::to_string(&manifest).unwrap();

        let pos = |key: &str| json.find(&format!("\"{key}\":")).unwrap();
        assert!(pos("description") < pos("main"));
        assert!(pos("author") < pos("license"));
        assert!(pos("license") < pos("repository"));

        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["description"], "a thing");
        assert_eq!(value["main"], "lib/index.js");
        assert_eq!(value["scripts"]["test"], "node --test");
        assert_eq!(value["author"], "Jane <jane@example.com>");
        assert_eq!(value["license"], "Apache-2.0");
        assert_eq!(value["repository"], "jane/demo");
    }
}
