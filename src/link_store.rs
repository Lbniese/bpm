//! Global developer-link registry backing `bpm link` / `bpm unlink`.
//!
//! `bpm link` (run inside a package) registers that package globally as a
//! symlink `$BPM_STORE/links/<name>` -> the package directory. A consumer
//! project then runs `bpm link <name>`, which records a `file:` dependency
//! pointing at that symlink so the normal install flow materializes
//! `node_modules/<name>` -> the registered target. This mirrors npm's two-step
//! `npm link` model.
//!
//! The registry itself is just a directory of symlinks under `<store_root>/links`.
//! Re-registering a name repoints the symlink; consumers pick up the new target
//! on their next `bpm install` (their `package.json` points at the symlink, not
//! the resolved target, so resolution follows the repoint).

use std::path::{Path, PathBuf};

use anyhow::Context;

/// Subdirectory under the store root holding registered link targets.
pub const LINKS_DIR: &str = "links";

/// A handle to the global link registry rooted at `<store_root>/links`.
#[derive(Debug, Clone)]
pub struct LinkStore {
    root: PathBuf,
}

impl LinkStore {
    /// Open the registry for a given store root. The `links/` directory is
    /// created lazily on first [`Self::register`].
    pub fn new(store_root: &Path) -> Self {
        Self {
            root: store_root.join(LINKS_DIR),
        }
    }

    /// The registry root (`<store_root>/links`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Register `name` -> `target_dir`, replacing any existing registration.
    /// Returns the created link path.
    pub fn register(&self, name: &str, target_dir: &Path) -> anyhow::Result<PathBuf> {
        validate_link_name(name)?;
        if !target_dir.is_dir() {
            anyhow::bail!(
                "cannot register '{}': target {} is not a directory",
                name,
                target_dir.display()
            );
        }
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating link registry {}", self.root.display()))?;
        let link = self.root.join(name);
        if link.is_symlink() || link.exists() {
            remove_any(&link).with_context(|| format!("removing stale link {}", link.display()))?;
        }
        make_symlink(target_dir, &link)
            .with_context(|| format!("linking {} -> {}", link.display(), target_dir.display()))?;
        Ok(link)
    }

    /// Remove a registration. Returns `true` if a registration was removed.
    pub fn unregister(&self, name: &str) -> anyhow::Result<bool> {
        validate_link_name(name)?;
        let link = self.root.join(name);
        if link.is_symlink() || link.exists() {
            remove_any(&link).with_context(|| format!("removing link {}", link.display()))?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Resolve a registered name to its canonical target directory, if the
    /// registration exists and points at an existing directory.
    pub fn resolve(&self, name: &str) -> Option<PathBuf> {
        let link = self.root.join(name);
        match std::fs::canonicalize(&link) {
            Ok(path) if path.is_dir() => Some(path),
            _ => None,
        }
    }

    /// List the names of all registered links, sorted.
    pub fn list(&self) -> anyhow::Result<Vec<String>> {
        let mut names = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                if entry.path().is_symlink() {
                    if let Some(name) = entry.file_name().to_str() {
                        names.push(name.to_string());
                    }
                }
            }
        }
        names.sort();
        Ok(names)
    }
}

/// Reject names that could escape the registry directory or collide with path
/// traversal. Registered names come from a `package.json` `name` field, which is
/// already constrained, but guard regardless.
fn validate_link_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name == "."
        || name == ".."
        || name.contains('\0')
    {
        anyhow::bail!("invalid link name '{name}'");
    }
    Ok(())
}

/// Remove a symlink, regular file, or empty directory at `path`.
fn remove_any(path: &Path) -> std::io::Result<()> {
    // A symlink (including one pointing at a directory) is removed as a file on
    // Unix; on Windows a directory symlink/junction needs `remove_dir`. Try
    // file first, then directory, returning the directory error if both fail.
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(_) => std::fs::remove_dir(path),
    }
}

/// Create a directory symlink `link` -> `target` (cross-platform).
fn make_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlinks are not supported on this platform",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_pkg(temp: &Path, name: &str) -> PathBuf {
        let dir = temp.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("package.json"),
            format!(r#"{{"name":"{name}","version":"1.0.0"}}"#),
        )
        .unwrap();
        dir
    }

    #[test]
    fn register_creates_symlink_and_resolves() {
        let store = tempfile::tempdir().unwrap();
        let pkg = tempfile::tempdir().unwrap();
        let target = make_pkg(pkg.path(), "demo");
        let links = LinkStore::new(store.path());

        let created = links.register("demo", &target).unwrap();
        assert!(created.is_symlink());
        let resolved = links.resolve("demo").unwrap();
        assert_eq!(resolved, target.canonicalize().unwrap());
    }

    #[test]
    fn register_replaces_existing() {
        let store = tempfile::tempdir().unwrap();
        let pkg_a = tempfile::tempdir().unwrap();
        let pkg_b = tempfile::tempdir().unwrap();
        let a = make_pkg(pkg_a.path(), "a");
        let b = make_pkg(pkg_b.path(), "b");
        let links = LinkStore::new(store.path());

        links.register("demo", &a).unwrap();
        links.register("demo", &b).unwrap();
        assert_eq!(links.resolve("demo").unwrap(), b.canonicalize().unwrap());
    }

    #[test]
    fn unregister_removes_and_reports() {
        let store = tempfile::tempdir().unwrap();
        let pkg = tempfile::tempdir().unwrap();
        let target = make_pkg(pkg.path(), "demo");
        let links = LinkStore::new(store.path());

        links.register("demo", &target).unwrap();
        assert!(links.unregister("demo").unwrap());
        assert!(!links.unregister("demo").unwrap());
        assert!(links.resolve("demo").is_none());
    }

    #[test]
    fn list_returns_registered_names_sorted() {
        let store = tempfile::tempdir().unwrap();
        let pkg = tempfile::tempdir().unwrap();
        let beta = make_pkg(pkg.path(), "beta");
        let alpha = make_pkg(pkg.path(), "alpha");
        let links = LinkStore::new(store.path());

        links.register("beta", &beta).unwrap();
        links.register("alpha", &alpha).unwrap();
        assert_eq!(links.list().unwrap(), vec!["alpha", "beta"]);
    }

    #[test]
    fn rejects_invalid_names() {
        let store = tempfile::tempdir().unwrap();
        let links = LinkStore::new(store.path());
        assert!(links.register("../escape", store.path()).is_err());
        assert!(links.register("with/slash", store.path()).is_err());
        assert!(links.register("", store.path()).is_err());
    }
}
