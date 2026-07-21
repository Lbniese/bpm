//! Lifecycle script execution (IMPLEMENTATION §14, §17 — Milestone 5).
//!
//! Runs permitted npm lifecycle scripts (`preinstall`, `install`, `postinstall`)
//! for installed packages. With a graph volume (the default install path),
//! scripts run **in place** against the package's directory inside the volume:
//! dependencies resolve through the volume's complete `node_modules` tree (npm
//! semantics), and files a script writes persist in the volume as derived
//! content. The package's own files are first isolated from the immutable store
//! (copied to independent inodes, nested deps preserved) so mutations can never
//! reach a store image. Without a volume, scripts run in a disposable temp
//! sandbox. `--ignore-scripts` skips the whole phase.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;

use crate::derived::{
    self, DerivedInputs, EnsureDerived, EnsureOptions, NullDerivedMetadata, RuntimeIdentity,
    SandboxFailure, TargetDescriptor,
};
use crate::graph::package_closure_digest;
use crate::integrity::{ArtifactId, Integrity};
use crate::lockfile::{LockSource, Lockfile, PackageEntry};
use crate::manifest::PackageManifest;
use crate::metrics::Metrics;
use crate::registry::RegistryClient;
use crate::store::ArtifactStore;

/// The lifecycle scripts bpm runs, in order, for each package.
pub const LIFECYCLE_PHASES: &[&str] = &["preinstall", "install", "postinstall"];
/// Lifecycle phases npm runs while preparing a Git package in its build clone.
const PREPARE_PHASES: &[&str] = &[
    "preinstall",
    "install",
    "postinstall",
    "preprepare",
    "prepare",
    "postprepare",
];

/// Derived-key runner version. Bumped when the derived build callback's
/// execution contract changes (isolation, snapshot, or attach semantics).
const DERIVED_RUNNER_VERSION: u32 = 1;
/// Derived-key policy version. Bumped when the set of inputs folded into the
/// key changes meaningfully (e.g. environment bounding lands in a later phase).
const DERIVED_POLICY_VERSION: u32 = 1;

/// Environment variables that can plausibly change a native lifecycle build's
/// output, and so must distinguish two derived keys even when the source and
/// dependency graph are identical.
///
/// This is a conservative allowlist, not the complete process environment:
/// folding the whole env in would make the cache almost never hit (PATH, HOME,
/// USER, hostname, and other host-specific noise differ across machines and
/// invocations) and would defeat cross-machine reuse. Anything not listed is
/// assumed not to affect build output. The full `env_clear` + complete
/// bounded-environment execution contract is a later refinement; until then
/// the script still inherits the parent env at run time, and this snapshot is
/// the subset that influences the derived key.
const ENV_INPUT_VARS: &[&str] = &[
    // C/C++ toolchain selection and flags.
    "CC",
    "CXX",
    "CFLAGS",
    "CXXFLAGS",
    "CPPFLAGS",
    "LDFLAGS",
    "AR",
    "NM",
    "RANLIB",
    // Build driver.
    "MAKE",
    "MAKEFLAGS",
    // node-gyp / prebuild / node-addon build configuration.
    "npm_config_target",
    "npm_config_arch",
    "npm_config_target_arch",
    "npm_config_runtime",
    "npm_config_disturl",
    "npm_config_python",
    "npm_config_build_from_source",
    "NODE_GYP_FORCE_PYTHON",
    "PYTHON",
    // Cross-compilation / sysroot discovery.
    "TARGET",
    "HOST",
    "PKG_CONFIG_PATH",
    "PKG_CONFIG_SYSROOT_DIR",
];

#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error("io error during lifecycle at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("required lifecycle script '{phase}' for {package} failed with exit code {exit_code}")]
    RequiredScriptFailure {
        package: String,
        phase: String,
        exit_code: i32,
    },
    #[error("Git prepare failed: {0}")]
    Prepare(String),
}

/// What happened when one phase of one package's lifecycle ran.
#[derive(Debug, Clone, Serialize)]
pub struct LifecycleOutcome {
    pub package: String,
    pub phase: String,
    pub command: String,
    pub ran: bool,
    pub exit_code: Option<i32>,
}

/// Aggregate result of a lifecycle pass.
#[derive(Debug, Default, Clone, Serialize)]
pub struct LifecycleStats {
    pub packages_with_scripts: usize,
    pub phases_executed: usize,
    pub phases_succeeded: usize,
    pub phases_failed: usize,
    pub skipped: bool,
    /// Package paths whose scripts ran (or would run) against the graph volume,
    /// producing derived content there. Recorded in the install plan so later
    /// `validate_plan` accepts their (non-hardlink) volume entries.
    #[serde(default)]
    pub derived_paths: Vec<String>,
    pub outcomes: Vec<LifecycleOutcome>,
}

/// Policy for running lifecycle scripts.
#[derive(Debug, Clone, Copy, Default)]
pub struct LifecyclePolicy {
    /// `true` = `--ignore-scripts`; the whole phase is a no-op.
    pub ignore_scripts: bool,
    /// `true` = a cached graph volume is being reused, so its derived lifecycle
    /// output is already persisted. Scan each fetchable package's image manifest
    /// to record which volume entries are derived copies (`derived_paths`, so
    /// the install plan validates), but do **not** execute any scripts. The
    /// warm-path optimization described by the M7 closeout: a reused volume
    /// must not re-run `preinstall`/`install`/`postinstall`.
    pub skip_execution: bool,
    /// `true` = consult the per-package derived-artifact store for each
    /// lifecycle-bearing package before executing its scripts. On a hit, the
    /// cached post-lifecycle image is attached into the volume and the scripts
    /// are skipped; on a miss they run in place and the result is published so
    /// a *different* graph that shares this package's dependency closure can
    /// reuse it. Off by default; enabled by `--derived-store` /
    /// `BPM_DERIVED_STORE=1`. Only effective when a graph volume is in use.
    pub use_derived_store: bool,
}

