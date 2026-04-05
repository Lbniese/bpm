//! Global developer-link registry backing `bpm link` / `bpm unlink`.
//!
//! `bpm link` (run inside a package) registers that package globally as a
//! symlink `$BPM_STORE/links/<name>` -> the package directory. Scoped packages
//! use `$BPM_STORE/links/@scope/pkg`, where `@scope` is a real directory. A
//! consumer records a `file:` dependency pointing at that registration so the
//! normal install flow materializes `node_modules/<name>` -> the target. This
//! mirrors npm's two-step `npm link` model.
//!
//! The registry is an npm-shaped tree of symlinks under `<store_root>/links`.
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

    /// Return the validated npm-shaped registration path for `name`.
    pub fn registration_path(&self, name: &str) -> anyhow::Result<PathBuf> {
        validate_link_name(name)?;
        if let Some((scope, package)) = name.split_once('/') {
            Ok(self.root.join(scope).join(package))
        } else {
            Ok(self.root.join(name))
        }
    }

    /// Register `name` -> `target_dir`, replacing any existing registration.
    /// Returns the created link path.
    pub fn register(&self, name: &str, target_dir: &Path) -> anyhow::Result<PathBuf> {
        let link = self.registration_path(name)?;
        if !target_dir.is_dir() {
            anyhow::bail!(
                "cannot register '{}': target {} is not a directory",
                name,
                target_dir.display()
            );
        }
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating link registry {}", self.root.display()))?;
        self.scope_dir(name, true)?;
        if link.is_symlink() || link.exists() {
            remove_any(&link).with_context(|| format!("removing stale link {}", link.display()))?;
        }
        make_symlink(target_dir, &link)
            .with_context(|| format!("linking {} -> {}", link.display(), target_dir.display()))?;
        Ok(link)
    }

    /// Remove a registration. Returns `true` if a registration was removed.
    pub fn unregister(&self, name: &str) -> anyhow::Result<bool> {
        let link = self.registration_path(name)?;
        let scope = self.scope_dir(name, false)?;
        if name.starts_with('@') && scope.is_none() {
            return Ok(false);
        }
        let removed = if link.is_symlink() || link.exists() {
            remove_any(&link).with_context(|| format!("removing link {}", link.display()))?;
            true
        } else {
            false
        };
        if removed {
            if let Some(scope) = scope {
                match std::fs::remove_dir(&scope) {
                    Ok(()) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                        ) => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("removing empty scope {}", scope.display()));
                    }
                }
            }
        }
        Ok(removed)
    }

    /// Resolve a registered name to its canonical target directory, if the
    /// registration exists and points at an existing directory.
    pub fn resolve(&self, name: &str) -> anyhow::Result<Option<PathBuf>> {
        let link = self.registration_path(name)?;
        if name.starts_with('@') && self.scope_dir(name, false)?.is_none() {
            return Ok(None);
        }
        Ok(match std::fs::canonicalize(&link) {
            Ok(path) if path.is_dir() => Some(path),
            _ => None,
        })
    }

    /// List unscoped and scoped registration names together, sorted.
    pub fn list(&self) -> anyhow::Result<Vec<String>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading link registry {}", self.root.display()));
            }
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", self.root.display()))?;
            let Some(root_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let file_type = entry
                .file_type()
                .with_context(|| format!("reading registration type {}", entry.path().display()))?;
            if file_type.is_symlink() && bpm_package_name_is_valid(&root_name) {
                names.push(root_name);
                continue;
            }
            if !root_name.starts_with('@') || !valid_scope_name(&root_name) {
                continue;
            }
            if file_type.is_symlink() || !file_type.is_dir() {
                anyhow::bail!(
                    "unsafe link scope '{}': {} must be a real directory",
                    root_name,
                    entry.path().display()
                );
            }
            for child in std::fs::read_dir(entry.path())
                .with_context(|| format!("reading link scope {}", entry.path().display()))?
            {
                let child = child?;
                let Some(package) = child.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let full_name = format!("{root_name}/{package}");
                if child.file_type()?.is_symlink() && bpm_package_name_is_valid(&full_name) {
                    names.push(full_name);
                }
            }
        }
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// Verify that a scoped name's structural container is a real directory.
    /// Missing containers are created only for registration.
    fn scope_dir(&self, name: &str, create: bool) -> anyhow::Result<Option<PathBuf>> {
        let Some((scope, _)) = name.split_once('/') else {
            return Ok(None);
        };
        let path = self.root.join(scope);
        loop {
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    anyhow::bail!(
                        "unsafe link scope '{scope}': {} is a symlink",
                        path.display()
                    );
                }
                Ok(metadata) if metadata.is_dir() => return Ok(Some(path)),
                Ok(_) => {
                    anyhow::bail!(
                        "invalid link scope '{scope}': {} is not a directory",
                        path.display()
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => {
                    return Ok(None);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match std::fs::create_dir(&path) {
                        Ok(()) => return Ok(Some(path)),
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!("creating link scope {}", path.display())
                            });
                        }
                    }
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("reading link scope {}", path.display()));
                }
            }
        }
    }
}

