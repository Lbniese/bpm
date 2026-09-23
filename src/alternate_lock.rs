//! Compatibility shim for disabled alternate-manager lock imports.
//!
//! Use [`crate::project_lock::load_npm_package_lock`] to import npm package-lock
//! v2/v3 files. Alternate formats cannot safely preserve physical placement.

use std::path::Path;

use crate::lockfile::Lockfile;

#[derive(Debug, thiserror::Error)]
pub enum AlternateLockError {
    #[error("cannot read lockfile {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("unsupported or malformed alternate lockfile: {0}")]
    Parse(String),
}

/// Always refuses alternate-manager imports without reading the input path.
#[deprecated(note = "use project_lock::load_npm_package_lock for npm package-lock.json v2/v3")]
pub fn import(_path: &Path) -> Result<Lockfile, AlternateLockError> {
    Err(AlternateLockError::Parse(
        "alternate-manager locks are disabled because their physical placement cannot be represented safely".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(deprecated)] // Exercise the public compatibility entry point.
    fn import_refuses_missing_and_valid_looking_paths() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.lock");
        let yarn = dir.path().join("yarn.lock");
        let content = b"left-pad@^1.3.0:\n  version \"1.3.0\"\n  resolved \"https://registry/left-pad.tgz\"\n\nrepeat-string@^1.0.0:\n  version \"1.6.1\"\n";
        std::fs::write(&yarn, content).unwrap();
        for path in [&missing, &yarn] {
            match import(path).unwrap_err() {
                AlternateLockError::Parse(message) => assert_eq!(
                    message,
                    "alternate-manager locks are disabled because their physical placement cannot be represented safely"
                ),
                error => panic!("expected refusal before reading {}: {error}", path.display()),
            }
        }
        assert!(!missing.exists());
        assert_eq!(std::fs::read(yarn).unwrap(), content);
        assert!(!dir.path().join("bpm.lock").exists());
    }
}