/// Run permitted lifecycle scripts for every fetchable package.
///
/// When a graph volume is supplied (`volume_path = Some`), each package's
/// scripts run **in place** against its directory inside the volume: its
/// dependencies resolve through the volume's complete `node_modules` tree
/// (npm semantics — `require('my-dep')` works because the dep is a sibling in
/// the volume), and any files a script writes persist in the graph-keyed volume
/// as derived content shared by every project with that graph. The package's
/// own files are first isolated from the immutable store (copied to independent
/// inodes, nested deps preserved) so postinstall mutations can never reach a
/// store image.
///
/// With no volume, scripts run in a disposable temp sandbox: no dependency
/// resolution, and mutations are discarded. This keeps workspace/compatible
/// installs safe (they symlink into the store) at the cost of correctness for
/// scripts that need their deps — the volume path is the supported one.
pub fn run_lifecycle(
    project_root: &Path,
    store: &ArtifactStore,
    lockfile: &Lockfile,
    artifact_ids: &[Option<ArtifactId>],
    volume_path: Option<&Path>,
    policy: LifecyclePolicy,
    metrics: &mut Metrics,
) -> Result<LifecycleStats, LifecycleError> {
    let mut stats = LifecycleStats {
        skipped: policy.ignore_scripts || policy.skip_execution,
        ..Default::default()
    };
    if policy.ignore_scripts {
        metrics.record("lifecycle", std::time::Duration::ZERO);
        return Ok(stats);
    }
    if policy.skip_execution {
        // Cached graph volume: its derived lifecycle output is already on disk.
        // Record which package paths hold derived (isolated, non-hardlink)
        // copies so the install plan's `lifecycle_paths` stays accurate and
        // `validate_plan` accepts them on the next install — but never execute
        // a script, since re-running them would only reproduce output that is
        // already present. The predicate below mirrors the execution branch
        // exactly (fetchable package + lifecycle script in its image manifest
        // + a materialized volume entry), so the recorded set matches what the
        // volume-building install produced — including skipping
        // platform-skipped packages that have no entry in the volume.
        for (i, pkg) in lockfile.packages.iter().enumerate() {
            if pkg.link || pkg.resolved.is_empty() {
                continue;
            }
            let Some(Some(id)) = artifact_ids.get(i).copied() else {
                continue;
            };
            let Some(vol) = volume_path else {
                continue;
            };
            if !vol.join(&pkg.path).is_dir() {
                // Not materialized (e.g. platform-skipped); nothing was derived.
                continue;
            }
            let image = store.image_path(&id);
            let scripts = match read_scripts(&image.join("package.json")) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if LIFECYCLE_PHASES.iter().any(|p| scripts.contains_key(*p)) {
                stats.packages_with_scripts += 1;
                stats.derived_paths.push(pkg.path.clone());
            }
        }
        metrics.record("lifecycle", std::time::Duration::ZERO);
        metrics.record("lifecycle_skipped_cached_volume", std::time::Duration::ZERO);
        return Ok(stats);
    }

    let mut outcomes: Vec<LifecycleOutcome> = Vec::new();

    // The derived store is consulted per lifecycle-bearing package. It needs a
    // runtime identity (probed from `node` once for the whole pass) and is only
    // meaningful against a graph volume; without one, scripts run in a sandbox
    // with no dependency resolution, so caching their output is unsound.
    let bounded_env = bounded_environment();
    let owned_runtime = if policy.use_derived_store && volume_path.is_some() {
        probe_runtime()
    } else {
        None
    };
    let null_metadata = NullDerivedMetadata;
    let derived_store = if owned_runtime.is_some() {
        derived::DerivedStore::open(store.root(), &null_metadata).ok()
    } else {
        None
    };

    for (i, pkg) in lockfile.packages.iter().enumerate() {
        if pkg.link || pkg.resolved.is_empty() {
            continue;
        }
        let Some(Some(id)) = artifact_ids.get(i).copied() else {
            continue;
        };
        // Read the package's own scripts from its (immutable) image manifest.
        let image = store.image_path(&id);
        let scripts = match read_scripts(&image.join("package.json")) {
            Ok(s) => s,
            Err(_) => continue, // unreadable manifest => no scripts to run
        };
        let has_lifecycle = LIFECYCLE_PHASES.iter().any(|p| scripts.contains_key(*p));
        if !has_lifecycle {
            continue;
        }
        stats.packages_with_scripts += 1;

        // Derived-artifact fast path (opt-in). On a cache hit the package's
        // post-lifecycle image is attached into the volume and its scripts are
        // skipped; on a miss they run in place and the result is published so
        // a different graph that shares this package's dependency closure can
        // reuse it. Any store/sandbox failure degrades gracefully to the
        // ordinary in-place execution below.
        if let (Some(vol), Some(runtime), Some(derived_store)) =
            (volume_path, owned_runtime.as_ref(), derived_store.as_ref())
        {
            let pkg_dir = vol.join(&pkg.path);
            if pkg_dir.is_dir() {
                let phase_count = LIFECYCLE_PHASES
                    .iter()
                    .filter(|phase| scripts.contains_key(**phase))
                    .count();
                let closure = package_closure_digest(lockfile, &pkg.path);
                let inputs = DerivedInputs {
                    source_artifact: id.as_bytes(),
                    source_revision: None,
                    dependency_graph: &closure,
                    target: current_target(),
                    runtime: RuntimeIdentity {
                        // Verified identity: the BLAKE3 digest of the
                        // canonicalized `node` binary, so a runtime change
                        // invalidates the key even before the reported
                        // version/ABI is consulted.
                        executable: &runtime.executable,
                        version: &runtime.version,
                        modules_abi: &runtime.modules_abi,
                        napi_version: runtime.napi_version.as_deref(),
                    },
                    phases: LIFECYCLE_PHASES,
                    scripts: &scripts,
                    environment: &bounded_env,
                    runner_version: DERIVED_RUNNER_VERSION,
                    policy_version: DERIVED_POLICY_VERSION,
                };
                let mut local_outcomes: Vec<LifecycleOutcome> = Vec::new();
                let built_entry = pkg_dir.clone();
                let image_root = image.clone();
                let project_root_owned = project_root.to_path_buf();
                let pkg_clone = pkg.clone();
                let result = derived_store.ensure(
                    &inputs,
                    &image,
                    EnsureOptions::default(),
                    |staging_image| {
                        // Run the scripts in place against the volume entry
                        // (current npm-compatible behavior: dependencies resolve
                        // through the complete volume node_modules tree), then
                        // snapshot the package's post-lifecycle own-tree into
                        // the staging image, stripped of `node_modules` so the
                        // published image is dependency-free.
                        if same_file(
                            &built_entry.join("package.json"),
                            &image_root.join("package.json"),
                        ) {
                            isolate_package(&image_root, &built_entry)
                                .map_err(|error| sandbox_io_failure(&pkg_clone, &error))?
                        }
                        for &phase in LIFECYCLE_PHASES {
                            let Some(cmd) = scripts.get(phase) else {
                                continue;
                            };
                            let status = run_script(
                                &built_entry,
                                phase,
                                cmd,
                                &project_root_owned,
                                store,
                                &pkg_clone,
                            );
                            let code = status
                                .map(|status| status.code().unwrap_or(-1))
                                .unwrap_or(-1);
                            local_outcomes.push(LifecycleOutcome {
                                package: pkg_clone.name.clone(),
                                phase: phase.to_string(),
                                command: cmd.clone(),
                                ran: true,
                                exit_code: Some(code),
                            });
                            if code != 0 {
                                return Err(SandboxFailure::new(
                                    pkg_clone.name.as_str(),
                                    phase,
                                    Some(code),
                                    None,
                                    &[],
                                    &[],
                                ));
                            }
                        }
                        snapshot_pkg_tree(&built_entry, staging_image)
                            .map_err(|error| sandbox_io_failure(&pkg_clone, &error))?;
                        Ok(())
                    },
                );
                match result {
                    Ok(EnsureDerived::Hit(reference)) => {
                        attach_derived(&reference.image_path, &pkg_dir)?;
                        metrics.record("derived_store_hit", Duration::ZERO);
                        stats.derived_paths.push(pkg.path.clone());
                        continue;
                    }
                    Ok(EnsureDerived::Built(_reference)) => {
                        // Scripts ran in place against `pkg_dir`; it already
                        // holds the package's derived tree (nested deps under
                        // its node_modules are untouched), so no attach needed.
                        metrics.record("derived_store_built", Duration::ZERO);
                        stats.phases_executed += phase_count;
                        stats.phases_succeeded += phase_count;
                        outcomes.append(&mut local_outcomes);
                        stats.derived_paths.push(pkg.path.clone());
                        continue;
                    }
                    Ok(EnsureDerived::Skipped) => {
                        // `ignore_scripts` is handled at the top of the function;
                        // reaching here is unexpected, so fall through.
                    }
                    Err(error) => match error {
                        derived::DerivedError::Sandbox { failure } => {
                            // A sandbox failure is a required lifecycle failure.
                            return Err(LifecycleError::RequiredScriptFailure {
                                package: failure.package,
                                phase: failure.phase,
                                exit_code: failure.exit_code.unwrap_or(-1),
                            });
                        }
                        other => {
                            // Store/IO error: degrade to ordinary in-place
                            // execution for this package.
                            metrics.record("derived_store_miss", Duration::ZERO);
                            eprintln!(
                                "warning: derived store unavailable for {}, running scripts in place: {other}",
                                pkg.path
                            );
                        }
                    },
                }
            }
        }

        // Choose the execution root. `sandbox` (when set) must outlive the
        // phase loop below so the temp dir is not reaped mid-run.
        let sandbox: Option<tempfile::TempDir>;
        let cwd: PathBuf;
        if let Some(vol) = volume_path {
            let pkg_dir = vol.join(&pkg.path);
            if !pkg_dir.is_dir() {
                // Not materialized (e.g. platform-skipped); nothing to run against.
                continue;
            }
            // Idempotency: a pristine volume entry still shares its package.json
            // inode with the store image and must be isolated before running; an
            // already-derived entry (a prior run's copy) does not, and is left
            // intact so re-installs do not reset prior derived content.
            if same_file(&pkg_dir.join("package.json"), &image.join("package.json")) {
                isolate_package(&image, &pkg_dir)?;
            }
            stats.derived_paths.push(pkg.path.clone());
            cwd = pkg_dir;
            sandbox = None;
        } else {
            // Disposable sandbox: never touches the store, but deps do not
            // resolve (no node_modules). The supported path is the volume one.
            let td = tempfile::tempdir().map_err(|source| LifecycleError::Io {
                path: "<temp>".into(),
                source,
            })?;
            cwd = td.path().to_path_buf();
            copy_tree(&image, &cwd)?;
            sandbox = Some(td);
        }

        for &phase in LIFECYCLE_PHASES {
            let Some(cmd) = scripts.get(phase) else {
                continue;
            };
            let status = metrics.measure("lifecycle", || {
                run_script(&cwd, phase, cmd, project_root, store, pkg)
            });
            let code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
            let outcome = LifecycleOutcome {
                package: pkg.name.clone(),
                phase: phase.to_string(),
                command: cmd.clone(),
                ran: true,
                exit_code: Some(code),
            };
            outcomes.push(outcome);
            stats.phases_executed += 1;
            if code == 0 {
                stats.phases_succeeded += 1;
            } else {
                stats.phases_failed += 1;
                // Return immediately on first required failure.
                // The sandbox is dropped via the return, cleaning temporary files.
                return Err(LifecycleError::RequiredScriptFailure {
                    package: pkg.name.clone(),
                    phase: phase.to_string(),
                    exit_code: code,
                });
            }
        }
        // Hold the sandbox (if any) until every phase has run.
        drop(sandbox);
    }

    stats.outcomes = outcomes;
    Ok(stats)
}

