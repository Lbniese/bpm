//! Command-line contracts for the `bpm` binary.

use std::{ffi::OsString, path::PathBuf};

use clap::{Parser, Subcommand, ValueEnum};

/// Dependency classes accepted by the bounded install omit/include surface.
///
/// Keep this typed instead of accepting arbitrary strings: this compatibility
/// slice deliberately supports only `dev`, and accepting `optional` or `peer`
/// without implementing their semantics would silently produce an incorrect
/// install tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum DependencyFilter {
    Dev,
}

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
    /// Inspect or reclaim the global artifact and metadata cache. npm `cache`
    /// compatibility.
    Cache {
        /// Operation: `ls`/`list` (default) prints a size + count breakdown;
        /// `verify` runs a repair + garbage-collection pass; `clean` reclaims
        /// every unreferenced object (protected installs are preserved).
        action: Option<String>,
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
        /// Run every tool install with `--ignore-scripts` (lifecycle parity sweep).
        #[arg(long = "ignore-scripts")]
        ignore_scripts: bool,
        /// Route all tools through a counting proxy and capture per-sample
        /// network shape. Measures network shape, not production timing.
        #[arg(long = "profile-parity")]
        profile_parity: bool,
        /// Require BPM to be faster than the named competitive tool in the
        /// same paired benchmark run (for example, `pnpm`).
        #[arg(long = "require-faster-than")]
        require_faster_than: Option<String>,
        /// Maximum paired median BPM/target ratio (default: 1.0 when gated).
        #[arg(long = "max-median-ratio")]
        max_median_ratio: Option<f64>,
        /// Maximum paired p95 BPM/target ratio (default: 1.0 when gated).
        #[arg(long = "max-p95-ratio")]
        max_p95_ratio: Option<f64>,
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
        /// Prompt interactively for a two-factor OTP (hidden input). The OTP is
        /// otherwise read from `$BPM_OTP`; it is never accepted as an argv
        /// value because argv and shell history are not secret channels.
        #[arg(long = "prompt-otp")]
        prompt_otp: bool,
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
        /// Exclude a dependency class from the installed tree. Repeatable;
        /// this slice supports only `dev`.
        #[arg(long, value_enum)]
        omit: Vec<DependencyFilter>,
        /// Include a dependency class even when it is omitted by a flag or
        /// NODE_ENV. Repeatable; this slice supports only `dev`.
        #[arg(long, value_enum)]
        include: Vec<DependencyFilter>,
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
        /// Exclude a dependency class from the installed tree. Repeatable;
        /// this slice supports only `dev`.
        #[arg(long, value_enum)]
        omit: Vec<DependencyFilter>,
        /// Include a dependency class even when it is omitted by a flag or
        /// NODE_ENV. Repeatable; this slice supports only `dev`.
        #[arg(long, value_enum)]
        include: Vec<DependencyFilter>,
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
    /// Show the registry-authenticated username (npm `whoami`).
    Whoami {
        /// Registry base URL.
        #[arg(long)]
        registry: Option<String>,
    },
    /// Manage registry authentication tokens (npm `token` compat).
    Token {
        /// Action: `list` (default), `create`, or `revoke`.
        action: Option<String>,
        /// Token id/`key` to revoke (for `revoke`; shown by `bpm token list`).
        id: Option<String>,
        /// Registry base URL.
        #[arg(long)]
        registry: Option<String>,
        /// Mint a read-only token (for `create`).
        #[arg(long = "read-only")]
        read_only: bool,
        /// CIDR whitelist entry for the new token (for `create`; repeatable).
        #[arg(long = "cidr")]
        cidr: Vec<String>,
        /// Prompt interactively for a two-factor OTP (hidden input). The OTP is
        /// otherwise read from `$BPM_OTP`; it is never accepted as an argv
        /// value because argv and shell history are not secret channels. The
        /// account password for `create` is read from `$BPM_PASSWORD` or a
        /// hidden terminal prompt automatically.
        #[arg(long = "prompt-otp")]
        prompt_otp: bool,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Manage package distribution tags (npm `dist-tag` compat).
    DistTag {
        /// Action: `ls` (default), `add`, or `rm`.
        action: Option<String>,
        /// Package name (for `ls`/`rm`) or `<pkg>@<version>` spec (for `add`).
        /// For `ls`, defaults to the name in the local `package.json`.
        target: Option<String>,
        /// Tag name (for `add`/`rm`); `add` defaults to `latest`.
        value: Option<String>,
        /// Registry base URL.
        #[arg(long)]
        registry: Option<String>,
        /// Emit machine-readable JSON (`ls`).
        #[arg(long)]
        json: bool,
    },
    /// Manage package owners/collaborators (npm `owner` compat).
    Owner {
        /// Action: `ls` (default), `add`, or `rm`.
        action: Option<String>,
        /// Package name (for `ls`) or user (for `add`/`rm`). For `ls`,
        /// defaults to the name in the local `package.json`.
        target: Option<String>,
        /// Package name (for `add`/`rm`); defaults to the local `package.json`
        /// name.
        value: Option<String>,
        /// Registry base URL.
        #[arg(long)]
        registry: Option<String>,
        /// Emit machine-readable JSON (`ls`).
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

    use clap::{error::ErrorKind, CommandFactory, Parser};

    use super::{Cli, Commands, DependencyFilter};

    #[test]
    fn documented_command_inventory() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let readme = std::fs::read_to_string(root.join("README.md")).unwrap();
        let cli_reference = std::fs::read_to_string(root.join("docs/cli.md")).unwrap();
        let command = Cli::command();
        let names = command
            .get_subcommands()
            .filter(|subcommand| !subcommand.is_hide_set())
            .map(|subcommand| subcommand.get_name())
            .collect::<Vec<_>>();

        for name in names {
            let marker = format!("`bpm {name}");
            assert!(
                readme.contains(&marker),
                "README.md does not document primary command {name}"
            );
            assert!(
                cli_reference.contains(&marker),
                "docs/cli.md does not document primary command {name}"
            );
        }

        assert_eq!(
            readme
                .lines()
                .filter(|line| *line == "## Recent Changes")
                .count(),
            1,
            "README.md must contain exactly one ## Recent Changes heading"
        );
        assert!(!readme.lines().any(|line| line == "## Changelog"));
        assert!(
            !readme.lines().any(is_dated_level_three_heading),
            "README.md must keep dated changes as flat bullets"
        );
    }

    fn is_dated_level_three_heading(line: &str) -> bool {
        let Some(date) = line.strip_prefix("### ") else {
            return false;
        };
        let bytes = date.as_bytes();
        bytes.len() == 10
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes
                .iter()
                .enumerate()
                .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
    }

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
            "--require-faster-than",
            "pnpm",
            "--max-median-ratio",
            "0.95",
            "--max-p95-ratio",
            "1.0",
        ])
        .unwrap();

        let Commands::Bench {
            require_tools,
            profile_bpm,
            compare_baseline,
            require_faster_than,
            max_median_ratio,
            max_p95_ratio,
            ..
        } = cli.command
        else {
            panic!("expected bench command");
        };
        assert!(require_tools);
        assert_eq!(profile_bpm, Some(PathBuf::from("/tmp/profile")));
        assert_eq!(compare_baseline, Some(PathBuf::from("/tmp/baseline.json")));
        assert_eq!(require_faster_than.as_deref(), Some("pnpm"));
        assert_eq!(max_median_ratio, Some(0.95));
        assert_eq!(max_p95_ratio, Some(1.0));
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
    fn install_and_ci_accept_repeatable_dev_omit_include_values() {
        let cli = Cli::try_parse_from([
            "bpm",
            "install",
            "--omit=dev",
            "--omit",
            "dev",
            "--include=dev",
        ])
        .unwrap();
        let Commands::Install { omit, include, .. } = cli.command else {
            panic!("expected install command");
        };
        assert_eq!(omit, vec![DependencyFilter::Dev, DependencyFilter::Dev]);
        assert_eq!(include, vec![DependencyFilter::Dev]);

        let cli = Cli::try_parse_from(["bpm", "ci", "--include=dev", "--omit=dev"]).unwrap();
        let Commands::Ci { omit, include, .. } = cli.command else {
            panic!("expected ci command");
        };
        assert_eq!(omit, vec![DependencyFilter::Dev]);
        assert_eq!(include, vec![DependencyFilter::Dev]);
    }

    #[test]
    fn omit_and_include_reject_unimplemented_dependency_classes() {
        for (flag, value) in [
            ("--omit=optional", "optional"),
            ("--omit=peer", "peer"),
            ("--include=optional", "optional"),
            ("--include=peer", "peer"),
        ] {
            let error = Cli::try_parse_from(["bpm", "install", flag]).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidValue, "{flag}");
            let rendered = error.to_string();
            assert!(rendered.contains(value), "{rendered}");
            assert!(rendered.contains("dev"), "{rendered}");
        }
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

    #[test]
    fn whoami_parses_registry_override() {
        let cli = Cli::try_parse_from(["bpm", "whoami"]).unwrap();
        let Commands::Whoami { registry } = cli.command else {
            panic!("expected whoami command");
        };
        assert!(registry.is_none());

        let cli =
            Cli::try_parse_from(["bpm", "whoami", "--registry", "https://reg.example"]).unwrap();
        let Commands::Whoami { registry } = cli.command else {
            panic!("expected whoami command");
        };
        assert_eq!(registry.as_deref(), Some("https://reg.example"));
    }

    #[test]
    fn token_defaults_to_list_and_parses_actions() {
        let cli = Cli::try_parse_from(["bpm", "token"]).unwrap();
        let Commands::Token { action, id, .. } = cli.command else {
            panic!("expected token command");
        };
        assert_eq!(action.as_deref(), None);
        assert!(id.is_none());

        let cli = Cli::try_parse_from([
            "bpm",
            "token",
            "create",
            "--read-only",
            "--cidr",
            "10.0.0.0/8",
            "--prompt-otp",
        ])
        .unwrap();
        let Commands::Token {
            action,
            read_only,
            cidr,
            prompt_otp,
            id,
            ..
        } = cli.command
        else {
            panic!("expected token command");
        };
        assert_eq!(action.as_deref(), Some("create"));
        assert!(read_only);
        assert_eq!(cidr, vec!["10.0.0.0/8".to_string()]);
        assert!(prompt_otp, "--prompt-otp parses as a boolean");
        assert!(id.is_none());

        let cli = Cli::try_parse_from(["bpm", "token", "revoke", "abc"]).unwrap();
        let Commands::Token { action, id, .. } = cli.command else {
            panic!("expected token command");
        };
        assert_eq!(action.as_deref(), Some("revoke"));
        assert_eq!(id.as_deref(), Some("abc"));
    }

    #[test]
    fn token_rejects_value_bearing_secret_flags() {
        // Passwords and OTPs must never be supplied through argv. Both old
        // value-bearing flags are now unknown and Clap must reject them.
        let err = Cli::try_parse_from(["bpm", "token", "create", "--password", "pw"]);
        assert!(err.is_err(), "--password must be rejected, got: {err:?}");
        let err = Cli::try_parse_from(["bpm", "token", "create", "--otp", "123456"]);
        assert!(err.is_err(), "--otp must be rejected, got: {err:?}");
        let err = Cli::try_parse_from(["bpm", "publish", "--otp", "123456"]);
        assert!(err.is_err(), "publish --otp must be rejected, got: {err:?}");
    }

    #[test]
    fn publish_prompt_otp_parses_as_boolean() {
        let cli = Cli::try_parse_from(["bpm", "publish", "--prompt-otp"])
            .expect("--prompt-otp parses for publish");
        let Commands::Publish { prompt_otp, .. } = cli.command else {
            panic!("expected publish command");
        };
        assert!(prompt_otp);

        let cli = Cli::try_parse_from(["bpm", "publish"]).unwrap();
        let Commands::Publish { prompt_otp, .. } = cli.command else {
            panic!("expected publish command");
        };
        assert!(!prompt_otp, "--prompt-otp defaults to false");
    }

    #[test]
    fn dist_tag_parses_actions_and_positionals() {
        let cli = Cli::try_parse_from(["bpm", "dist-tag"]).unwrap();
        let Commands::DistTag {
            action,
            target,
            value,
            ..
        } = cli.command
        else {
            panic!("expected dist-tag command");
        };
        assert_eq!(action.as_deref(), None); // defaults to "ls"
        assert!(target.is_none());
        assert!(value.is_none());

        let cli = Cli::try_parse_from(["bpm", "dist-tag", "ls", "lodash"]).unwrap();
        let Commands::DistTag {
            action,
            target,
            value,
            ..
        } = cli.command
        else {
            panic!("expected dist-tag command");
        };
        assert_eq!(action.as_deref(), Some("ls"));
        assert_eq!(target.as_deref(), Some("lodash"));
        assert!(value.is_none());

        let cli =
            Cli::try_parse_from(["bpm", "dist-tag", "add", "@scope/pkg@1.2.3", "next"]).unwrap();
        let Commands::DistTag {
            action,
            target,
            value,
            ..
        } = cli.command
        else {
            panic!("expected dist-tag command");
        };
        assert_eq!(action.as_deref(), Some("add"));
        assert_eq!(target.as_deref(), Some("@scope/pkg@1.2.3"));
        assert_eq!(value.as_deref(), Some("next"));

        let cli = Cli::try_parse_from(["bpm", "dist-tag", "rm", "lodash", "old"]).unwrap();
        let Commands::DistTag {
            action,
            target,
            value,
            ..
        } = cli.command
        else {
            panic!("expected dist-tag command");
        };
        assert_eq!(action.as_deref(), Some("rm"));
        assert_eq!(target.as_deref(), Some("lodash"));
        assert_eq!(value.as_deref(), Some("old"));
    }

    #[test]
    fn owner_parses_actions_and_positionals() {
        // Bare `bpm owner` defaults to ls (action = None).
        let cli = Cli::try_parse_from(["bpm", "owner"]).unwrap();
        let Commands::Owner {
            action,
            target,
            value,
            ..
        } = cli.command
        else {
            panic!("expected owner command");
        };
        assert_eq!(action.as_deref(), None);
        assert!(target.is_none());
        assert!(value.is_none());

        // `owner ls <pkg>` → action=ls, target=pkg.
        let cli = Cli::try_parse_from(["bpm", "owner", "ls", "lodash"]).unwrap();
        let Commands::Owner {
            action,
            target,
            value,
            ..
        } = cli.command
        else {
            panic!("expected owner command");
        };
        assert_eq!(action.as_deref(), Some("ls"));
        assert_eq!(target.as_deref(), Some("lodash"));
        assert!(value.is_none());

        // `owner add <user> [pkg]` → action=add, target=user, value=pkg.
        let cli = Cli::try_parse_from(["bpm", "owner", "add", "alice", "@scope/pkg"]).unwrap();
        let Commands::Owner {
            action,
            target,
            value,
            ..
        } = cli.command
        else {
            panic!("expected owner command");
        };
        assert_eq!(action.as_deref(), Some("add"));
        assert_eq!(target.as_deref(), Some("alice"));
        assert_eq!(value.as_deref(), Some("@scope/pkg"));

        // `owner rm <user> <pkg>`.
        let cli = Cli::try_parse_from(["bpm", "owner", "rm", "bob", "mypkg"]).unwrap();
        let Commands::Owner {
            action,
            target,
            value,
            ..
        } = cli.command
        else {
            panic!("expected owner command");
        };
        assert_eq!(action.as_deref(), Some("rm"));
        assert_eq!(target.as_deref(), Some("bob"));
        assert_eq!(value.as_deref(), Some("mypkg"));
    }

    #[test]
    fn cache_defaults_to_ls_and_accepts_subcommands() {
        // Bare `bpm cache` defaults to ls (action = None).
        let cli = Cli::try_parse_from(["bpm", "cache"]).unwrap();
        let Commands::Cache { action, store } = cli.command else {
            panic!("expected cache command");
        };
        assert!(action.is_none());
        assert!(store.is_none());

        // `bpm cache verify --store X`.
        let cli = Cli::try_parse_from(["bpm", "cache", "verify", "--store", "/tmp/s"]).unwrap();
        let Commands::Cache { action, store } = cli.command else {
            panic!("expected cache command");
        };
        assert_eq!(action.as_deref(), Some("verify"));
        assert_eq!(store.as_deref(), Some(std::path::Path::new("/tmp/s")));

        // `bpm cache clean`.
        let cli = Cli::try_parse_from(["bpm", "cache", "clean"]).unwrap();
        let Commands::Cache { action, .. } = cli.command else {
            panic!("expected cache command");
        };
        assert_eq!(action.as_deref(), Some("clean"));
    }
}
