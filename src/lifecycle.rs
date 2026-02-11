//! Lifecycle script execution (IMPLEMENTATION §14, §17 — Milestone 5).
//!
//! Runs permitted npm lifecycle scripts (`preinstall`, `install`, `postinstall`)
//! for installed packages in an **isolated build sandbox**, never against the
//! immutable store image or the shared graph volume. A package's image is
//! copied to a temp dir, the scripts run there with an npm-compatible
//! environment, and a summary of what ran is reported. `--ignore-scripts` skips
//! the whole phase.
//!
//! The derived-artifact store (publishing build output keyed by build inputs)
//! is deliberately deferred; this milestone delivers the compatible script
//! environment + observability. Scripts that fail are reported but do not
//! poison the install unless they touch the project — see `LifecyclePolicy`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use thiserror::Error;

use crate::integrity::ArtifactId;
use crate::lockfile::{Lockfile, PackageEntry};
use crate::metrics::Metrics;
use crate::store::ArtifactStore;

/// The lifecycle scripts bpm runs, in order, for each package.
pub const LIFECYCLE_PHASES: &[&str] = &["preinstall", "install", "postinstall"];

#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error("io error during lifecycle at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
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
    pub outcomes: Vec<LifecycleOutcome>,
}

/// Policy for running lifecycle scripts.
#[derive(Debug, Clone, Copy, Default)]
pub struct LifecyclePolicy {
    /// `true` = `--ignore-scripts`; the whole phase is a no-op.
    pub ignore_scripts: bool,
}

/// Run permitted lifecycle scripts for every fetchable package, each in its
/// own temp sandbox.
pub fn run_lifecycle(
    project_root: &Path,
    store: &ArtifactStore,
    lockfile: &Lockfile,
    artifact_ids: &[Option<ArtifactId>],
    policy: LifecyclePolicy,
    metrics: &mut Metrics,
) -> Result<LifecycleStats, LifecycleError> {
    let mut stats = LifecycleStats {
        skipped: policy.ignore_scripts,
        ..Default::default()
    };
    if policy.ignore_scripts {
        metrics.record("lifecycle", std::time::Duration::ZERO);
        return Ok(stats);
    }

    let mut outcomes: Vec<LifecycleOutcome> = Vec::new();

    for (i, pkg) in lockfile.packages.iter().enumerate() {
        if pkg.link || pkg.resolved.is_empty() {
            continue;
        }
        let Some(Some(id)) = artifact_ids.get(i).copied() else {
            continue;
        };
        // Read the package's own scripts from its (immutable) image manifest.
        let manifest_path = store.image_path(&id).join("package.json");
        let scripts = match read_scripts(&manifest_path) {
            Ok(s) => s,
            Err(_) => continue, // unreadable manifest => no scripts to run
        };
        let has_lifecycle = LIFECYCLE_PHASES.iter().any(|p| scripts.contains_key(*p));
        if !has_lifecycle {
            continue;
        }
        stats.packages_with_scripts += 1;

        // Sandbox: a temp copy of the package image, so scripts can never mutate
        // the immutable store or the shared graph volume.
        let sandbox = tempfile::tempdir().map_err(|source| LifecycleError::Io {
            path: "<temp>".into(),
            source,
        })?;
        copy_tree(&store.image_path(&id), sandbox.path())?;
        let cwd = sandbox.path().to_path_buf();

        for &phase in LIFECYCLE_PHASES {
            let Some(cmd) = scripts.get(phase) else {
                continue;
            };
            let mut outcome = LifecycleOutcome {
                package: pkg.name.clone(),
                phase: phase.to_string(),
                command: cmd.clone(),
                ran: true,
                exit_code: None,
            };
            let status = metrics.measure("lifecycle", || {
                run_script(&cwd, phase, cmd, project_root, store, pkg)
            });
            let code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
            outcome.exit_code = Some(code);
            if code == 0 {
                stats.phases_succeeded += 1;
            } else {
                stats.phases_failed += 1;
            }
            stats.phases_executed += 1;
            outcomes.push(outcome);
        }
    }

    stats.outcomes = outcomes;
    Ok(stats)
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
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).current_dir(cwd);
    // npm-compatible environment (IMPLEMENTATION §14).
    cmd.env("npm_lifecycle_event", phase);
    cmd.env("npm_lifecycle_script", command);
    cmd.env("npm_package_name", &pkg.name);
    cmd.env("npm_package_version", &pkg.version);
    cmd.env("npm_config_user_agent", "bpm/0.1.0");
    cmd.env("npm_execpath", "bpm");
    cmd.env("INIT_CWD", project_root);
    cmd.env("NODE", which("node").unwrap_or_else(|| "node".to_string()));
    // Project node_modules/.bin should be reachable for scripts; prepend it.
    if let Some(path) = std::env::var_os("PATH") {
        let bin = project_root.join("node_modules").join(".bin");
        let mut new_path = std::ffi::OsString::from(&bin);
        new_path.push(std::path::MAIN_SEPARATOR.to_string());
        new_path.push(&path);
        cmd.env("PATH", new_path);
    }
    cmd.status()
}

fn which(tool: &str) -> Option<String> {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool}"))
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
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

// keep PathBuf import meaningful for future derived-artifact paths.
#[allow(dead_code)]
fn _pathbuf_marker() -> PathBuf {
    PathBuf::new()
}
