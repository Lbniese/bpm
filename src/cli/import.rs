//! Lockfile import orchestration (npm package-lock v2/v3, yarn v1).

use std::path::{Path, PathBuf};

use anyhow::Context;
use bpm::lockfile::{Lockfile, BPM_LOCK_FILE};
use bpm::project_lock::{load_npm_package_lock, load_pnpm_lock, load_yarn_lock};
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
            if Path::new("package-lock.json").exists() {
                PathBuf::from("package-lock.json")
            } else if Path::new("yarn.lock").exists() {
                PathBuf::from("yarn.lock")
            } else {
                PathBuf::from("pnpm-lock.yaml")
            }
        }
    };
    // Dispatch by name, then shape: a file NAMED yarn.lock whose content is
    // JSON is a renamed npm lock (explicit paths never force-format), and a
    // yarn.lock always begins with its header comment or a bare spec key.
    let content = std::fs::read_to_string(&input).context("cannot read the lockfile to import")?;
    let file_name = input
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string();
    let is_yarn = file_name == "yarn.lock" && !content.trim_start().starts_with('{');
    let is_pnpm = file_name == "pnpm-lock.yaml" && content.contains("lockfileVersion:");
    let bpm::ImportReport {
        lockfile,
        diagnostics,
    } = if is_pnpm {
        load_pnpm_lock(&input).context(
            "bpm import accepts npm package-lock.json v2/v3, yarn v1 yarn.lock, and pnpm v9 pnpm-lock.yaml",
        )?
    } else if is_yarn {
        load_yarn_lock(&input).context(
            "bpm import accepts npm package-lock.json v2/v3, yarn v1 yarn.lock, and pnpm v9 pnpm-lock.yaml",
        )?
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
