//! Cross-platform path-safety validation for package paths, workspace targets,
//! bin names, and bin targets.
//!
//! Every function in this module performs lexical validation **before** any
//! filesystem mutation.  No `canonicalize`, no post-join prefix check, no
//! best-effort cleanup.
//!
//! Rules are platform-portable and reject ambiguous or hostile input on all
//! supported operating systems.

use thiserror::Error;

/// Errors raised when a path, name, or target fails safety validation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PathSafetyError {
    #[error("package path {path:?} is invalid: {reason}")]
    InvalidPackagePath { path: String, reason: String },
    #[error("workspace target {target:?} is invalid: {reason}")]
    InvalidWorkspaceTarget { target: String, reason: String },
    #[error("bin name {name:?} is invalid: {reason}")]
    InvalidBinName { name: String, reason: String },
    #[error("bin target {target:?} is invalid: {reason}")]
    InvalidBinTarget { target: String, reason: String },
}

/// Windows reserved device names (case-insensitive, no extension).
const WINDOWS_DEVICE_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Return `true` if `part` is a valid single-path-segment component (no
/// empty, `.`, `..`, or other special forms).
fn is_valid_component(part: &str) -> bool {
    !part.is_empty() && part != "." && part != ".."
}

/// Check that `path` is a normalized relative POSIX path containing no
/// root, drive prefix, backslash, empty, `.`, or `..` component.
fn is_normalized_relative(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.starts_with('\\') {
        return false;
    }
    if path.as_bytes().get(1) == Some(&b':') {
        return false; // drive prefix
    }
    if path.contains('\\') {
        return false;
    }
    path.split('/').all(is_valid_component)
}

// ── Public validators ───────────────────────────────────────────────────

/// Validate a package path, which must be an npm-shaped normalized relative
/// path: `node_modules/<name>` or nested `<package>/node_modules/<name>`.
///
/// A scoped name (`@scope/name`) contributes exactly two components.
pub fn validate_package_path(path: &str) -> Result<String, PathSafetyError> {
    if path.is_empty() {
        return Err(PathSafetyError::InvalidPackagePath {
            path: path.to_owned(),
            reason: "package path cannot be empty".into(),
        });
    }
    if !is_normalized_relative(path) {
        return Err(PathSafetyError::InvalidPackagePath {
            path: path.to_owned(),
            reason: "package path must be a normalized relative path without root, backslash, or traversal components".into(),
        });
    }
    // Must contain at least one `node_modules/<name>` segment.
    let segments: Vec<&str> = path.split('/').collect();
    let has_nm = segments
        .windows(2)
        .any(|w| w[0] == "node_modules" && !w[1].is_empty() && w[1] != "." && w[1] != "..");
    if !has_nm {
        return Err(PathSafetyError::InvalidPackagePath {
            path: path.to_owned(),
            reason: "package path must contain a node_modules/<name> segment".into(),
        });
    }
    Ok(path.to_owned())
}

/// Validate a workspace target path: a normalized project-relative path
/// allowing `packages/widget` but no escape.
pub fn validate_workspace_target(target: &str) -> Result<String, PathSafetyError> {
    if !is_normalized_relative(target) {
        return Err(PathSafetyError::InvalidWorkspaceTarget {
            target: target.to_owned(),
            reason: "workspace target must be a normalized relative path without root, backslash, or traversal components".into(),
        });
    }
    Ok(target.to_owned())
}

/// Validate a bin name: exactly one portable filename component.
///
/// Rejects empty, `.`, `..`, slash/backslash, colon, control characters,
/// Windows reserved device names, trailing dot/space.
pub fn validate_bin_name(name: &str) -> Result<String, PathSafetyError> {
    if name.is_empty() {
        return Err(PathSafetyError::InvalidBinName {
            name: name.to_owned(),
            reason: "bin name cannot be empty".into(),
        });
    }
    if name.contains('/') || name.contains('\\') {
        return Err(PathSafetyError::InvalidBinName {
            name: name.to_owned(),
            reason: "bin name must not contain path separators".into(),
        });
    }
    if name == "." || name == ".." {
        return Err(PathSafetyError::InvalidBinName {
            name: name.to_owned(),
            reason: "bin name must not be '.' or '..'".into(),
        });
    }
    if name.contains(':') {
        return Err(PathSafetyError::InvalidBinName {
            name: name.to_owned(),
            reason: "bin name must not contain colon".into(),
        });
    }
    if name.contains(|c: char| c.is_control()) {
        return Err(PathSafetyError::InvalidBinName {
            name: name.to_owned(),
            reason: "bin name must not contain control characters".into(),
        });
    }
    // Trim trailing dot or space for Windows compatibility.
    let trimmed = name.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        return Err(PathSafetyError::InvalidBinName {
            name: name.to_owned(),
            reason: "bin name is empty after trimming trailing dot/space".into(),
        });
    }
    // Reject Windows reserved device names (case-insensitive).
    let lower = trimmed.to_ascii_lowercase();
    if WINDOWS_DEVICE_NAMES.contains(&lower.as_str()) {
        return Err(PathSafetyError::InvalidBinName {
            name: name.to_owned(),
            reason: format!("bin name is a reserved Windows device name: {trimmed}"),
        });
    }
    Ok(trimmed.to_owned())
}