/// One immutable package image produced by Git's build-context lifecycle.
#[derive(Debug, Clone)]
pub struct PreparedImage {
    pub image_path: PathBuf,
    pub key: derived::DerivedKey,
}

/// Build immutable images for Git packages that declare a `prepare` script.
///
/// Preparation is intentionally separate from the consumer graph. A transient
/// closure containing dev dependencies is materialized only in a temporary
/// build root, and the published image contains the package's own files with
/// `node_modules` stripped. Final installation can therefore use the image
/// without exposing preparation-only dependencies.
pub fn prepare_git_packages(
    _project_root: &Path,
    store: &ArtifactStore,
    lockfile: &Lockfile,
    artifact_ids: &[Option<ArtifactId>],
    registry: &RegistryClient,
    metrics: &mut Metrics,
) -> Result<BTreeMap<String, PreparedImage>, LifecycleError> {
    let runtime = probe_runtime()
        .ok_or_else(|| LifecycleError::Prepare("could not probe the node runtime".into()))?;
    let environment = bounded_environment();
    let metadata = NullDerivedMetadata;
    let derived_store = derived::DerivedStore::open(store.root(), &metadata)
        .map_err(|error| LifecycleError::Prepare(error.to_string()))?;
    let mut prepared = BTreeMap::new();

    for (index, package) in lockfile.packages.iter().enumerate() {
        let Some(Some(artifact_id)) = artifact_ids.get(index) else {
            continue;
        };
        let Some(LockSource::Git {
            resolved_commit, ..
        }) = lockfile
            .resolution
            .packages
            .get(&package.path)
            .map(|resolution| &resolution.source)
        else {
            continue;
        };
        let image = store.image_path(artifact_id);
        let scripts =
            read_scripts(&image.join("package.json")).map_err(|source| LifecycleError::Io {
                path: image.join("package.json").display().to_string(),
                source,
            })?;
        if !scripts.contains_key("prepare") {
            continue;
        }
        let manifest = PackageManifest::from_path(&image.join("package.json"))
            .map_err(|error| LifecycleError::Prepare(error.to_string()))?;
        let closure = crate::resolver::build_prepare_closure(
            &manifest,
            registry,
            "bpm-git-prepare",
            crate::resolver::current_target_platform(),
        )
        .map_err(|error| LifecycleError::Prepare(error.to_string()))?;
        let closure_digest = *closure.digest();
        let inputs = DerivedInputs {
            source_artifact: artifact_id.as_bytes(),
            source_revision: Some(resolved_commit.as_str()),
            dependency_graph: &closure_digest,
            target: current_target(),
            runtime: RuntimeIdentity {
                executable: &runtime.executable,
                version: &runtime.version,
                modules_abi: &runtime.modules_abi,
                napi_version: runtime.napi_version.as_deref(),
            },
            phases: PREPARE_PHASES,
            scripts: &scripts,
            environment: &environment,
            runner_version: DERIVED_RUNNER_VERSION,
            policy_version: DERIVED_POLICY_VERSION + 1,
        };
        let source_image = image.clone();
        let package_for_script = package.clone();
        let closure_lock = closure.lockfile;
        let result = derived_store
            .ensure(
                &inputs,
                &source_image,
                EnsureOptions::default(),
                |staging_image| {
                    let build_dir = tempfile::tempdir().map_err(|error| {
                        SandboxFailure::new(
                            package_for_script.name.as_str(),
                            "prepare",
                            None,
                            None,
                            &[],
                            error.to_string().as_bytes(),
                        )
                    })?;
                    let build_root = build_dir.path().join("package");
                    copy_tree(&source_image, &build_root)
                        .map_err(|error| sandbox_io_failure(&package_for_script, &error))?;
                    materialize_prepare_closure(
                        &build_root,
                        &closure_lock,
                        store,
                        registry,
                        metrics,
                    )
                    .map_err(|error| sandbox_io_failure(&package_for_script, &error))?;
                    for &phase in PREPARE_PHASES {
                        let Some(command) = scripts.get(phase) else {
                            continue;
                        };
                        let status = run_script(
                            &build_root,
                            phase,
                            command,
                            &build_root,
                            store,
                            &package_for_script,
                        )
                        .map_err(|error| {
                            SandboxFailure::new(
                                package_for_script.name.as_str(),
                                phase,
                                None,
                                None,
                                &[],
                                error.to_string().as_bytes(),
                            )
                        })?;
                        let code = status.code().unwrap_or(-1);
                        if code != 0 {
                            return Err(SandboxFailure::new(
                                package_for_script.name.as_str(),
                                phase,
                                Some(code),
                                None,
                                &[],
                                &[],
                            ));
                        }
                    }
                    snapshot_pkg_tree(&build_root, staging_image)
                        .map_err(|error| sandbox_io_failure(&package_for_script, &error))
                },
            )
            .map_err(|error| LifecycleError::Prepare(error.to_string()))?;
        let reference = match result {
            EnsureDerived::Hit(reference) | EnsureDerived::Built(reference) => reference,
            EnsureDerived::Skipped => continue,
        };
        metrics.record("git_prepare_image", Duration::ZERO);
        prepared.insert(
            package.path.clone(),
            PreparedImage {
                image_path: reference.image_path,
                key: reference.key,
            },
        );
    }
    Ok(prepared)
}