/// Link registrations intentionally use the strict registry npm-name policy,
/// which is a subset of the manifest parser's historical-name policy and
/// guarantees exactly one optional scope separator.
fn bpm_package_name_is_valid(name: &str) -> bool {
    crate::registry::is_valid_npm_name(name)
}

fn valid_scope_name(scope: &str) -> bool {
    bpm_package_name_is_valid(&format!("{scope}/package"))
}

fn validate_link_name(name: &str) -> anyhow::Result<()> {
    if !bpm_package_name_is_valid(name) {
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
        let resolved = links.resolve("demo").unwrap().unwrap();
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
        assert_eq!(
            links.resolve("demo").unwrap().unwrap(),
            b.canonicalize().unwrap()
        );
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
        assert!(links.resolve("demo").unwrap().is_none());
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
    fn scoped_registrations_share_a_real_scope_and_list_in_order() {
        let store = tempfile::tempdir().unwrap();
        let packages = tempfile::tempdir().unwrap();
        let plain = make_pkg(packages.path(), "plain");
        let first = make_pkg(packages.path(), "@scope/first");
        let second = make_pkg(packages.path(), "@scope/second");
        let links = LinkStore::new(store.path());

        links.register("@scope/second", &second).unwrap();
        let first_link = links.register("@scope/first", &first).unwrap();
        links.register("plain", &plain).unwrap();

        let scope = links.root().join("@scope");
        assert!(scope.is_dir());
        assert!(!scope.is_symlink());
        assert!(first_link.is_symlink());
        assert_eq!(
            links.resolve("@scope/first").unwrap().unwrap(),
            first.canonicalize().unwrap()
        );
        assert_eq!(
            links.list().unwrap(),
            vec!["@scope/first", "@scope/second", "plain"]
        );

        assert!(links.unregister("@scope/first").unwrap());
        assert!(scope.is_dir(), "nonempty scope was removed");
        assert!(links.resolve("@scope/second").unwrap().is_some());
        assert!(links.unregister("@scope/second").unwrap());
        assert!(!scope.exists(), "empty scope was not removed");
        assert!(links.resolve("plain").unwrap().is_some());
    }

    #[test]
    fn reregistering_one_scoped_package_leaves_its_sibling() {
        let store = tempfile::tempdir().unwrap();
        let packages_a = tempfile::tempdir().unwrap();
        let packages_b = tempfile::tempdir().unwrap();
        let first = make_pkg(packages_a.path(), "@scope/first");
        let replacement = make_pkg(packages_b.path(), "@scope/first");
        let sibling = make_pkg(packages_a.path(), "@scope/sibling");
        let links = LinkStore::new(store.path());

        links.register("@scope/first", &first).unwrap();
        links.register("@scope/sibling", &sibling).unwrap();
        links.register("@scope/first", &replacement).unwrap();

        assert_eq!(
            links.resolve("@scope/first").unwrap().unwrap(),
            replacement.canonicalize().unwrap()
        );
        assert_eq!(
            links.resolve("@scope/sibling").unwrap().unwrap(),
            sibling.canonicalize().unwrap()
        );
    }

    #[test]
    fn malformed_names_fail_every_named_operation() {
        let store = tempfile::tempdir().unwrap();
        let links = LinkStore::new(store.path());
        for name in [
            "",
            "../escape",
            "with/slash/extra",
            "@scope/",
            "@scope/../escape",
            "Uppercase",
            "nonascii-é",
            "with\\backslash",
        ] {
            assert!(links.registration_path(name).is_err(), "accepted {name}");
            assert!(
                links.register(name, store.path()).is_err(),
                "accepted {name}"
            );
            assert!(links.resolve(name).is_err(), "accepted {name}");
            assert!(links.unregister(name).is_err(), "accepted {name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn scoped_operations_never_follow_a_scope_symlink() {
        let store = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let target = make_pkg(target_root.path(), "target");
        let sentinel = outside.path().join("sentinel");
        fs::write(&sentinel, "safe").unwrap();
        let links = LinkStore::new(store.path());
        fs::create_dir_all(links.root()).unwrap();
        make_symlink(outside.path(), &links.root().join("@scope")).unwrap();

        assert!(links.register("@scope/pkg", &target).is_err());
        assert!(links.resolve("@scope/pkg").is_err());
        assert!(links.unregister("@scope/pkg").is_err());
        assert!(links.list().is_err());
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "safe");
        assert!(!outside.path().join("pkg").exists());
    }

    #[test]
    fn scoped_operations_reject_a_regular_file_container() {
        let store = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let target = make_pkg(target_root.path(), "target");
        let links = LinkStore::new(store.path());
        fs::create_dir_all(links.root()).unwrap();
        fs::write(links.root().join("@scope"), "not a directory").unwrap();

        assert!(links.register("@scope/pkg", &target).is_err());
        assert!(links.resolve("@scope/pkg").is_err());
        assert!(links.unregister("@scope/pkg").is_err());
        assert!(links.list().is_err());
    }
}
