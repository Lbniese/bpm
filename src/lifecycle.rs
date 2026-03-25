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
        // Hold the sandbox (if any) until every phase has run.
        drop(sandbox);
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
}
