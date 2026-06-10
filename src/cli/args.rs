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
    /// Install from `bpm.lock`, or fetch a package and link its declared bins.
    #[command(alias = "i", alias = "add")]
    Install {
        /// Package spec, URL, or `file://` path. Omit to install `bpm.lock`.
        target: Option<String>,
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
        /// Do not run lifecycle scripts.
        #[arg(long)]
        ignore_scripts: bool,
        /// Cache lifecycle-derived package images per dependency closure, so a
        /// package's scripts never re-run when another graph shares its closure
        /// (experimental; default off).
        #[arg(long)]
        derived_store: bool,
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
}
