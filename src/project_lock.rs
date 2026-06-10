//! Project lock discovery and package-lock loading boundary.
//!
//! This module selects the nearest supported project lock without mutating the
//! source files. Directory-local precedence is `bpm.lock` first, then npm
//! `package-lock.json` v3; only when neither exists in the current directory do
//! we walk to the parent.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::diagnostic::{sort_diagnostics, Diagnostic};
use crate::lockfile::{Lockfile, LockfileError, BPM_LOCK_FILE};
use crate::manifest::{ManifestError, PackageManifest};
use crate::npm_lock::{self, ImportReport, NpmLockError};

/// The npm project lock filename supported for read-only direct installs.
pub const NPM_PACKAGE_LOCK_FILE: &str = "package-lock.json";

/// The selected project lock source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectLockKind {
    /// Native BPM lockfile.
    Bpm,
    /// npm `package-lock.json` with `lockfileVersion: 3`.
    NpmV3,
}

impl ProjectLockKind {
    /// Human-readable filename for diagnostics and help text.
    pub fn filename(self) -> &'static str {
        match self {
            Self::Bpm => BPM_LOCK_FILE,
            Self::NpmV3 => NPM_PACKAGE_LOCK_FILE,
        }
    }
}

/// A discovered, normalized project lock.
#[derive(Debug)]
pub struct ProjectLock {
    pub path: PathBuf,
    pub project_root: PathBuf,
    pub kind: ProjectLockKind,
    pub lockfile: Lockfile,
    pub diagnostics: Vec<Diagnostic>,
}

/// Error while discovering or loading a project lock.
#[derive(Debug, Error)]
pub enum ProjectLockError {
    #[error("failed to read project lock at {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to load bpm.lock at {path}: {source}")]
    BpmLock { path: String, source: LockfileError },
    #[error("failed to load package-lock.json at {path}: {source}")]
    NpmLock { path: String, source: NpmLockError },
    #[error("failed to load package.json at {path}: {source}")]
    Manifest { path: String, source: ManifestError },
}