/// Fetch and materialize the transient preparation closure into `build_root`.
fn materialize_prepare_closure(
    build_root: &Path,
    closure: &Lockfile,
    store: &ArtifactStore,
    registry: &RegistryClient,
    metrics: &mut Metrics,
) -> Result<(), LifecycleError> {
    let mut artifact_ids = Vec::with_capacity(closure.packages.len());
    for package in &closure.packages {
        if package.link {
            artifact_ids.push(None);
            continue;
        }
        let integrity = package
            .integrity
            .as_deref()
            .map(Integrity::parse)
            .transpose()
            .map_err(|error| LifecycleError::Prepare(error.to_string()))?;
        let artifact = store
            .ensure_artifact_with_client(
                registry.http(),
                &package.resolved,
                integrity.as_ref(),
                metrics,
            )
            .map_err(|error| LifecycleError::Prepare(error.to_string()))?;
        store
            .ensure_image(&artifact.id, metrics)
            .map_err(|error| LifecycleError::Prepare(error.to_string()))?;
        artifact_ids.push(Some(artifact.id));
    }
    crate::materializer::materialize_lockfile_with_backend(
        build_root,
        store,
        closure,
        &artifact_ids,
        crate::materializer::MaterializeMode::Compatible,
        crate::materializer::MaterializeBackend::Hardlink,
    )
    .map_err(|error| LifecycleError::Prepare(error.to_string()))?;
    Ok(())
}

