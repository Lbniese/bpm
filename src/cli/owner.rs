//! `bpm owner` — manage package owners/collaborators (npm `owner`).
//!
//! - `bpm owner ls [pkg]` — list a package's maintainers (defaults to the
//!   name in the local `package.json`). Public packages need no auth.
//! - `bpm owner add <user> [pkg]` — grant `user` write access. Requires
//!   owner rights.
//! - `bpm owner rm <user> [pkg]` — remove `user`. Requires owner rights.

use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use bpm::config::NpmConfig;
use bpm::http::HttpClient;
use bpm::registry::{Maintainer, RegistryClient, RegistryError};

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
            let maintainers = client.maintainers(&name).map_err(map_err)?;
            if opts.json {
                println!("{}", serde_json::to_string_pretty(&maintainers)?);
            } else if maintainers.is_empty() {
                println!("{name} has no owners");
            } else {
                for m in &maintainers {
                    print_maintainer(m);
                }
            }
            Ok(())
        }
        "add" | "set" => {
            require_auth(&client)?;
            let user = opts.target.as_deref().ok_or_else(|| {
                anyhow::anyhow!("add requires <user>: `bpm owner add <user> [pkg]`")
            })?;
            let name = resolve_name(&cwd, opts.value.as_deref())?;
            client.add_owner(&name, user).map_err(map_err)?;
            println!("+{user}: {name}");
            Ok(())
        }
        "rm" | "remove" | "delete" => {
            require_auth(&client)?;
            let user = opts.target.as_deref().ok_or_else(|| {
                anyhow::anyhow!("rm requires <user>: `bpm owner rm <user> [pkg]`")
            })?;
            let name = resolve_name(&cwd, opts.value.as_deref())?;
            client.remove_owner(&name, user).map_err(map_err)?;
            println!("-{user}: {name}");
            Ok(())
        }
        other => anyhow::bail!("unknown owner action {other:?}; expected one of: ls, add, rm"),
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

/// Resolve the package name: an explicit target, else the `name` in the local
/// `package.json`.
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

/// Print one maintainer as `name <email>` (omitting the email when absent),
/// matching npm's `owner ls` output.
fn print_maintainer(m: &Maintainer) {
    match &m.email {
        Some(email) if !email.is_empty() => println!("{} <{email}>", m.name),
        _ => println!("{}", m.name),
    }
}

fn map_err(e: RegistryError) -> anyhow::Error {
    match e {
        RegistryError::BadStatus { code, .. } if code == 401 || code == 403 => {
            anyhow::anyhow!("not authenticated: registry rejected the token (HTTP {code})")
        }
        other => anyhow::anyhow!("{other}"),
    }
}