/// Read and normalize an npm `package-lock.json` v3 from `path`, enriching it
/// from the sibling `package.json` when present. The input lock is never
/// modified and no `bpm.lock` is created.
pub fn load_npm_package_lock(path: &Path) -> Result<ImportReport, ProjectLockError> {
    let text = fs::read_to_string(path).map_err(|source| ProjectLockError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let ImportReport {
        mut lockfile,
        diagnostics,
    } = npm_lock::import(&text).map_err(|source| ProjectLockError::NpmLock {
        path: path.display().to_string(),
        source,
    })?;

    let project_root = path.parent().unwrap_or_else(|| Path::new("."));
    let manifest_path = project_root.join("package.json");
    if manifest_path.is_file() {
        let manifest = PackageManifest::from_path(&manifest_path).map_err(|source| {
            ProjectLockError::Manifest {
                path: manifest_path.display().to_string(),
                source,
            }
        })?;
        npm_lock::apply_manifest_root_metadata(&mut lockfile, &manifest).map_err(|source| {
            ProjectLockError::NpmLock {
                path: path.display().to_string(),
                source,
            }
        })?;
    }

    Ok(ImportReport {
        lockfile,
        diagnostics,
    })
}

/// Validate that imported npm diagnostics are safe for direct install/CI.
pub fn validate_npm_direct_install(diagnostics: &[Diagnostic]) -> Result<(), ProjectLockError> {
    let mut blocking = diagnostics
        .iter()
        .filter(|diagnostic| {
            matches!(
                diagnostic.code,
                "LINK_PACKAGE_UNSUPPORTED" | "MISSING_RESOLVED"
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    sort_diagnostics(&mut blocking);
    if blocking.is_empty() {
        return Ok(());
    }
    let messages = blocking
        .iter()
        .map(|diagnostic| {
            let package = diagnostic
                .package
                .as_deref()
                .map(|value| format!(" (in {value})"))
                .unwrap_or_default();
            format!("{}{}: {}", diagnostic.code, package, diagnostic.message)
        })
        .collect::<Vec<_>>()
        .join("; ");
    Err(ProjectLockError::NpmLock {
        path: NPM_PACKAGE_LOCK_FILE.to_string(),
        source: NpmLockError::DirectInstallUnsupported(messages),
    })
}

/// Look upward from `start` for the nearest supported project lock.
pub fn find_project_lock(start: &Path) -> Result<Option<ProjectLock>, ProjectLockError> {
    let mut dir: Option<&Path> = Some(start);
    while let Some(current) = dir {
        let bpm_path = current.join(BPM_LOCK_FILE);
        if bpm_path.is_file() {
            let lockfile =
                Lockfile::from_path(&bpm_path).map_err(|source| ProjectLockError::BpmLock {
                    path: bpm_path.display().to_string(),
                    source,
                })?;
            return Ok(Some(ProjectLock {
                path: bpm_path,
                project_root: current.to_path_buf(),
                kind: ProjectLockKind::Bpm,
                lockfile,
                diagnostics: Vec::new(),
            }));
        }

        let npm_path = current.join(NPM_PACKAGE_LOCK_FILE);
        if npm_path.is_file() {
            let ImportReport {
                lockfile,
                diagnostics,
            } = load_npm_package_lock(&npm_path)?;
            return Ok(Some(ProjectLock {
                path: npm_path,
                project_root: current.to_path_buf(),
                kind: ProjectLockKind::NpmV3,
                lockfile,
                diagnostics,
            }));
        }

        dir = current.parent();
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::diagnostic::Severity;
    use crate::lockfile::{PackageEntry, RootEntry};

    fn bpm_lock() -> Lockfile {
        let mut lockfile = Lockfile::new("test");
        lockfile.root = RootEntry {
            name: Some("app".into()),
            version: Some("1.0.0".into()),
            dependencies: BTreeMap::from([("foo".into(), "1.0.0".into())]),
        };
        lockfile.packages.push(PackageEntry {
            path: "node_modules/foo".into(),
            name: "foo".into(),
            version: "1.0.0".into(),
            resolved: "file:///tmp/foo.tgz".into(),
            integrity: Some("sha512-abc".into()),
            ..Default::default()
        });
        lockfile
    }

    fn npm_v3() -> &'static str {
        r#"{"name":"app","lockfileVersion":3,"packages":{"":{"name":"app","version":"1.0.0","dependencies":{"foo":"1.0.0"}},"node_modules/foo":{"version":"1.0.0","resolved":"file:///tmp/foo.tgz","integrity":"sha512-abc"}}}"#
    }

    #[test]
    fn selects_bpm_lock_when_it_is_the_only_lock() {
        let temp = tempfile::tempdir().unwrap();
        bpm_lock()
            .write_to(&temp.path().join(BPM_LOCK_FILE))
            .unwrap();

        let selected = find_project_lock(temp.path()).unwrap().unwrap();

        assert_eq!(selected.kind, ProjectLockKind::Bpm);
        assert_eq!(selected.project_root, temp.path());
    }

    #[test]
    fn loads_npm_lock_without_creating_bpm_lock() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(NPM_PACKAGE_LOCK_FILE), npm_v3()).unwrap();

        let selected = find_project_lock(temp.path()).unwrap().unwrap();

        assert_eq!(selected.kind, ProjectLockKind::NpmV3);
        assert_eq!(selected.lockfile.packages.len(), 1);
        assert!(!temp.path().join(BPM_LOCK_FILE).exists());
    }

    #[test]
    fn sibling_bpm_lock_wins_over_package_lock() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(NPM_PACKAGE_LOCK_FILE), npm_v3()).unwrap();
        bpm_lock()
            .write_to(&temp.path().join(BPM_LOCK_FILE))
            .unwrap();

        let selected = find_project_lock(temp.path()).unwrap().unwrap();

        assert_eq!(selected.kind, ProjectLockKind::Bpm);
    }

    #[test]
    fn nested_package_lock_wins_over_ancestor_bpm_lock() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("child");
        fs::create_dir(&child).unwrap();
        bpm_lock()
            .write_to(&temp.path().join(BPM_LOCK_FILE))
            .unwrap();
        fs::write(child.join(NPM_PACKAGE_LOCK_FILE), npm_v3()).unwrap();

        let selected = find_project_lock(&child).unwrap().unwrap();

        assert_eq!(selected.kind, ProjectLockKind::NpmV3);
        assert_eq!(selected.project_root, child);
    }

    #[test]
    fn unsupported_npm_versions_are_path_aware() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join(NPM_PACKAGE_LOCK_FILE),
            r#"{"lockfileVersion":2,"packages":{}}"#,
        )
        .unwrap();

        let error = find_project_lock(temp.path()).unwrap_err().to_string();

        assert!(error.contains("package-lock.json"), "{error}");
        assert!(error.contains("unsupported lockfileVersion 2"), "{error}");
    }

    #[test]
    fn malformed_npm_locks_are_path_aware() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(NPM_PACKAGE_LOCK_FILE), "{").unwrap();

        let error = find_project_lock(temp.path()).unwrap_err().to_string();

        assert!(error.contains("package-lock.json"), "{error}");
        assert!(error.contains("failed to parse"), "{error}");
    }

    #[test]
    fn missing_sibling_manifest_is_allowed_and_read_only() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(NPM_PACKAGE_LOCK_FILE), npm_v3()).unwrap();

        let selected = find_project_lock(temp.path()).unwrap().unwrap();

        assert_eq!(selected.kind, ProjectLockKind::NpmV3);
        assert!(!temp.path().join(BPM_LOCK_FILE).exists());
    }

    #[test]
    fn blocking_diagnostics_reject_direct_install_in_stable_order() {
        let diagnostics = vec![
            Diagnostic::new(Severity::Info, "PLATFORM_CONSTRAINT", "accepted"),
            Diagnostic::new(Severity::Warning, "MISSING_RESOLVED", "missing").with_package("b"),
            Diagnostic::new(Severity::Warning, "LINK_PACKAGE_UNSUPPORTED", "link")
                .with_package("a"),
        ];

        let error = validate_npm_direct_install(&diagnostics)
            .unwrap_err()
            .to_string();

        let link = error.find("LINK_PACKAGE_UNSUPPORTED").unwrap();
        let missing = error.find("MISSING_RESOLVED").unwrap();
        assert!(link < missing, "{error}");
        assert!(!error.contains("PLATFORM_CONSTRAINT"), "{error}");
    }
}