/// Read the `scripts` map from a `package.json` at `manifest_path`.
fn read_scripts(manifest_path: &Path) -> Result<BTreeMap<String, String>, std::io::Error> {
    let bytes = fs::read(manifest_path)?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    let Some(obj) = v.as_object() else {
        return Ok(BTreeMap::new());
    };
    let Some(scripts) = obj.get("scripts").and_then(|s| s.as_object()) else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for (k, vv) in scripts {
        if let Some(s) = vv.as_str() {
            out.insert(k.clone(), s.to_string());
        }
    }
    Ok(out)
}

/// Execute one script via `sh -c` with an npm-compatible environment.
fn run_script(
    cwd: &Path,
    phase: &str,
    command: &str,
    project_root: &Path,
    _store: &ArtifactStore,
    pkg: &PackageEntry,
) -> std::io::Result<std::process::ExitStatus> {
    let mut cmd = crate::platform::script_command(command);
    cmd.current_dir(cwd);
    // npm-compatible environment (IMPLEMENTATION §14).
    cmd.env("npm_lifecycle_event", phase);
    cmd.env("npm_lifecycle_script", command);
    cmd.env("npm_package_name", &pkg.name);
    cmd.env("npm_package_version", &pkg.version);
    cmd.env(
        "npm_config_user_agent",
        concat!("bpm/", env!("CARGO_PKG_VERSION")),
    );
    cmd.env("npm_execpath", "bpm");
    cmd.env("INIT_CWD", project_root);
    let node = crate::platform::find_executable(
        std::ffi::OsStr::new("node"),
        std::env::var_os("PATH").as_deref(),
    )
    .unwrap_or_else(|| PathBuf::from("node"));
    cmd.env("NODE", node);
    // Project node_modules/.bin should be reachable for scripts; prepend it.
    if let Some(path) = std::env::var_os("PATH") {
        let bin = project_root.join("node_modules").join(".bin");
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&path));
        let new_path = std::env::join_paths(paths).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("could not construct lifecycle PATH: {error}"),
            )
        })?;
        cmd.env("PATH", new_path);
    }
    cmd.status()
}

/// Recursively copy a directory tree (files + symlinks), cheap temp-sandbox
/// materialization for script execution.
fn copy_tree(src: &Path, dst: &Path) -> Result<(), LifecycleError> {
    copy_tree_inner(src, dst)
}

fn copy_tree_inner(src: &Path, dst: &Path) -> Result<(), LifecycleError> {
    fs::create_dir_all(dst).map_err(|source| LifecycleError::Io {
        path: dst.display().to_string(),
        source,
    })?;
    for entry in fs::read_dir(src).map_err(|source| LifecycleError::Io {
        path: src.display().to_string(),
        source,
    })? {
        let entry = entry.map_err(|source| LifecycleError::Io {
            path: src.display().to_string(),
            source,
        })?;
        let kind = entry.file_type().map_err(|source| LifecycleError::Io {
            path: entry.path().display().to_string(),
            source,
        })?;
        let s = entry.path();
        let d = dst.join(entry.file_name());
        if kind.is_dir() {
            copy_tree_inner(&s, &d)?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            {
                let target = fs::read_link(&s).map_err(|source| LifecycleError::Io {
                    path: s.display().to_string(),
                    source,
                })?;
                std::os::unix::fs::symlink(&target, &d).map_err(|source| LifecycleError::Io {
                    path: d.display().to_string(),
                    source,
                })?;
            }
            #[cfg(not(unix))]
            {
                // On non-unix, fall back to copying the resolved file.
                let _ = fs::copy(&s, &d).map_err(|source| LifecycleError::Io {
                    path: d.display().to_string(),
                    source,
                })?;
            }
        } else {
            fs::copy(&s, &d).map_err(|source| LifecycleError::Io {
                path: d.display().to_string(),
                source,
            })?;
        }
    }
    Ok(())
}

/// Turn a hardlinked volume package directory into a writable, store-independent
/// copy of its pristine image, preserving any nested `node_modules` (placed by
/// the materializer) so the package's own dependencies keep resolving after
/// isolation.
///
/// Each package file is unlinked then re-copied: this breaks the hardlink it
/// shared with the immutable store image so postinstall mutations stay local
/// to the volume. Because the pristine image has no `node_modules`, the nested
/// dependency directories already present in `pkg_dir` are never traversed and
/// are left untouched.
fn isolate_package(store_image: &Path, pkg_dir: &Path) -> Result<(), LifecycleError> {
    isolate_copy_tree(store_image, pkg_dir)
}

