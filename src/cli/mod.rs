//! CLI parsing and command dispatch.

mod args;
mod audit;
mod bench;
mod cache;
mod doctor;
mod exec;
mod fetch;
mod gc;
mod import;
mod init;
mod install;
mod link;
mod ls;
mod mutate;
mod outdated;
mod publish;
mod run;
mod view;
mod why;

use std::process::ExitCode;

use args::{Cli, Commands};
use clap::Parser;

pub(crate) fn run() -> ExitCode {
    let command = Cli::parse().command;
    if let Commands::Exec { command, args } = command {
        return match exec::run(&command, &args) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("error: {error:#}");
                ExitCode::FAILURE
            }
        };
    }

    let result = match command {
        Commands::Doctor { json } => doctor::run(json),
        Commands::Gc {
            older_than,
            max_size,
            store,
        } => gc::run(older_than, max_size, store),
        Commands::Cache { action, store } => cache::run(cache::Options { action, store }),
        Commands::Fetch {
            target,
            integrity,
            registry,
            store,
            no_extract,
            json_metrics,
            offline,
            prefer_offline,
            prefer_online,
            remote_cache,
        } => fetch::run(
            &target,
            integrity,
            registry,
            store,
            no_extract,
            json_metrics,
            fetch::resolve_cache_mode(offline, prefer_offline, prefer_online),
            remote_cache,
        ),
        Commands::Bench {
            fixture,
            scenario,
            tools,
            require_tools,
            runs,
            json,
            save_baseline,
            compare_baseline,
            baseline_informational,
            regression_envelope,
            profile_bpm,
            list,
        } => bench::run(bench::Options {
            fixture,
            scenario,
            tools,
            require_tools,
            runs,
            json,
            save_baseline,
            compare_baseline,
            baseline_informational,
            regression_envelope,
            profile_bpm,
            list,
        }),
        Commands::Import { path, out, json } => import::run(path, out, json),
        Commands::Init {
            yes,
            force,
            name,
            version,
            description,
            entry,
            license,
            author,
            repository,
            test_script,
        } => init::run(init::Options {
            yes,
            force,
            name,
            version,
            description,
            entry,
            license,
            author,
            repository,
            test_script,
        }),
        Commands::Publish {
            registry,
            access,
            otp,
            provenance,
        } => publish::run(registry, access, otp, provenance),
        Commands::Audit {
            registry,
            json,
            offline,
            audit_level,
        } => audit::run(registry, json, offline, &audit_level),
        Commands::Install {
            targets,
            frozen,
            registry,
            store,
            concurrency,
            json_metrics,
            global,
            save_dev,
            save_exact,
            ignore_scripts,
            derived_store,
            git_prepare,
            legacy_peer_deps,
            offline,
            prefer_offline,
            prefer_online,
            remote_cache,
        } => install::run(install::Options {
            targets,
            frozen,
            registry,
            store,
            concurrency,
            json_metrics,
            global,
            ignore_scripts,
            derived_store,
            git_prepare,
            legacy_peer_deps,
            cache_mode: fetch::resolve_cache_mode(offline, prefer_offline, prefer_online),
            remote_cache,
            save_dev,
            save_exact,
        }),
        Commands::Ci {
            registry,
            store,
            concurrency,
            json_metrics,
            ignore_scripts,
            derived_store,
            git_prepare,
            legacy_peer_deps,
            offline,
            prefer_offline,
            prefer_online,
            remote_cache,
        } => install::run(install::Options {
            targets: Vec::new(),
            frozen: true,
            registry,
            store,
            concurrency,
            json_metrics,
            global: false,
            ignore_scripts,
            derived_store,
            git_prepare,
            legacy_peer_deps,
            cache_mode: fetch::resolve_cache_mode(offline, prefer_offline, prefer_online),
            remote_cache,
            save_dev: false,
            save_exact: false,
        }),
        Commands::Bin { global: _ } => (|| -> anyhow::Result<()> {
            println!("{}", install::bin_dir()?.display());
            Ok(())
        })(),
        Commands::Root { global } => (|| -> anyhow::Result<()> {
            if global {
                println!("{}", fetch::store_root(None)?.display());
            } else {
                let cwd = std::env::current_dir()?;
                println!(
                    "{}",
                    bpm::project::find_project_root(&cwd)?
                        .join("node_modules")
                        .display()
                );
            }
            Ok(())
        })(),
        Commands::Prefix { global } => (|| -> anyhow::Result<()> {
            if global {
                println!("{}", fetch::store_root(None)?.display());
            } else {
                let cwd = std::env::current_dir()?;
                println!("{}", bpm::project::find_project_root(&cwd)?.display());
            }
            Ok(())
        })(),
        Commands::Run { script } => run::run(&script),
        Commands::Outdated {
            target,
            registry,
            store,
            offline,
            json,
        } => outdated::run(target, registry, store, offline, json),
        Commands::View {
            package,
            field,
            registry,
            store,
            offline,
            json,
        } => view::run(view::Options {
            package,
            field,
            registry,
            store,
            offline,
            json,
        }),
        Commands::Why { target } => why::execute(&target),
        Commands::Ls {
            name,
            all,
            depth,
            json,
        } => ls::run(ls::Options {
            filter: name,
            all,
            depth,
            json,
        }),
        Commands::Link {
            target,
            store,
            registry,
        } => link::run(link::Options {
            target,
            store,
            registry,
        }),
        Commands::Unlink {
            name,
            global,
            store,
            registry,
        } => link::run_unlink(link::UnlinkOptions {
            name,
            global,
            store,
            registry,
        }),
        Commands::Uninstall {
            names,
            registry,
            store,
            concurrency,
            json_metrics,
            ignore_scripts,
            derived_store,
            git_prepare,
            legacy_peer_deps,
            offline,
            prefer_offline,
            prefer_online,
            global,
            remote_cache,
        } => mutate::run_uninstall(mutate::UninstallOptions {
            names,
            registry,
            store,
            concurrency,
            json_metrics,
            ignore_scripts,
            derived_store,
            git_prepare,
            legacy_peer_deps,
            cache_mode: fetch::resolve_cache_mode(offline, prefer_offline, prefer_online),
            remote_cache,
            global,
        }),
        Commands::Upgrade {
            names,
            registry,
            store,
            concurrency,
            json_metrics,
            ignore_scripts,
            derived_store,
            git_prepare,
            legacy_peer_deps,
            offline,
            prefer_offline,
            prefer_online,
            remote_cache,
        } => mutate::run_upgrade(mutate::UpgradeOptions {
            names,
            registry,
            store,
            concurrency,
            json_metrics,
            ignore_scripts,
            derived_store,
            git_prepare,
            legacy_peer_deps,
            cache_mode: fetch::resolve_cache_mode(offline, prefer_offline, prefer_online),
            remote_cache,
        }),
        Commands::Dedupe {
            registry,
            store,
            concurrency,
            json_metrics,
            ignore_scripts,
            derived_store,
            git_prepare,
            legacy_peer_deps,
            offline,
            prefer_offline,
            prefer_online,
            remote_cache,
        } => mutate::run_dedupe(mutate::DedupeOptions {
            registry,
            store,
            concurrency,
            json_metrics,
            ignore_scripts,
            derived_store,
            git_prepare,
            legacy_peer_deps,
            cache_mode: fetch::resolve_cache_mode(offline, prefer_offline, prefer_online),
            remote_cache,
        }),
        Commands::Exec { .. } => unreachable!("exec handled before result-based commands"),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
