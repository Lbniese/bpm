//! Repository and project root discovery.
//!
//! "Project root" is the nearest ancestor of a starting path that contains a
//! `package.json`. "Repository root" is the nearest ancestor that contains a
//! `.git` marker; when no `.git` is found we fall back to the project root so a
//! non-Git checkout still works.
//!
//! Discovery never touches the network, never canonicalizes paths (which would
//! be an extra syscall and could leak host-specific absolute paths into cache
//! keys), and enumerates parents deterministically from the start path upward.

use std::path::{Path, PathBuf};
use thiserror::Error;

/// A marker filename indicating a repository root.
const GIT_MARKER: &str = ".git";
/// A marker filename indicating an npm project root.
const MANIFEST: &str = "package.json";

/// Error locating a root.
#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("no package.json found in {start} or any parent")]
    NoManifest { start: String },
}

/// Resolve an absolute starting path without canonicalizing it.
///
/// Canonicalization would add an extra syscall round-trip and could fold
/// host-specific absolute paths into cache keys; we only need a stable root
/// for upward traversal, so a relative start is joined to the process
/// current directory instead.
fn absolute(start: &Path) -> PathBuf {
    if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(start))
            .unwrap_or_else(|_| start.to_path_buf())
    }
}

fn parent_chain(start: &Path) -> impl Iterator<Item = PathBuf> {
    let root = absolute(start);
    std::iter::successors(Some(root), |p| p.parent().map(Path::to_path_buf))
}

/// Returns `true` if `dir` contains a `package.json` entry.
fn has_manifest(dir: &Path) -> bool {
    dir.join(MANIFEST).is_file()
}

/// Returns `true` if `dir` contains a `.git` directory or `.git` file (the
/// latter occurs in git worktrees/submodules).
fn has_git(dir: &Path) -> bool {
    let git = dir.join(GIT_MARKER);
    git.is_dir() || git.is_file()
}

/// Find the nearest ancestor (inclusive) of `start` containing `package.json`.
pub fn find_project_root(start: &Path) -> Result<PathBuf, ProjectError> {
    for dir in parent_chain(start) {
        if has_manifest(&dir) {
            return Ok(dir);
        }
    }
    Err(ProjectError::NoManifest {
        start: absolute(start).display().to_string(),
    })
}

/// Find the nearest ancestor (inclusive) of `start` containing a `.git` marker.
///
/// In a monorepo the `.git` marker commonly lives above the leaf package, so
/// search continues above the project root rather than stopping there. When no
/// `.git` is found at all, fall back to the project root so a non-Git checkout
/// still has a usable repository root for `bpm doctor`.
pub fn find_repository_root(start: &Path) -> Result<PathBuf, ProjectError> {
    let project_root = find_project_root(start)?;
    for dir in parent_chain(start) {
        if has_git(&dir) {
            return Ok(dir);
        }
    }
    Ok(project_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn mkdirs(root: &Path, rel: &str) -> PathBuf {
        let p = root.join(rel);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(dir: &Path, name: &str, contents: &str) {
        fs::write(dir.join(name), contents).unwrap();
    }

    #[test]
    fn finds_nearest_package_json() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write(root, "package.json", r#"{"name":"root"}"#);
        let sub = mkdirs(root, "packages/a/src");
        write(&sub.join(".."), "package.json", r#"{"name":"a"}"#);
        let found = find_project_root(&sub).unwrap();
        assert_eq!(found, root.join("packages/a"));
    }

    #[test]
    fn errors_when_no_manifest() {
        let tmp = tempdir().unwrap();
        let sub = mkdirs(tmp.path(), "deep/dir");
        let err = find_project_root(&sub).expect_err("should error");
        assert!(matches!(err, ProjectError::NoManifest { .. }));
    }

    #[test]
    fn prefers_git_marker_for_repository_root() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        write(root, "package.json", r#"{"name":"root"}"#);
        let sub = mkdirs(root, "apps/web/src");
        // apps/web/src/.. == apps/web
        write(&sub.join(".."), "package.json", r#"{"name":"web"}"#);
        // project root is the nested web package.
        assert_eq!(find_project_root(&sub).unwrap(), root.join("apps/web"));
        // repository root climbs to the .git marker at the monorepo root.
        assert_eq!(find_repository_root(&sub).unwrap(), root.to_path_buf());
    }

    #[test]
    fn falls_back_to_project_root_without_git() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write(root, "package.json", r#"{"name":"app"}"#);
        let sub = mkdirs(root, "src");
        assert_eq!(find_repository_root(&sub).unwrap(), root.to_path_buf());
    }
}
