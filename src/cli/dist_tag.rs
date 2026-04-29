//! `bpm dist-tag` — manage package distribution tags (npm `dist-tag`).
//!
//! - `bpm dist-tag ls [pkg]` — list a package's dist-tags (defaults to the
//!   name in the local `package.json`). Public packages need no auth.
//! - `bpm dist-tag add <pkg>@<version> [tag]` — point `tag` (default
//!   `latest`) at a version. Requires publish rights.
//! - `bpm dist-tag rm <pkg> <tag>` — remove a tag. Requires publish rights.

use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use bpm::config::NpmConfig;
use bpm::http::HttpClient;
use bpm::registry::{RegistryClient, RegistryError};

pub(crate) struct Options {
    pub action: Option<String>,
    pub target: Option<String>,
    pub value: Option<String>,
    pub registry: Option<String>,
    pub json: bool,
}

pub(crate) fn run(opts: Options) -> Result<()> {
    let cwd = env::current_dir()?;
    let home = env::var_os("HOME").map(PathBuf::from);
    let config = NpmConfig::load(&cwd, home.as_deref())
        .map_err(|e| anyhow::anyhow!("failed to load npm config: {e}"))?;

    let effective_registry = opts
        .registry
        .clone()
        .or_else(|| env::var_os("BPM_REGISTRY").map(|s| s.to_string_lossy().into_owned()));
    let config = match effective_registry {
        Some(r) => config
            .with_registry_override(&r)
            .map_err(|e| anyhow::anyhow!("invalid registry override: {e}"))?,
        None => config,
    };

    let http = HttpClient::new(config.clone());
    let client = RegistryClient::with_client(config, http);

    let action = opts.action.as_deref().unwrap_or("ls");
    match action {
        "ls" | "list" => {
            // Listing is a public read; no auth preflight.
            let name = resolve_name(&cwd, opts.target.as_deref())?;
            let tags = client.dist_tags(&name).map_err(map_err)?;
            if opts.json {
                println!("{}", serde_json::to_string_pretty(&tags)?);
            } else if tags.is_empty() {
                println!("{name} has no dist-tags");
            } else {
                for (tag, version) in &tags {
                    println!("{tag}: {version}");
                }
            }
            Ok(())
        }
        "add" | "set" => {
            require_auth(&client)?;
            let spec = opts.target.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "add requires <pkg>@<version>: `bpm dist-tag add mypkg@1.2.3 [tag]`"
                )
            })?;
            let (name, version) = parse_versioned_spec(spec)?;
            let tag = opts.value.clone().unwrap_or_else(|| "latest".to_string());
            client
                .set_dist_tag(&name, &tag, &version)
                .map_err(map_err)?;
            println!("+{tag}: {name}@{version}");
            Ok(())
        }
        "rm" | "remove" | "delete" => {
            require_auth(&client)?;
            let name = opts.target.as_deref().ok_or_else(|| {
                anyhow::anyhow!("rm requires <pkg> <tag>: `bpm dist-tag rm mypkg mytag`")
            })?;
            let tag = opts.value.as_deref().ok_or_else(|| {
                anyhow::anyhow!("rm requires <pkg> <tag>: `bpm dist-tag rm mypkg mytag`")
            })?;
            client.delete_dist_tag(name, tag).map_err(map_err)?;
            println!("-{tag}: {name}");
            Ok(())
        }
        other => anyhow::bail!("unknown dist-tag action {other:?}; expected one of: ls, add, rm"),
    }
}

/// Require an authenticated session for mutating operations.
fn require_auth(client: &RegistryClient) -> Result<()> {
    let registry = client.registry();
    if !client.config().has_auth_for_url(registry) {
        anyhow::bail!("not authenticated: no auth token for {registry} (add one in .npmrc)");
    }
    Ok(())
}

/// Resolve the package name for `ls`: an explicit target, else the `name` in
/// the local `package.json`.
fn resolve_name(cwd: &std::path::Path, target: Option<&str>) -> Result<String> {
    if let Some(name) = target {
        return Ok(name.to_string());
    }
    let path = cwd.join("package.json");
    let text = fs::read_to_string(&path).map_err(|e| {
        anyhow::anyhow!(
            "no package name given and could not read {}: {e}",
            path.display()
        )
    })?;
    let manifest = bpm::manifest::PackageManifest::from_json(&text, &path)?;
    manifest
        .name
        .ok_or_else(|| anyhow::anyhow!("no package name given and package.json has no name"))
}

/// Split `<pkg>@<version>` (e.g. `@scope/name@1.2.3`) into name and version.
fn parse_versioned_spec(spec: &str) -> Result<(String, String)> {
    let idx = spec.rfind('@').ok_or_else(|| {
        anyhow::anyhow!("expected <pkg>@<version> (e.g. mypkg@1.2.3), got {spec:?}")
    })?;
    let (name, rest) = spec.split_at(idx);
    let version = &rest[1..]; // drop the '@'
    if name.is_empty() {
        anyhow::bail!("missing package name in {spec:?}");
    }
    if version.is_empty() {
        anyhow::bail!("missing version in {spec:?}");
    }
    Ok((name.to_string(), version.to_string()))
}

fn map_err(e: RegistryError) -> anyhow::Error {
    match e {
        RegistryError::BadStatus { code, .. } if code == 401 || code == 403 => {
            anyhow::anyhow!("not authenticated: registry rejected the token (HTTP {code})")
        }
        other => anyhow::anyhow!("{other}"),
    }
}
