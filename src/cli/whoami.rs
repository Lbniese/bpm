//! `bpm whoami` — show the registry-authenticated username (npm `whoami`).
//!
//! Reads npm config (`$HOME/.npmrc` then the project `.npmrc`), builds the
//! registry client (auth token attached automatically by the HTTP layer), and
//! calls the registry's `/-/whoami` endpoint. Prints the username on success;
//! exits nonzero with a clear message when no token is configured or the
//! registry rejects the credentials.

use std::env;
use std::path::PathBuf;

use anyhow::Result;
use bpm::config::NpmConfig;
use bpm::http::HttpClient;
use bpm::registry::{RegistryClient, RegistryError};

pub(super) struct Options {
    pub registry: Option<String>,
}

pub(super) fn run(opts: Options) -> Result<()> {
    let cwd = env::current_dir()?;
    let home = env::var_os("HOME").map(PathBuf::from);
    let config = NpmConfig::load(&cwd, home.as_deref())
        .map_err(|e| anyhow::anyhow!("failed to load npm config: {e}"))?;

    let effective_registry = opts
        .registry
        .or_else(|| env::var_os("BPM_REGISTRY").map(|s| s.to_string_lossy().into_owned()));
    let config = match effective_registry {
        Some(r) => config
            .with_registry_override(&r)
            .map_err(|e| anyhow::anyhow!("invalid registry override: {e}"))?,
        None => config,
    };

    // A token is required for whoami; surface a clear error before any request
    // when none is configured for the registry URL.
    if !config.has_auth_for_url(config.registry()) {
        anyhow::bail!(
            "not authenticated: no auth token for {} (add one in .npmrc)",
            config.registry()
        );
    }

    let http = HttpClient::new(config.clone());
    let client = RegistryClient::with_client(config, http);
    let username = client.whoami().map_err(|e| match e {
        RegistryError::BadStatus { code, .. } if code == 401 || code == 403 => {
            anyhow::anyhow!("not authenticated: registry rejected the token (HTTP {code})")
        }
        other => anyhow::anyhow!("{other}"),
    })?;

    println!("{username}");
    Ok(())
}
