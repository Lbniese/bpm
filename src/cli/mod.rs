//! CLI parsing and command dispatch.

mod args;
mod audit;
mod bench;
mod doctor;
mod exec;
mod fetch;
mod gc;
mod import;
mod install;
mod publish;
mod run;

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
        } => fetch::run(
            &target,
            integrity,
            registry,
            store,
            no_extract,
            json_metrics,
            fetch::resolve_cache_mode(offline, prefer_offline, prefer_online),
        ),
        Commands::Bench {
            fixture,
            scenario,
            tools,
            runs,
            json,
            save_baseline,
            list,
        } => bench::run(bench::Options {
            fixture,
            scenario,
            tools,
            runs,
            json,
            save_baseline,
            list,
        }),
        Commands::Import { path, out, json } => import::run(path, out, json),
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
            target,
            frozen,
            registry,
            store,
            concurrency,
            json_metrics,
            global,
            ignore_scripts,
            legacy_peer_deps,
            offline,
            prefer_offline,
            prefer_online,
        } => install::run(install::Options {
            target,
            frozen,
            registry,
            store,
            concurrency,
            json_metrics,
            global,
            ignore_scripts,
            legacy_peer_deps,
            cache_mode: fetch::resolve_cache_mode(offline, prefer_offline, prefer_online),
        }),
        Commands::Ci {
            registry,
            store,
            concurrency,
            json_metrics,
            ignore_scripts,
            legacy_peer_deps,
            offline,
            prefer_offline,
            prefer_online,
        } => install::run(install::Options {
            target: None,
            frozen: true,
            registry,
            store,
            concurrency,
            json_metrics,
            global: false,
            ignore_scripts,
            legacy_peer_deps,
            cache_mode: fetch::resolve_cache_mode(offline, prefer_offline, prefer_online),
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