/// Validate a bin target: a non-empty package-relative path.
///
/// Allows a leading `./` prefix which is stripped.  Rejects root, backslash,
/// empty/`.`/`..` components, and any path outside the package image.
pub fn validate_bin_target(target: &str) -> Result<String, PathSafetyError> {
    // Strip leading `./` (npm convention).
    let normalized = if let Some(stripped) = target.strip_prefix("./") {
        stripped
    } else {
        target
    };
    if normalized.is_empty() {
        return Err(PathSafetyError::InvalidBinTarget {
            target: target.to_owned(),
            reason: "bin target is empty after normalization".into(),
        });
    }
    if !is_normalized_relative(normalized) {
        return Err(PathSafetyError::InvalidBinTarget {
            target: target.to_owned(),
            reason: "bin target must be a normalized relative path without root, backslash, or traversal components".into(),
        });
    }
    Ok(normalized.to_owned())
}

/// Validate a scoped-package string-form bin shorthand.
///
/// When npm's string-form `bin` shorthand is used on a scoped package
/// (`@scope/pkg`), the command name is derived as `pkg` (the basename).
/// Object-form bin keys are validated directly via [`validate_bin_name`].
pub fn scoped_string_bin_name(scoped_name: &str) -> Option<&str> {
    let (_, tail) = scoped_name.split_once('/')?;
    let basename = tail.rsplit('/').next()?;
    if basename.is_empty() {
        return None;
    }
    Some(basename)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── validate_package_path ────────────────────────────────────────

    #[test]
    fn valid_package_paths() {
        assert!(validate_package_path("node_modules/foo").is_ok());
        assert!(validate_package_path("node_modules/@scope/pkg").is_ok());
        assert!(validate_package_path("node_modules/a/node_modules/b").is_ok());
        assert!(validate_package_path("node_modules/a/node_modules/@scope/pkg").is_ok());
    }

    #[test]
    fn rejects_traversal_package_path() {
        assert!(validate_package_path("../escape").is_err());
        assert!(validate_package_path("node_modules/foo/../../escape").is_err());
    }

    #[test]
    fn rejects_absolute_package_path() {
        assert!(validate_package_path("/absolute").is_err());
    }

    #[test]
    fn rejects_drive_prefix() {
        assert!(validate_package_path("C:/foo").is_err());
    }

    #[test]
    fn rejects_backslash() {
        assert!(validate_package_path("node_modules\\foo").is_err());
    }

    #[test]
    fn rejects_empty_or_dot_components() {
        assert!(validate_package_path("node_modules/foo/").is_err());
        assert!(validate_package_path("node_modules/foo/./bar").is_err());
        assert!(validate_package_path("node_modules/foo/../bar").is_err());
    }

    #[test]
    fn rejects_path_without_node_modules() {
        assert!(validate_package_path("foo").is_err());
        assert!(validate_package_path("packages/foo").is_err());
    }

    // ── validate_bin_name ─────────────────────────────────────────────

    #[test]
    fn valid_bin_names() {
        assert!(validate_bin_name("demo").is_ok());
        assert!(validate_bin_name("my-cli").is_ok());
        assert!(validate_bin_name("foo_bar").is_ok());
        assert!(validate_bin_name("babel").is_ok());
    }

    #[test]
    fn rejects_traversal_bin_name() {
        assert!(validate_bin_name("../outside").is_err());
    }

    #[test]
    fn rejects_bin_name_with_slash() {
        assert!(validate_bin_name("scope/name").is_err());
        assert!(validate_bin_name("a/b").is_err());
    }

    #[test]
    fn rejects_windows_device_name() {
        assert!(validate_bin_name("CON").is_err());
        assert!(validate_bin_name("nul").is_err());
        assert!(validate_bin_name("LPT1").is_err());
    }

    #[test]
    fn rejects_control_characters() {
        assert!(validate_bin_name("bad\x00").is_err());
        assert!(validate_bin_name("bad\n").is_err());
    }

    #[test]
    fn rejects_dot_and_dotdot() {
        assert!(validate_bin_name(".").is_err());
        assert!(validate_bin_name("..").is_err());
    }

    // ── validate_bin_target ───────────────────────────────────────────

    #[test]
    fn valid_bin_targets() {
        assert!(validate_bin_target("cli.js").is_ok());
        assert!(validate_bin_target("./cli.js").is_ok());
        assert!(validate_bin_target("bin/start.js").is_ok());
        assert!(validate_bin_target("./lib/runner.js").is_ok());
    }

    #[test]
    fn rejects_traversal_bin_target() {
        assert!(validate_bin_target("../outside").is_err());
        assert!(validate_bin_target("../../outside").is_err());
    }

    #[test]
    fn rejects_absolute_bin_target() {
        assert!(validate_bin_target("/outside").is_err());
    }

    #[test]
    fn rejects_drive_prefix_bin_target() {
        assert!(validate_bin_target("C:/outside").is_err());
    }

    #[test]
    fn rejects_backslash_bin_target() {
        assert!(validate_bin_target("dir\\file.js").is_err());
    }

    // ── scoped_string_bin_name ────────────────────────────────────────

    #[test]
    fn scoped_string_bin_derives_package_name() {
        assert_eq!(scoped_string_bin_name("@scope/pkg"), Some("pkg"));
        assert_eq!(scoped_string_bin_name("@scope/my-pkg"), Some("my-pkg"));
    }

    #[test]
    fn scoped_string_bin_handles_unscoped_package() {
        assert_eq!(scoped_string_bin_name("pkg"), None);
    }

    // ── validate_workspace_target ─────────────────────────────────────

    #[test]
    fn valid_workspace_targets() {
        assert!(validate_workspace_target("packages/widget").is_ok());
        assert!(validate_workspace_target("apps/backend").is_ok());
    }

    #[test]
    fn rejects_traversal_workspace_target() {
        assert!(validate_workspace_target("../escape").is_err());
        assert!(validate_workspace_target("packages/../escape").is_err());
    }
}
