//! Lockfile import orchestration (npm package-lock v2/v3, yarn v1).

use std::path::{Path, PathBuf};

use anyhow::Context;
use bpm::lockfile::{Lockfile, BPM_LOCK_FILE};
use bpm::project_lock::{load_npm_package_lock, load_yarn_lock};
use serde::Serialize;

#[derive(Serialize)]
struct ImportJson<'a> {
    wrote: String,
    package_count: usize,
    diagnostics: &'a [bpm::Diagnostic],
    lockfile: &'a Lockfile,
}

pub(super) fn run(path: Option<PathBuf>, out: Option<PathBuf>, json: bool) -> anyhow::Result<()> {
    let input = match path {
        Some(path) => path,
        // Auto-detect the conventional lockfiles in the working directory.
        None => {
            if Path::new("package-lock.json").exists() || !Path::new("yarn.lock").exists() {
                PathBuf::from("package-lock.json")
            } else {
                PathBuf::from("yarn.lock")
            }
        }
    };
    // Dispatch by name, then shape: a file NAMED yarn.lock whose content is
    // JSON is a renamed npm lock (explicit paths never force-format), and a
    // yarn.lock always begins with its header comment or a bare spec key.
    let is_yarn = input
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "yarn.lock")
        && !std::fs::read_to_string(&input)
            .map(|text| text.trim_start().starts_with('{'))
            .unwrap_or(false);
    let bpm::ImportReport {
        lockfile,
        diagnostics,
    } = if is_yarn {
        load_yarn_lock(&input)
            .context("bpm import accepts npm package-lock.json v2/v3 and yarn v1 yarn.lock")?
    } else {
        load_npm_package_lock(&input)
            .context("bpm import accepts only npm package-lock.json v2/v3")?
    };
    let out_path = out.unwrap_or_else(|| {
        input
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join(BPM_LOCK_FILE))
            .unwrap_or_else(|| PathBuf::from(BPM_LOCK_FILE))
    });
    lockfile.write_to(&out_path)?;

    if json {
        let payload = ImportJson {
            wrote: out_path.display().to_string(),
            package_count: lockfile.packages.len(),
            diagnostics: &diagnostics,
            lockfile: &lockfile,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .map_err(|e| anyhow::anyhow!("failed to serialize import result: {e}"))?
        );
    } else {
        println!(
            "imported {} packages into {}",
            lockfile.packages.len(),
            out_path.display()
        );
        for diagnostic in &diagnostics {
            let package = diagnostic
                .package
                .as_deref()
                .map(|value| format!(" (in {value})"))
                .unwrap_or_default();
            eprintln!(
                "{}[{}] {}{}",
                diagnostic.severity.as_str(),
                diagnostic.code,
                diagnostic.message,
                package
            );
        }
    }
    Ok(())
}
