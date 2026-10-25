//! Package-manager-provided tools for lifecycle scripts.
//!
//! npm ships `node-gyp` inside its own install and exposes it to every
//! lifecycle script; pnpm and yarn bundle their own copy too. Native-module
//! `install: node-gyp rebuild` scripts therefore "just work" under those
//! managers and fail with exit 127 under BPM.
//!
//! BPM provides the same guarantee without vendoring bytes into the binary:
//! the first install whose lifecycle scripts reference `node-gyp` creates a
//! managed tool project under `<store>/tools/node-gyp-<spec>/` by running
//! BPM's own install pipeline as a child process (`bpm install
//! --ignore-scripts` against a generated manifest pinning the spec). The
//! resulting `node_modules/.bin` is prepended to script PATHs after the
//! tree's own `.bin` chains, so a project's own node-gyp dependency still
//! takes precedence, matching npm's resolution order. Warm installs reuse
//! the graph volume and complete in milliseconds.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// The managed node-gyp version. Tools are pinned like any dependency;
/// operators can override with `BPM_NODE_GYP` (any npm spec, e.g. `11`).
pub const NODE_GYP_SPEC: &str = "13.1.0";

#[derive(Debug, Error)]
pub enum ToolsError {
    #[error("cannot determine running BPM executable for tool install: {0}")]
    CurrentExe(String),
    #[error("managed node-gyp install failed (exit {code}): {stderr}")]
    InstallFailed { code: i32, stderr: String },
    #[error("io error during tool setup at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

fn io_err(path: &Path, source: std::io::Error) -> ToolsError {
    ToolsError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Filesystem-safe form of the spec for the tool directory name.
fn spec_dir_component(spec: &str) -> String {
    let cleaned: String = spec
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "default".to_string()
    } else {
        cleaned
    }
}

/// Ensure the managed node-gyp tool exists and return its
/// `node_modules/.bin` directory for lifecycle PATHs.
///
/// Idempotent and safe to call concurrently: publication is serialized by a
/// store lock keyed on the spec, and an existing complete install (marker
/// recording the spec + a `node-gyp` bin) short-circuits the child install.
pub fn ensure_node_gyp(store: &crate::store::ArtifactStore) -> Result<PathBuf, ToolsError> {
    let spec = std::env::var("BPM_NODE_GYP")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| NODE_GYP_SPEC.to_string());
    let dir = store
        .root()
        .join("tools")
        .join(format!("node-gyp-{}", spec_dir_component(&spec)));
    let marker = dir.join(".bpm-tool.json");
    let bin = dir.join("node_modules").join(".bin");
    let marker_body = format!("{{\"node-gyp\":\"{spec}\"}}\n");
    if bin.join("node-gyp").exists()
        && fs::read_to_string(&marker).ok().as_deref() == Some(marker_body.as_str())
    {
        return Ok(bin);
    }

    let _guard = store
        .acquire_lock("tools-node-gyp")
        .map_err(|source| ToolsError::Io {
            path: "<tools lock>".into(),
            source: std::io::Error::other(source.to_string()),
        })?;
    // Recheck under the lock: a concurrent install may have completed it.
    if bin.join("node-gyp").exists()
        && fs::read_to_string(&marker).ok().as_deref() == Some(marker_body.as_str())
    {
        return Ok(bin);
    }

    fs::create_dir_all(&dir).map_err(|source| io_err(&dir, source))?;
    let manifest = format!(
        "{{\"name\":\"bpm-tools\",\"version\":\"0.0.0\",\"private\":true,\
         \"dependencies\":{{\"node-gyp\":\"{spec}\"}}}}\n"
    );
    fs::write(dir.join("package.json"), &manifest).map_err(|source| io_err(&dir, source))?;
    // A stale lock from an aborted run would pin a previous resolution;
    // remove it so the child install resolves the spec afresh.
    let _ = fs::remove_file(dir.join("bpm.lock"));

    let exe =
        std::env::current_exe().map_err(|source| ToolsError::CurrentExe(source.to_string()))?;
    let output = std::process::Command::new(&exe)
        .arg("install")
        .arg("--ignore-scripts")
        .arg("--store")
        .arg(store.root())
        // The tool project is an implementation detail, not a user project:
        // keep its output out of the install log unless it fails.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .current_dir(&dir)
        .output()
        .map_err(|source| io_err(&exe, source))?;
    if !output.status.success() {
        return Err(ToolsError::InstallFailed {
            code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    if !bin.join("node-gyp").exists() {
        return Err(ToolsError::InstallFailed {
            code: 0,
            stderr: format!(
                "install succeeded but {} is missing",
                bin.join("node-gyp").display()
            ),
        });
    }
    fs::write(&marker, &marker_body).map_err(|source| io_err(&marker, source))?;
    Ok(bin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_dir_component_is_filesystem_safe() {
        assert_eq!(spec_dir_component("13.1.0"), "13.1.0");
        assert_eq!(spec_dir_component("^11 || ^12"), "_11_____12");
        assert_eq!(spec_dir_component(""), "default");
    }
}
