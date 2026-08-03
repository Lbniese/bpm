//! `bpm link` / `bpm unlink` — npm-compatible developer linking (global two-step).
//!
//! - `bpm link` (in a package dir): register the cwd package globally as
//!   `$BPM_STORE/links/<name>` -> `<cwd>` (scoped names use
//!   `$BPM_STORE/links/@scope/pkg`).
//! - `bpm link <name>` (in a consumer project): add
//!   `<name>: "file:$BPM_STORE/links/<name>"` to `package.json` and run the
//!   normal install, which materializes `node_modules/<name>` -> the target.
//! - `bpm unlink <name>` (in a consumer): remove the dependency and reinstall
//!   (delegates to the uninstall flow).
//! - `bpm unlink --global [<name>]`: unregister (default `<name>` = cwd package).
//!
//! Re-registering a name repoints the global symlink; consumers follow the
//! repoint on their next `bpm install` because their `package.json` points at
//! the symlink (`$BPM_STORE/links/<name>`), not the resolved target, so each
//! resolution re-canonicalizes through the current registration.

use std::path::Path;

use anyhow::Context;

use bpm::link_store::LinkStore;
use bpm::manifest::PackageManifest;
use bpm::metadata_cache::CacheMode;
use bpm::project::find_project_root;

use super::fetch::store_root;
use super::install;
use super::mutate;

pub(super) struct Options {
    pub target: Option<String>,
    pub store: Option<std::path::PathBuf>,
    pub registry: Option<String>,
}

pub(super) struct UnlinkOptions {
    pub name: Option<String>,
    pub global: bool,
    pub store: Option<std::path::PathBuf>,
    pub registry: Option<String>,
}

pub(super) fn run(options: Options) -> anyhow::Result<()> {
    let store_root_path = store_root(options.store.clone())?;
    let links = LinkStore::new(&store_root_path);

    match options.target {
        // `bpm link` — register the cwd package globally.
        None => run_register(&links),
        // `bpm link <name>` — consume a registered package into the cwd project.
        Some(name) => run_consume(
            &links,
            &name,
            options.store.clone(),
            options.registry.clone(),
        ),
    }
}

pub(super) fn run_unlink(options: UnlinkOptions) -> anyhow::Result<()> {
    let store_root_path = store_root(options.store.clone())?;
    let links = LinkStore::new(&store_root_path);

    if options.global {
        // `bpm unlink --global [<name>]` — unregister from the global registry.
        run_unregister(&links, options.name.as_deref())
    } else {
        // `bpm unlink <name>` — unconsume (delegate to the uninstall flow).
        match options.name {
            Some(name) => run_unconsume(name, options.store.clone(), options.registry.clone()),
            None => anyhow::bail!(
                "bpm unlink needs a package name; use `bpm unlink <name>` to remove a link from \
                 the project, or `bpm unlink --global <name>` to unregister it"
            ),
        }
    }
}

/// Read the `name` field from `<dir>/package.json`.
fn package_name(dir: &Path) -> anyhow::Result<String> {
    let manifest = PackageManifest::from_path(&dir.join("package.json"))
        .with_context(|| format!("no readable package.json in {}", dir.display()))?;
    manifest
        .name
        .with_context(|| format!("package.json in {} has no \"name\" field", dir.display()))
}

fn run_register(links: &LinkStore) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let name = package_name(&cwd)?;
    let target = cwd.canonicalize().context("canonicalizing cwd")?;
    links.register(&name, &target)?;
    println!("registered: {name} -> {}", target.display());
    println!("to use it in a project, run `bpm link {name}` from that project");
    Ok(())
}

fn run_unregister(links: &LinkStore, name: Option<&str>) -> anyhow::Result<()> {
    let name = match name {
        Some(n) => n.to_string(),
        None => {
            let cwd = std::env::current_dir()?;
            package_name(&cwd)?
        }
    };
    if links.unregister(&name)? {
        println!("unregistered: {name}");
    } else {
        println!("nothing to unregister: no global link named '{name}'");
    }
    Ok(())
}

fn run_consume(
    links: &LinkStore,
    name: &str,
    store: Option<std::path::PathBuf>,
    registry: Option<String>,
) -> anyhow::Result<()> {
    let target = links.resolve(name)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no global link named '{name}'; run `bpm link` in that package's directory first to \
             register it"
        )
    })?;

    // Record the dependency as a `file:` spec pointing at the global symlink so
    // re-registration remains observable on the next consume/install. The
    // mutation owner stages manifest and selected lock together before install.
    let spec = format!("file:{}", links.registration_path(name)?.display());
    let install_options = install_options(store, registry);
    let outcome = mutate::run_link_consume(name, &spec, &install_options)?;
    if outcome.changed {
        println!("linked {name} -> {}", target.display());
    } else {
        println!("already linked: {name}");
    }
    Ok(())
}

fn run_unconsume(
    name: String,
    store: Option<std::path::PathBuf>,
    registry: Option<String>,
) -> anyhow::Result<()> {
    mutate::run_uninstall(mutate::UninstallOptions {
        names: vec![name.clone()],
        registry,
        store,
        concurrency: 0,
        json_metrics: None,
        ignore_scripts: false,
        derived_store: false,
        git_prepare: false,
        legacy_peer_deps: false,
        cache_mode: CacheMode::Default,
        remote_cache: None,
        global: false,
    })?;

    // The direct (Compatible) materialization path used for link entries does
    // not reconcile away a stale top-level symlink, so remove it explicitly.
    // This is safe: `run_uninstall` already dropped `<name>` from package.json,
    // so any remaining `node_modules/<name>` is stale. If a future improvement
    // to reconciliation removes it first, this is a harmless no-op.
    let cwd = std::env::current_dir()?;
    let project_root = find_project_root(&cwd).unwrap_or(cwd);
    let node_modules_entry = project_root.join("node_modules").join(&name);
    if (node_modules_entry.is_symlink() || node_modules_entry.exists())
        && std::fs::remove_file(&node_modules_entry).is_err()
    {
        // A directory symlink on Windows needs `remove_dir`.
        std::fs::remove_dir(&node_modules_entry)
            .with_context(|| format!("removing {}", node_modules_entry.display()))?;
    }
    if let Some((scope, _)) = name.split_once('/') {
        let scope_dir = project_root.join("node_modules").join(scope);
        match std::fs::remove_dir(&scope_dir) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("removing empty scope {}", scope_dir.display()));
            }
        }
    }
    Ok(())
}

/// Build a default `install::Options` for the consume/unconsume reinstall.
fn install_options(
    store: Option<std::path::PathBuf>,
    registry: Option<String>,
) -> install::Options {
    install::Options {
        targets: Vec::new(),
        frozen: false,
        registry,
        store,
        concurrency: 0,
        json_metrics: None,
        global: false,
        ignore_scripts: false,
        omit_dev: false,
        include_dev: false,
        derived_store: false,
        // Pass `false` so `install::run` applies its own env-based default
        // (on unless `BPM_GIT_PREPARE=0`), matching a plain `bpm install`.
        git_prepare: false,
        legacy_peer_deps: false,
        cache_mode: CacheMode::Default,
        remote_cache: None,
        save_dev: false,
        save_exact: false,
    }
}
