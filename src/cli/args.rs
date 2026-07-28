//! Command-line contracts for the `bpm` binary.

use std::{ffi::OsString, path::PathBuf};

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "bpm",
    bin_name = "bpm",
    about = "Bloom Package Manager: an npm-compatible, performance-focused package installer",
    version
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
    /// Diagnose the current project's package.json.
    Doctor {
        /// Emit machine-readable JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Reclaim unreferenced objects from the global store.
    Gc {
        /// Retain objects newer than this age (default: 30d).
        #[arg(long = "older-than")]
        older_than: Option<String>,
        /// Reclaim enough eligible objects to fit within this size.
        #[arg(long = "max-size")]
        max_size: Option<String>,
        /// Store root (defaults to `$BPM_STORE` or `$HOME/.bpm`).
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// Download, verify, store, and extract a package by spec or exact URL.
    Fetch {
        /// Package spec or an exact tarball URL / `file://` path.
        target: String,
        /// Expected integrity string (`sha512-<base64>`).
        #[arg(long)]
        integrity: Option<String>,
        /// Registry base URL for spec resolution.
        #[arg(long)]
        registry: Option<String>,
        /// Store root (defaults to `$BPM_STORE` or `$HOME/.bpm`).
        #[arg(long)]
        store: Option<PathBuf>,
        /// Do not extract the package image (archive only).
        #[arg(long = "no-extract")]
        no_extract: bool,
        /// Write phase metrics as canonical JSON to `PATH`.
        #[arg(long = "json-metrics")]
        json_metrics: Option<PathBuf>,
        /// Never contact the registry; resolve only against cached metadata.
        #[arg(long)]
        offline: bool,
        /// Prefer cached metadata without revalidation; fetch only on a miss.
        #[arg(long)]
        prefer_offline: bool,
        /// Always revalidate cached metadata against the registry.
        #[arg(long)]
        prefer_online: bool,
        /// Optional verified read-through cache for raw artifacts.
        #[arg(long)]
        remote_cache: Option<String>,
    },
    /// Run benchmark scenarios and report timing statistics.
    Bench {
        /// Fixture to benchmark (list to show available).
        #[arg(long, default_value = "minimal")]
        fixture: String,
        /// Scenario to run (default: all).
        #[arg(long)]
        scenario: Option<String>,
        /// Tools to include (comma-separated, default: npm,pnpm,bpm).
        #[arg(long, default_value = "npm,pnpm,bpm")]
        tools: String,
        /// Fail before running if any requested benchmark tool is unavailable.
        #[arg(long = "require-tools")]
        require_tools: bool,
        /// Number of iterations per scenario.
        #[arg(long, default_value_t = 3)]
        runs: usize,
        /// Write JSON results to PATH instead of text.
        #[arg(long)]
        json: Option<PathBuf>,
        /// Write a machine/date-stamped baseline JSON file under this directory.
        #[arg(long = "save-baseline")]
        save_baseline: Option<PathBuf>,
        /// Compare the current run against a semantic baseline JSON file.
        #[arg(long = "compare-baseline")]
        compare_baseline: Option<PathBuf>,
        /// Allow cross-machine or version-mismatched baseline comparisons as informational output.
        #[arg(long = "baseline-informational")]
        baseline_informational: bool,
        /// Maximum allowed current/baseline median ratio for baseline comparison.
        #[arg(long = "regression-envelope", default_value_t = 2.0)]
        regression_envelope: f64,
        /// Write separate diagnostic BPM phase profiles under this directory.
        #[arg(long = "profile-bpm")]
        profile_bpm: Option<PathBuf>,
        /// List available scenarios and fixtures.
        #[arg(long)]
        list: bool,
    },
    /// Import an npm `package-lock.json` and emit a canonical `bpm.lock`.
    Import {
        /// Input lockfile path (defaults to `./package-lock.json`).
        path: Option<PathBuf>,
        /// Output `bpm.lock` path (defaults to `<input dir>/bpm.lock`).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Emit machine-readable JSON to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Create a new `package.json` in the current directory (npm `init` compatibility).
    Init {
        /// Use defaults for every field; do not prompt.
        #[arg(short = 'y', long = "yes")]
        yes: bool,
        /// Overwrite an existing `package.json`.
        #[arg(long)]
        force: bool,
        /// Package name (defaults to the current directory name).
        #[arg(long)]
        name: Option<String>,
        /// Initial version (default: 1.0.0).
        #[arg(long)]
        version: Option<String>,
        /// Description written to `package.json`.
        #[arg(long)]
        description: Option<String>,
        /// Entry point / `main` field (default: index.js).
        #[arg(long)]
        entry: Option<String>,
        /// License SPDX id (default: MIT).
        #[arg(long)]
        license: Option<String>,
        /// Author string.
        #[arg(long)]
        author: Option<String>,
        /// Git repository shorthand or URL.
        #[arg(long)]
        repository: Option<String>,
        /// Test command for the `scripts.test` field.
        #[arg(long = "test-script")]
        test_script: Option<String>,
    },
    /// Publish the current package to an npm-compatible registry.
    Publish {
        #[arg(long)]
        registry: Option<String>,
        #[arg(long)]
        access: Option<String>,
        /// One-time password for registries requiring npm two-factor auth.
        #[arg(long)]
        otp: Option<String>,
        /// Attach a minimal provenance statement to the publish document.
        #[arg(long)]
        provenance: bool,
    },
    /// Query registry advisories for the current project's dependencies.
    Audit {
        #[arg(long)]
        registry: Option<String>,
        #[arg(long)]
        json: bool,
        /// Do not contact the registry; normalize and summarize local lock data only.
        #[arg(long)]
        offline: bool,
        /// Fail when advisories at or above this severity are present.
        #[arg(long = "audit-level", default_value = "low")]
        audit_level: String,
    },
    /// Install from `bpm.lock`, add a dependency, or fetch and link a package's bins.
    #[command(alias = "i", alias = "add")]
    Install {
        /// Package spec(s) to add to the project (registry only in this slice).
        /// Omit to install the existing `bpm.lock` / `package-lock.json`.
        targets: Vec<String>,
        /// Require `package.json` and `bpm.lock` to agree.
        #[arg(long)]
        frozen: bool,
        /// Registry base URL for package-spec resolution.
        #[arg(long)]
        registry: Option<String>,
        /// Store root (defaults to `$BPM_STORE` or `$HOME/.bpm`).
        #[arg(long)]
        store: Option<PathBuf>,
        /// Max concurrent fetch + extract workers (0 selects an adaptive limit).
        #[arg(long, default_value_t = 0)]
        concurrency: usize,
        /// Write phase metrics as canonical JSON to `PATH`.
        #[arg(long = "json-metrics")]
        json_metrics: Option<PathBuf>,
        /// Install a target package into the user-level bin prefix (npm-compatible spelling).
        #[arg(short = 'g', long)]
        global: bool,
        /// Add registry targets to `devDependencies` instead of `dependencies`.
        #[arg(short = 'D', long = "save-dev")]
        save_dev: bool,
        /// Save the resolved version as an exact `X.Y.Z` instead of `^X.Y.Z`.
        #[arg(short = 'E', long = "save-exact")]
        save_exact: bool,
        /// Do not run lifecycle scripts.
        #[arg(long)]
        ignore_scripts: bool,
        /// Cache lifecycle-derived package images per dependency closure, so a
        /// package's scripts never re-run when another graph shares its closure
        /// (experimental; default off).
        #[arg(long)]
        derived_store: bool,
        /// Run npm's Git build-context `prepare` lifecycle (experimental; default on; disable with --no-git-prepare).
        #[arg(long)]
        git_prepare: bool,
        /// Ignore peer dependency conflicts.
        #[arg(long = "legacy-peer-deps")]
        legacy_peer_deps: bool,
        /// Never contact the registry; resolve only against cached metadata.
        #[arg(long)]
        offline: bool,
        /// Prefer cached metadata without revalidation; fetch only on a miss.
        #[arg(long)]
        prefer_offline: bool,
        /// Always revalidate cached metadata against the registry.
        #[arg(long)]
        prefer_online: bool,
        /// Optional verified read-through cache for raw artifacts.
        #[arg(long)]
        remote_cache: Option<String>,
    },
    /// Symlink the cwd package into the global registry (`bpm link`), or
    /// consume a globally-registered package into the current project
    /// (`bpm link <name>`). npm `link` compatibility.
    Link {
        /// Name of a globally-registered package to link into the current
        /// project. Omit to register the cwd package globally instead.
        target: Option<String>,
        /// Store root (defaults to `$BPM_STORE` or `$HOME/.bpm`).
        #[arg(long)]
        store: Option<PathBuf>,
        /// Registry base URL (passed through to the consume install step).
        #[arg(long)]
        registry: Option<String>,
    },
    /// Remove a consumed link from the current project (`bpm unlink <name>`),
    /// or unregister a package from the global registry
    /// (`bpm unlink --global [<name>]`). npm `unlink` compatibility.
    Unlink {
        /// Name of the package to unlink. With `--global`, defaults to the cwd
        /// package's name.
        name: Option<String>,
        /// Remove the package from the global registry instead of the project.
        #[arg(short = 'g', long)]
        global: bool,
        /// Store root (defaults to `$BPM_STORE` or `$HOME/.bpm`).
        #[arg(long)]
        store: Option<PathBuf>,
        /// Registry base URL (passed through to the unconsume reinstall step).
        #[arg(long)]
        registry: Option<String>,
    },
    /// Remove one or more packages from the project manifest and lock.
    #[command(alias = "remove", alias = "rm", alias = "un")]
    Uninstall {
        /// Package name(s) to remove from every dependency section.
        #[arg(required = true)]
        names: Vec<String>,
        /// Registry base URL for package-spec resolution.
        #[arg(long)]
        registry: Option<String>,
        /// Store root (defaults to `$BPM_STORE` or `$HOME/.bpm`).
        #[arg(long)]
        store: Option<PathBuf>,
        /// Max concurrent fetch + extract workers (0 selects an adaptive limit).
        #[arg(long, default_value_t = 0)]
        concurrency: usize,
        /// Write phase metrics as canonical JSON to `PATH`.
        #[arg(long = "json-metrics")]
        json_metrics: Option<PathBuf>,
        /// Do not run lifecycle scripts.
        #[arg(long)]
        ignore_scripts: bool,
        /// Cache lifecycle-derived package images per dependency closure
        /// (experimental; default off).
        #[arg(long)]
        derived_store: bool,
        /// Run npm's Git build-context `prepare` lifecycle (experimental; default on; disable with --no-git-prepare).
        #[arg(long)]
        git_prepare: bool,
        /// Ignore peer dependency conflicts.
        #[arg(long = "legacy-peer-deps")]
        legacy_peer_deps: bool,
        /// Never contact the registry; resolve only against cached metadata.
        #[arg(long)]
        offline: bool,
        /// Prefer cached metadata without revalidation; fetch only on a miss.
        #[arg(long)]
        prefer_offline: bool,
        /// Always revalidate cached metadata against the registry.
        #[arg(long)]
        prefer_online: bool,
        /// Optional verified read-through cache for raw artifacts.
        #[arg(long)]
        remote_cache: Option<String>,
        /// Rejected: global-bin ownership metadata does not exist yet, so
        /// deleting by filename would be unsafe.
        #[arg(short = 'g', long)]
        global: bool,
    },
    /// Re-resolve within declared ranges and bump locked versions to the
    /// newest satisfying ones (npm `upgrade` compatibility). Does not edit
    /// `package.json` ranges.
    Upgrade {
        /// Package name(s) to upgrade (omit to upgrade all within their ranges).
        names: Vec<String>,
        /// Registry base URL for package-spec resolution.
        #[arg(long)]
        registry: Option<String>,
        /// Store root (defaults to `$BPM_STORE` or `$HOME/.bpm`).
        #[arg(long)]
        store: Option<PathBuf>,
        /// Max concurrent fetch + extract workers (0 selects an adaptive limit).
        #[arg(long, default_value_t = 0)]
        concurrency: usize,
        /// Write phase metrics as canonical JSON to `PATH`.
        #[arg(long = "json-metrics")]
        json_metrics: Option<PathBuf>,
        /// Do not run lifecycle scripts.
        #[arg(long)]
        ignore_scripts: bool,
        /// Cache lifecycle-derived package images per dependency closure
        /// (experimental; default off).
        #[arg(long)]
        derived_store: bool,
        /// Run npm's Git build-context `prepare` lifecycle (experimental; default on; disable with --no-git-prepare).
        #[arg(long)]
        git_prepare: bool,
        /// Ignore peer dependency conflicts.
        #[arg(long = "legacy-peer-deps")]
        legacy_peer_deps: bool,
        /// Never contact the registry; resolve only against cached metadata.
        #[arg(long)]
        offline: bool,
        /// Prefer cached metadata without revalidation; fetch only on a miss.
        #[arg(long)]
        prefer_offline: bool,
        /// Always revalidate cached metadata against the registry.
        #[arg(long)]
        prefer_online: bool,
        /// Optional verified read-through cache for raw artifacts.
        #[arg(long)]
        remote_cache: Option<String>,
    },
    /// Re-resolve to minimize duplicate versions and rewrite the lock
    /// (npm `dedupe` compatibility).
    Dedupe {
        /// Registry base URL for package-spec resolution.
        #[arg(long)]
        registry: Option<String>,
        /// Store root (defaults to `$BPM_STORE` or `$HOME/.bpm`).
        #[arg(long)]
        store: Option<PathBuf>,
        /// Max concurrent fetch + extract workers (0 selects an adaptive limit).
        #[arg(long, default_value_t = 0)]
        concurrency: usize,
        /// Write phase metrics as canonical JSON to `PATH`.
        #[arg(long = "json-metrics")]
        json_metrics: Option<PathBuf>,
        /// Do not run lifecycle scripts.
        #[arg(long)]
        ignore_scripts: bool,
        /// Cache lifecycle-derived package images per dependency closure
        /// (experimental; default off).
        #[arg(long)]
        derived_store: bool,
        /// Run npm's Git build-context `prepare` lifecycle (experimental; default on; disable with --no-git-prepare).
        #[arg(long)]
        git_prepare: bool,
        /// Ignore peer dependency conflicts.
        #[arg(long = "legacy-peer-deps")]
        legacy_peer_deps: bool,
        /// Never contact the registry; resolve only against cached metadata.
        #[arg(long)]
        offline: bool,
        /// Prefer cached metadata without revalidation; fetch only on a miss.
        #[arg(long)]
        prefer_offline: bool,
        /// Always revalidate cached metadata against the registry.
        #[arg(long)]
        prefer_online: bool,
        /// Optional verified read-through cache for raw artifacts.
        #[arg(long)]
        remote_cache: Option<String>,
    },
    /// Clean install from `bpm.lock` (npm `ci` compatibility).
    Ci {
        /// Registry base URL for package-spec resolution.
        #[arg(long)]
        registry: Option<String>,
        /// Store root (defaults to `$BPM_STORE` or `$HOME/.bpm`).
        #[arg(long)]
        store: Option<PathBuf>,
        /// Max concurrent fetch + extract workers (0 selects an adaptive limit).
        #[arg(long, default_value_t = 0)]
        concurrency: usize,
        /// Write phase metrics as canonical JSON to `PATH`.
        #[arg(long = "json-metrics")]
        json_metrics: Option<PathBuf>,
        /// Do not run lifecycle scripts.
        #[arg(long)]
        ignore_scripts: bool,
        /// Cache lifecycle-derived package images per dependency closure, so a
        /// package's scripts never re-run when another graph shares its closure
        /// (experimental; default off).
        #[arg(long)]
        derived_store: bool,
        /// Run npm's Git build-context `prepare` lifecycle (experimental; default on; disable with --no-git-prepare).
        #[arg(long)]
        git_prepare: bool,
        /// Ignore peer dependency conflicts.
        #[arg(long = "legacy-peer-deps")]
        legacy_peer_deps: bool,
        /// Never contact the registry; resolve only against cached metadata.
        #[arg(long)]
        offline: bool,
        /// Prefer cached metadata without revalidation; fetch only on a miss.
        #[arg(long)]
        prefer_offline: bool,
        /// Always revalidate cached metadata against the registry.
        #[arg(long)]
        prefer_online: bool,
        /// Optional verified read-through cache for raw artifacts.
        #[arg(long)]
        remote_cache: Option<String>,
    },
    /// Print the directory where global executable shims are linked.
    Bin {
        #[arg(short = 'g', long)]
        global: bool,
    },
    /// Print the node_modules root for the current project or global store.
    Root {
        #[arg(short = 'g', long)]
        global: bool,
    },
    /// Print the current project prefix or the global BPM prefix.
    Prefix {
        #[arg(short = 'g', long)]
        global: bool,
    },
    /// Execute a command with the nearest project's dependency bins on PATH.
    #[command(alias = "x")]
    Exec {
        /// Command to execute.
        command: OsString,
        /// Arguments passed unchanged to the command.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Run a `package.json` lifecycle script with an npm-compatible environment.
    #[command(alias = "run-script")]
    Run {
        /// Script name to run (for example `build`, `test`, or `preinstall`).
        script: String,
    },
    /// Show outdated packages (resolved vs latest registry version).
    Outdated {
        /// Package name filter (omit for all packages).
        target: Option<String>,
        /// Registry base URL.
        #[arg(long)]
        registry: Option<String>,
        /// Store root (defaults to `$BPM_STORE` or `$HOME/.bpm`).
        #[arg(long)]
        store: Option<PathBuf>,
        /// Never contact the registry; resolve only against cached metadata.
        #[arg(long)]
        offline: bool,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show registry metadata for a package (npm `view` compat).
    View {
        /// Package spec: `<name>`, `<name>@<version>`, or `<name>@<range>`.
        package: String,
        /// Optional field selector (e.g. `dependencies`, `dist.tarball`,
        /// `versions`, or `dist-tags`).
        field: Option<String>,
        /// Registry base URL.
        #[arg(long)]
        registry: Option<String>,
        /// Store root (defaults to `$BPM_STORE` or `$HOME/.bpm`).
        #[arg(long)]
        store: Option<PathBuf>,
        /// Never contact the registry; resolve only against cached metadata.
        #[arg(long)]
        offline: bool,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show why a package is in the dependency tree.
    Why {
        /// Package name to trace.
        target: String,
    },
    /// List installed packages as a dependency tree (npm `ls` compat).
    #[command(alias = "list")]
    Ls {
        /// Only show paths leading to packages matching this name.
        name: Option<String>,
        /// Show every occurrence instead of deduplicating shared packages.
        #[arg(short = 'a', long)]
        all: bool,
        /// Maximum levels to expand below the root (default: unlimited).
        #[arg(long)]
        depth: Option<usize>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::{OsStr, OsString},
        path::PathBuf,
    };

    use clap::{error::ErrorKind, Parser};

    use super::{Cli, Commands};

    #[test]
    fn exec_requires_a_command() {
        let error = Cli::try_parse_from(["bpm", "exec"]).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn exec_preserves_native_command_and_trailing_arguments() {
        let cli = Cli::try_parse_from([
            OsString::from("bpm"),
            OsString::from("exec"),
            OsString::from("fixture-command"),
            OsString::from("argument with spaces"),
            OsString::new(),
            OsString::from("--leading-flag"),
        ])
        .unwrap();

        let Commands::Exec { command, args } = cli.command else {
            panic!("expected exec command");
        };
        assert_eq!(command, OsStr::new("fixture-command"));
        assert_eq!(
            args,
            [
                OsString::from("argument with spaces"),
                OsString::new(),
                OsString::from("--leading-flag"),
            ]
        );
    }

    #[test]
    fn other_command_contracts_are_unchanged() {
        let cli = Cli::try_parse_from(["bpm", "doctor", "--json"]).unwrap();

        assert!(matches!(cli.command, Commands::Doctor { json: true }));
    }

    #[test]
    fn bench_accepts_strict_and_profile_options() {
        let cli = Cli::try_parse_from([
            "bpm",
            "bench",
            "--require-tools",
            "--profile-bpm",
            "/tmp/profile",
            "--compare-baseline",
            "/tmp/baseline.json",
        ])
        .unwrap();

        let Commands::Bench {
            require_tools,
            profile_bpm,
            compare_baseline,
            ..
        } = cli.command
        else {
            panic!("expected bench command");
        };
        assert!(require_tools);
        assert_eq!(profile_bpm, Some(PathBuf::from("/tmp/profile")));
        assert_eq!(compare_baseline, Some(PathBuf::from("/tmp/baseline.json")));
    }

    #[cfg(unix)]
    #[test]
    fn exec_preserves_non_utf8_arguments() {
        use std::os::unix::ffi::OsStringExt;

        let native_argument = OsString::from_vec(vec![b'f', 0x80, b'o']);
        let cli = Cli::try_parse_from([
            OsString::from("bpm"),
            OsString::from("exec"),
            OsString::from("fixture-command"),
            native_argument.clone(),
        ])
        .unwrap();

        let Commands::Exec { args, .. } = cli.command else {
            panic!("expected exec command");
        };
        assert_eq!(args, [native_argument]);
    }

    #[test]
    fn install_accepts_multiple_targets_and_save_flags() {
        let cli = Cli::try_parse_from([
            "bpm",
            "add",
            "--save-dev",
            "--save-exact",
            "lodash",
            "@scope/lib",
        ])
        .unwrap();

        let Commands::Install {
            targets,
            save_dev,
            save_exact,
            global,
            ..
        } = cli.command
        else {
            panic!("expected install command");
        };
        assert_eq!(
            targets,
            vec!["lodash".to_string(), "@scope/lib".to_string()]
        );
        assert!(save_dev);
        assert!(save_exact);
        assert!(!global);
    }

    #[test]
    fn install_alias_i_works() {
        let cli = Cli::try_parse_from(["bpm", "i", "lodash"]).unwrap();
        let Commands::Install { targets, .. } = cli.command else {
            panic!("expected install command");
        };
        assert_eq!(targets, vec!["lodash".to_string()]);
    }

    #[test]
    fn install_without_targets_parses() {
        let cli = Cli::try_parse_from(["bpm", "install", "--frozen"]).unwrap();
        let Commands::Install {
            targets, frozen, ..
        } = cli.command
        else {
            panic!("expected install command");
        };
        assert!(targets.is_empty());
        assert!(frozen);
    }

    #[test]
    fn install_scoped_spec_with_version_parses() {
        let cli = Cli::try_parse_from(["bpm", "add", "@scope/lib@^1.2.0"]).unwrap();
        let Commands::Install { targets, .. } = cli.command else {
            panic!("expected install command");
        };
        assert_eq!(targets, vec!["@scope/lib@^1.2.0".to_string()]);
    }

    #[test]
    fn uninstall_aliases_all_parse() {
        for alias in ["uninstall", "remove", "rm", "un"] {
            let cli = Cli::try_parse_from(["bpm", alias, "lodash", "chalk"])
                .unwrap_or_else(|_| panic!("{alias} should parse"));
            let Commands::Uninstall { names, .. } = cli.command else {
                panic!("{alias} expected uninstall command");
            };
            assert_eq!(
                names,
                vec!["lodash".to_string(), "chalk".to_string()],
                "{alias}"
            );
        }
    }

    #[test]
    fn uninstall_requires_at_least_one_name() {
        let error = Cli::try_parse_from(["bpm", "remove"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn init_yes_flag_parses() {
        let cli = Cli::try_parse_from(["bpm", "init", "-y"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Init {
                yes: true,
                force: false,
                ..
            }
        ));
    }

    #[test]
    fn init_accepts_field_overrides() {
        let cli = Cli::try_parse_from([
            "bpm",
            "init",
            "--name",
            "demo",
            "--license",
            "Apache-2.0",
            "--force",
        ])
        .unwrap();
        let Commands::Init {
            name,
            license,
            force,
            ..
        } = cli.command
        else {
            panic!("expected init command");
        };
        assert_eq!(name.as_deref(), Some("demo"));
        assert_eq!(license.as_deref(), Some("Apache-2.0"));
        assert!(force);
    }

    #[test]
    fn ls_parses_flags_and_alias() {
        let cli = Cli::try_parse_from(["bpm", "ls", "--all", "--depth", "1", "--json"]).unwrap();
        let Commands::Ls {
            name,
            all,
            depth,
            json,
        } = cli.command
        else {
            panic!("expected ls command");
        };
        assert!(name.is_none());
        assert!(all);
        assert_eq!(depth, Some(1));
        assert!(json);

        let cli = Cli::try_parse_from(["bpm", "list", "lodash"]).unwrap();
        let Commands::Ls {
            name,
            all,
            depth,
            json,
        } = cli.command
        else {
            panic!("expected ls command");
        };
        assert_eq!(name.as_deref(), Some("lodash"));
        assert!(!all);
        assert!(depth.is_none());
        assert!(!json);
    }

    #[test]
    fn view_parses_package_spec_and_field() {
        let cli = Cli::try_parse_from(["bpm", "view", "lodash", "dependencies", "--json"]).unwrap();
        let Commands::View {
            package,
            field,
            registry,
            store,
            offline,
            json,
        } = cli.command
        else {
            panic!("expected view command");
        };
        assert_eq!(package, "lodash");
        assert_eq!(field.as_deref(), Some("dependencies"));
        assert!(registry.is_none());
        assert!(store.is_none());
        assert!(!offline);
        assert!(json);
    }

    #[test]
    fn link_register_and_consume_parse() {
        let cli = Cli::try_parse_from(["bpm", "link"]).unwrap();
        let Commands::Link {
            target,
            store,
            registry,
        } = cli.command
        else {
            panic!("expected link command");
        };
        assert!(target.is_none());
        assert!(store.is_none());
        assert!(registry.is_none());

        let cli = Cli::try_parse_from(["bpm", "link", "mylib", "--store", "/tmp/s"]).unwrap();
        let Commands::Link {
            target,
            store,
            registry,
        } = cli.command
        else {
            panic!("expected link command");
        };
        assert_eq!(target.as_deref(), Some("mylib"));
        assert_eq!(store.as_deref(), Some(std::path::Path::new("/tmp/s")));
        assert!(registry.is_none());
    }

    #[test]
    fn unlink_global_and_unconsume_parse() {
        let cli = Cli::try_parse_from(["bpm", "unlink", "--global", "mylib"]).unwrap();
        let Commands::Unlink {
            name,
            global,
            store,
            registry,
        } = cli.command
        else {
            panic!("expected unlink command");
        };
        assert_eq!(name.as_deref(), Some("mylib"));
        assert!(global);
        assert!(store.is_none());
        assert!(registry.is_none());

        let cli = Cli::try_parse_from(["bpm", "unlink", "mylib"]).unwrap();
        let Commands::Unlink { name, global, .. } = cli.command else {
            panic!("expected unlink command");
        };
        assert_eq!(name.as_deref(), Some("mylib"));
        assert!(!global);
    }
}