fn isolate_copy_tree(src: &Path, dst: &Path) -> Result<(), LifecycleError> {
    fs::create_dir_all(dst).map_err(|source| LifecycleError::Io {
        path: dst.display().to_string(),
        source,
    })?;
    for entry in fs::read_dir(src).map_err(|source| LifecycleError::Io {
        path: src.display().to_string(),
        source,
    })? {
        let entry = entry.map_err(|source| LifecycleError::Io {
            path: src.display().to_string(),
            source,
        })?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let kind = entry.file_type().map_err(|source| LifecycleError::Io {
            path: from.display().to_string(),
            source,
        })?;
        if kind.is_dir() {
            isolate_copy_tree(&from, &to)?;
            continue;
        }
        // Remove the existing (hardlinked) entry first so the fresh copy is an
        // independent inode, never truncating the shared store image.
        if symlink_exists(&to) {
            remove_any(&to).map_err(|source| LifecycleError::Io {
                path: to.display().to_string(),
                source,
            })?;
        }
        if kind.is_symlink() {
            #[cfg(unix)]
            {
                let target = fs::read_link(&from).map_err(|source| LifecycleError::Io {
                    path: from.display().to_string(),
                    source,
                })?;
                std::os::unix::fs::symlink(&target, &to).map_err(|source| LifecycleError::Io {
                    path: to.display().to_string(),
                    source,
                })?;
            }
            #[cfg(not(unix))]
            {
                fs::copy(&from, &to).map_err(|source| LifecycleError::Io {
                    path: to.display().to_string(),
                    source,
                })?;
            }
        } else {
            fs::copy(&from, &to).map_err(|source| LifecycleError::Io {
                path: to.display().to_string(),
                source,
            })?;
        }
    }
    Ok(())
}

/// `true` when `a` and `b` are the same on-disk file (same device + inode on
/// Unix). Used to tell a pristine hardlinked volume entry apart from an
/// already-derived (isolated) copy.
fn same_file(a: &Path, b: &Path) -> bool {
    let (Ok(a), Ok(b)) = (fs::metadata(a), fs::metadata(b)) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(not(unix))]
    {
        a.len() == b.len()
    }
}

fn symlink_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Owned counterpart of [`RuntimeIdentity`] so a single `node` probe can feed
/// every package's derived key in one lifecycle pass.
struct OwnedRuntimeIdentity {
    /// BLAKE3 digest of the canonicalized `node` executable (32 bytes). This
    /// verified identity is folded into every derived key, so a native
    /// artifact built under one runtime is never substituted for one built
    /// under another even if both report the same version string.
    executable: Vec<u8>,
    version: String,
    modules_abi: String,
    napi_version: Option<String>,
}

/// Probe the `node` runtime once for the derived-key identity. Returns `None`
/// when `node` is unavailable or its reported identity is incomplete, in which
/// case the derived store is skipped for the whole pass and scripts run in
/// place as usual.
///
/// A *single* `node` invocation gathers `execPath`, `version`, the modules
/// ABI, and the N-API version (newline-separated); node startup (~50 ms)
/// dominates the probe, so collapsing what was five spawns into one keeps the
/// derived store cheap enough to leave on by default.
fn probe_runtime() -> Option<OwnedRuntimeIdentity> {
    let probe = node_output(&[
        "-p",
        "[process.execPath, process.version, String(process.versions.modules), String(process.versions.napi)].join('\\n')",
    ])?;
    let mut lines = probe.lines();
    let exec_path = lines.next()?;
    let version = lines.next()?.to_owned();
    let modules_abi = lines.next()?.to_owned();
    let napi_raw = lines.next()?;
    let napi_version = (napi_raw != "undefined").then(|| napi_raw.to_owned());
    let executable = node_executable_digest(exec_path)?;
    Some(OwnedRuntimeIdentity {
        executable,
        version,
        modules_abi,
        napi_version,
    })
}

/// BLAKE3 digest of the real `node` binary that will run the lifecycle
/// scripts. Canonicalizing `execPath` collapses symlink farms (nvm, homebrew,
/// fnm) onto the underlying binary -- two installs that ultimately exec the
/// same bytes hash identically, two distinct runtimes hash differently. The
/// digest is streamed so the binary is never fully resident. Any failure
/// yields `None`, and the caller skips the derived store for the whole pass
/// (graceful degradation to in-place execution).
fn node_executable_digest(exec_path: &str) -> Option<Vec<u8>> {
    let canonical = fs::canonicalize(exec_path).ok()?;
    if !canonical.is_file() {
        return None;
    }
    let file = fs::File::open(&canonical).ok()?;
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(file).ok()?;
    Some(hasher.finalize().as_bytes().to_vec())
}

/// Snapshot the bounded lifecycle environment for the derived key: the values
/// of [`ENV_INPUT_VARS`] that are actually set in the current process
/// environment. Only these are folded into the key (everything else is assumed
/// build-invariant), keeping the cache stable in the face of ambient noise like
/// PATH/HOME/USER/hostname that must not defeat reuse. Deterministic and
/// independent of the process environment's iteration order.
fn bounded_environment() -> BTreeMap<OsString, OsString> {
    ENV_INPUT_VARS
        .iter()
        .filter_map(|&name| std::env::var_os(name).map(|value| (OsString::from(name), value)))
        .collect()
}

fn node_output(args: &[&str]) -> Option<String> {
    let output = Command::new("node").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Target platform descriptor for the current process, folded into every
/// derived key so a native artifact built for one target never substitutes for
/// another.
fn current_target() -> TargetDescriptor<'static> {
    let abi = if cfg!(target_env = "gnu") {
        "gnu"
    } else if cfg!(target_env = "musl") {
        "musl"
    } else if cfg!(target_env = "msvc") {
        "msvc"
    } else {
        ""
    };
    TargetDescriptor {
        os: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
        family: std::env::consts::FAMILY,
        abi,
    }
}

