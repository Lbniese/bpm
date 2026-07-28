//! `bpm token` — manage registry authentication tokens (npm `token`).
//!
//! Reads npm config, builds the registry client (bearer token attached
//! automatically by the HTTP layer), and talks to npm's `/-/npm/v1/tokens`
//! endpoint:
//!
//! - `bpm token` / `bpm token list` — list tokens (`key`, readonly, cidr,
//!   created).
//! - `bpm token create [--read-only] [--cidr CIDR] [--password PASS] [--otp]`
//!   — mint a new token. npm requires re-authentication with the account
//!   password (pass `--password` or set `$BPM_PASSWORD`); pass `--otp` when the
//!   account enforces 2FA.
//! - `bpm token revoke <id>` — revoke a token by the `key` shown by `list`.

use std::env;
use std::path::PathBuf;

use anyhow::Result;
use bpm::config::NpmConfig;
use bpm::http::HttpClient;
use bpm::registry::{CreateTokenRequest, RegistryClient, RegistryError, RegistryToken};

pub(crate) struct Options {
    pub action: Option<String>,
    pub id: Option<String>,
    pub registry: Option<String>,
    pub read_only: bool,
    pub cidr: Vec<String>,
    pub password: Option<String>,
    pub otp: Option<String>,
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

    // All token operations require an authenticated session.
    if !config.has_auth_for_url(config.registry()) {
        anyhow::bail!(
            "not authenticated: no auth token for {} (add one in .npmrc)",
            config.registry()
        );
    }

    let http = HttpClient::new(config.clone());
    let client = RegistryClient::with_client(config, http);

    let action = opts.action.as_deref().unwrap_or("list");
    match action {
        "list" | "ls" => {
            let tokens = client.list_tokens().map_err(map_err)?;
            if opts.json {
                println!("{}", serde_json::to_string_pretty(&tokens)?);
            } else {
                print_token_table(&tokens);
            }
            Ok(())
        }
        "create" | "new" => {
            let password = opts
                .password
                .clone()
                .or_else(|| env::var_os("BPM_PASSWORD").map(|s| s.to_string_lossy().into_owned()));
            let password = password.ok_or_else(|| {
                anyhow::anyhow!(
                    "creating a token requires a password: pass --password or set $BPM_PASSWORD"
                )
            })?;
            let req = CreateTokenRequest {
                password,
                readonly: opts.read_only,
                cidrs: opts.cidr.clone(),
            };
            let token = client
                .create_token(&req, opts.otp.as_deref())
                .map_err(map_err)?;
            if opts.json {
                println!("{}", serde_json::to_string_pretty(&token)?);
            } else {
                print_token(&token, true);
            }
            Ok(())
        }
        "revoke" | "rm" | "delete" => {
            let id = opts.id.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "revoke requires a token id: `bpm token revoke <id>` (see `bpm token list`)"
                )
            })?;
            client
                .revoke_token(&id, opts.otp.as_deref())
                .map_err(map_err)?;
            println!("revoked token {id}");
            Ok(())
        }
        other => {
            anyhow::bail!("unknown token action {other:?}; expected one of: list, create, revoke")
        }
    }
}

/// Translate a registry error into a user-facing anyhow error, calling out
/// credential rejection (401/403) distinctly from other failures.
fn map_err(e: RegistryError) -> anyhow::Error {
    match e {
        RegistryError::BadStatus { code, .. } if code == 401 || code == 403 => {
            anyhow::anyhow!("not authenticated: registry rejected the token (HTTP {code})")
        }
        other => anyhow::anyhow!("{other}"),
    }
}

fn print_token_table(tokens: &[RegistryToken]) {
    if tokens.is_empty() {
        println!("no tokens");
        return;
    }
    println!("{:<24} {:<8} {:<20} created", "id", "readonly", "cidr");
    for t in tokens {
        let id = t.key.as_deref().unwrap_or("-");
        let ro = if t.readonly { "yes" } else { "no" };
        let cidr = t
            .cidr_whitelist
            .as_ref()
            .map(|c| c.join(","))
            .unwrap_or_default();
        let created = t.created.as_deref().unwrap_or("-");
        println!("{:<24} {:<8} {:<20} {}", id, ro, cidr, created);
    }
}

fn print_token(t: &RegistryToken, with_token: bool) {
    if let Some(k) = &t.key {
        println!("id:       {k}");
    }
    if with_token {
        if let Some(tok) = &t.token {
            println!("token:    {tok}");
        }
    }
    println!("readonly: {}", t.readonly);
    if let Some(c) = &t.cidr_whitelist {
        if !c.is_empty() {
            println!("cidr:     {}", c.join(","));
        }
    }
    if let Some(created) = &t.created {
        println!("created:  {created}");
    }
}