/// Recursively mirror `src` into `dst`, replacing any existing `dst` entry at
/// each position. When `skip_top_node_modules` is set, a top-level
/// `node_modules` in `src` is skipped (the volume's dependency placement, never
/// part of the package's own image).
fn mirror_tree(src: &Path, dst: &Path, skip_top_node_modules: bool) -> Result<(), LifecycleError> {
    fs::create_dir_all(dst).map_err(|source| lc_io(dst, source))?;
    for entry in fs::read_dir(src).map_err(|source| lc_io(src, source))? {
        let entry = entry.map_err(|source| lc_io(src, source))?;
        let name = entry.file_name();
        if skip_top_node_modules && name == "node_modules" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let kind = entry.file_type().map_err(|source| lc_io(&from, source))?;
        if symlink_exists(&to) {
            remove_any(&to).map_err(|source| lc_io(&to, source))?;
        }
        if kind.is_dir() {
            mirror_tree(&from, &to, false)?;
        } else if kind.is_symlink() {
            copy_symlink(&from, &to)?;
        } else {
            fs::copy(&from, &to).map_err(|source| lc_io(&to, source))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(from: &Path, to: &Path) -> Result<(), LifecycleError> {
    let target = fs::read_link(from).map_err(|source| lc_io(from, source))?;
    std::os::unix::fs::symlink(&target, to).map_err(|source| lc_io(to, source))?;
    Ok(())
}

#[cfg(not(unix))]
fn copy_symlink(from: &Path, to: &Path) -> Result<(), LifecycleError> {
    fs::copy(from, to).map_err(|source| lc_io(to, source))?;
    Ok(())
}

/// Snapshot a package's post-lifecycle own-tree from the volume entry into the
/// derived store's staging image, excluding `node_modules`: the nested
/// dependencies there belong to the volume, not the package's published image,
/// so stripping them keeps the derived identity a pure function of the
/// package's own tree (the contract proven in `src/derived/store.rs`).
fn snapshot_pkg_tree(built_entry: &Path, staging_image: &Path) -> Result<(), LifecycleError> {
    clear_dir_contents(staging_image)?;
    mirror_tree(built_entry, staging_image, true)
}

/// Attach a cached derived image onto a pristine volume entry, overlaying the
/// package's post-lifecycle own-files while preserving nested dependencies the
/// materializer already placed under the entry's `node_modules`.
fn attach_derived(derived_image: &Path, vol_entry: &Path) -> Result<(), LifecycleError> {
    for entry in fs::read_dir(vol_entry).map_err(|source| lc_io(vol_entry, source))? {
        let entry = entry.map_err(|source| lc_io(vol_entry, source))?;
        if entry.file_name() == "node_modules" {
            continue;
        }
        remove_any(&entry.path()).map_err(|source| lc_io(&entry.path(), source))?;
    }
    mirror_tree(derived_image, vol_entry, false)
}

fn clear_dir_contents(dir: &Path) -> Result<(), LifecycleError> {
    for entry in fs::read_dir(dir).map_err(|source| lc_io(dir, source))? {
        let entry = entry.map_err(|source| lc_io(dir, source))?;
        remove_any(&entry.path()).map_err(|source| lc_io(&entry.path(), source))?;
    }
    Ok(())
}

fn sandbox_io_failure(pkg: &PackageEntry, error: &LifecycleError) -> derived::SandboxFailure {
    let message = error.to_string();
    derived::SandboxFailure::new(
        pkg.name.as_str(),
        "install",
        None,
        None,
        &[],
        message.as_bytes(),
    )
}

fn lc_io(path: &Path, source: std::io::Error) -> LifecycleError {
    LifecycleError::Io {
        path: path.display().to_string(),
        source,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Stage a pristine image and a hardlinked volume entry (with a nested dep),
    /// the layout the graph volume produces before lifecycle runs.
    #[cfg(unix)]
    fn stage_volume_entry(tmp: &Path) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::MetadataExt;
        let image = tmp.join("image");
        let vol = tmp.join("volume/node_modules/pkg");
        fs::create_dir_all(&image).unwrap();
        fs::write(image.join("package.json"), b"{\"name\":\"pkg\"}").unwrap();
        fs::write(image.join("index.js"), b"module.exports=1;").unwrap();
        fs::create_dir_all(&vol).unwrap();
        fs::hard_link(image.join("package.json"), vol.join("package.json")).unwrap();
        fs::hard_link(image.join("index.js"), vol.join("index.js")).unwrap();
        fs::create_dir_all(vol.join("node_modules/dep")).unwrap();
        fs::write(
            vol.join("node_modules/dep/package.json"),
            b"{\"name\":\"dep\"}",
        )
        .unwrap();
        // Sanity: the staged entry is genuinely hardlinked to the store image.
        assert_eq!(
            fs::metadata(vol.join("package.json")).unwrap().ino(),
            fs::metadata(image.join("package.json")).unwrap().ino(),
        );
        (image, vol)
    }

    #[cfg(unix)]
    #[test]
    fn isolate_package_breaks_hardlinks_but_keeps_nested_deps() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempdir().unwrap();
        let (image, vol) = stage_volume_entry(tmp.path());
        let store_ino = fs::metadata(image.join("package.json")).unwrap().ino();

        isolate_package(&image, &vol).unwrap();

        // Package files now have independent inodes (decoupled from the store).
        assert_ne!(
            fs::metadata(vol.join("package.json")).unwrap().ino(),
            store_ino,
            "package.json must be decoupled from the store image",
        );
        // Content is preserved.
        assert_eq!(
            fs::read(vol.join("package.json")).unwrap(),
            b"{\"name\":\"pkg\"}",
        );
        assert!(vol.join("index.js").is_file());
        // Nested dependency directories are preserved.
        assert!(vol.join("node_modules/dep/package.json").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn isolate_package_protects_the_store_from_mutation() {
        let tmp = tempdir().unwrap();
        let (image, vol) = stage_volume_entry(tmp.path());

        isolate_package(&image, &vol).unwrap();

        // A postinstall-style mutation of the isolated entry must not reach the
        // immutable store image.
        fs::write(vol.join("package.json"), b"mutated").unwrap();
        assert_eq!(
            fs::read(image.join("package.json")).unwrap(),
            b"{\"name\":\"pkg\"}",
            "store image must be unchanged after isolating mutations",
        );
    }

    // --- derived-store attach / snapshot helpers ---

    /// Stage a volume entry shaped like a materialized package: pristine own
    /// files plus a nested dependency placed by the materializer.
    fn stage_volume_entry_with_deps(tmp: &Path) -> (PathBuf, PathBuf) {
        let image = tmp.join("image");
        let vol = tmp.join("volume/node_modules/pkg");
        fs::create_dir_all(&image).unwrap();
        fs::write(image.join("package.json"), b"pristine").unwrap();
        fs::write(image.join("index.js"), b"v1").unwrap();
        fs::create_dir_all(&vol).unwrap();
        fs::write(vol.join("package.json"), b"pristine").unwrap();
        fs::write(vol.join("index.js"), b"v1").unwrap();
        fs::create_dir_all(vol.join("node_modules/dep")).unwrap();
        fs::write(vol.join("node_modules/dep/package.json"), b"dep").unwrap();
        (image, vol)
    }

    #[test]
    fn snapshot_pkg_tree_strips_node_modules_and_copies_own_files() {
        let tmp = tempdir().unwrap();
        let (image, vol) = stage_volume_entry_with_deps(tmp.path());
        // Simulate a post-install mutation of the package's own files plus a
        // derived artifact written by the script.
        fs::write(vol.join("index.js"), b"compiled").unwrap();
        fs::write(vol.join("build.out"), b"derived").unwrap();
        let staging = tmp.path().join("staging");
        fs::create_dir_all(&staging).unwrap();
        // The staging image starts as a pristine copy (ensure's contract).
        fs::write(staging.join("package.json"), b"pristine").unwrap();
        fs::write(staging.join("index.js"), b"v1").unwrap();

        snapshot_pkg_tree(&vol, &staging).unwrap();

        // Own files are mirrored (post-lifecycle content wins).
        assert_eq!(fs::read(staging.join("package.json")).unwrap(), b"pristine");
        assert_eq!(fs::read(staging.join("index.js")).unwrap(), b"compiled");
        assert_eq!(fs::read(staging.join("build.out")).unwrap(), b"derived");
        // Nested dependencies are stripped from the published image.
        assert!(!staging.join("node_modules").exists());
        let _ = image;
    }

    #[test]
    fn attach_derived_overlays_image_and_preserves_nested_deps() {
        let tmp = tempdir().unwrap();
        let (_image, vol) = stage_volume_entry_with_deps(tmp.path());
        // A cached derived image: the package's post-lifecycle own tree, no
        // node_modules.
        let derived = tmp.path().join("derived/image");
        fs::create_dir_all(&derived).unwrap();
        fs::write(derived.join("package.json"), b"derived").unwrap();
        fs::write(derived.join("index.js"), b"compiled").unwrap();
        fs::write(derived.join("build.out"), b"derived").unwrap();

        attach_derived(&derived, &vol).unwrap();

        // Pristine own-files are replaced by the derived image's content.
        assert_eq!(fs::read(vol.join("package.json")).unwrap(), b"derived");
        assert_eq!(fs::read(vol.join("index.js")).unwrap(), b"compiled");
        assert!(vol.join("build.out").is_file());
        // Nested dependencies the materializer placed are preserved.
        assert_eq!(
            fs::read(vol.join("node_modules/dep/package.json")).unwrap(),
            b"dep",
        );
    }

    #[test]
    fn mirror_tree_skips_only_top_level_node_modules() {
        let tmp = tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(src.join("node_modules/dep")).unwrap();
        fs::write(src.join("node_modules/dep/package.json"), b"dep").unwrap();
        fs::create_dir_all(src.join("sub/node_modules/inner")).unwrap();
        fs::write(src.join("sub/node_modules/inner/package.json"), b"inner").unwrap();
        fs::write(src.join("own.js"), b"own").unwrap();

        mirror_tree(&src, &dst, true).unwrap();

        // Top-level node_modules is skipped.
        assert!(!dst.join("node_modules").exists());
        // Everything else (including nested node_modules deeper in the tree) is
        // mirrored, since only the package's own top-level placement is excluded.
        assert_eq!(fs::read(dst.join("own.js")).unwrap(), b"own");
        assert_eq!(
            fs::read(dst.join("sub/node_modules/inner/package.json")).unwrap(),
            b"inner",
        );
    }

    #[test]
    fn probe_runtime_yields_a_fixed_size_stable_node_digest() {
        // probe_runtime requires `node` on PATH; skip cleanly when absent
        // rather than failing on an environment that has none.
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("node not on PATH; skipping probe_runtime digest test");
            return;
        }
        let identity = probe_runtime().expect("node is present but probe_runtime returned None");
        // BLAKE3 produces a 32-byte digest; the field must never carry the old
        // fixed marker string.
        assert_eq!(
            identity.executable.len(),
            32,
            "executable identity must be a 32-byte BLAKE3 digest of the node binary",
        );
        // Canonicalization + hashing are deterministic; the same binary probed
        // twice must yield identical bytes (so the derived key is stable across
        // installs that share a runtime).
        let again = probe_runtime().expect("second probe_runtime returned None");
        assert_eq!(
            identity.executable, again.executable,
            "node executable digest must be stable across probes",
        );
    }

    #[test]
    fn bounded_environment_captures_only_allowlisted_vars() {
        let env = bounded_environment();
        // Deterministic across calls (a pure function of the current env).
        assert_eq!(env, bounded_environment());
        // Every captured key must be one of the allowlisted inputs -- no
        // ambient bleed-through.
        for name in env.keys() {
            let name = name.to_str().unwrap_or("");
            assert!(
                ENV_INPUT_VARS.contains(&name),
                "{name:?} captured by bounded_environment but is not in ENV_INPUT_VARS",
            );
        }
        // The allowlist must cover the vars most likely to steer a native build.
        for must in [
            "CC",
            "CXX",
            "CFLAGS",
            "npm_config_arch",
            "npm_config_target",
        ] {
            assert!(
                ENV_INPUT_VARS.contains(&must),
                "ENV_INPUT_VARS missing {must}"
            );
        }
        // Host-specific ambient noise must never be folded into a derived key,
        // or the cache would never hit and would not transfer across machines.
        for noise in ["PATH", "HOME", "USER", "PWD", "HOSTNAME", "SHELL", "TMPDIR"] {
            assert!(
                !ENV_INPUT_VARS.contains(&noise),
                "{noise} must stay out of the derived-key environment (host-specific noise)",
            );
        }
    }
}
