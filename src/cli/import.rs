//! npm lockfile import orchestration.

use std::{fs, path::PathBuf};

use bpm::lockfile::{Lockfile, BPM_LOCK_FILE};
use bpm::manifest::PackageManifest;
use bpm::npm_lock::{apply_manifest_root_metadata, import as import_lock, ImportReport};
use serde::Serialize;

#[derive(Serialize)]
struct ImportJson<'a> {
    wrote: String,
    package_count: usize,
    diagnostics: &'a [bpm::Diagnostic],
    lockfile: &'a Lockfile,
}

pub(super) fn run(path: Option<PathBuf>, out: Option<PathBuf>, json: bool) -> anyhow::Result<()> {
    let input = path.unwrap_or_else(|| PathBuf::from("package-lock.json"));
    let (lockfile, diagnostics) =
        if input.file_name().and_then(|n| n.to_str()) == Some("package-lock.json") {
            let text = fs::read_to_string(&input)
                .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", input.display()))?;
            let ImportReport {
                mut lockfile,
                diagnostics,
            } = import_lock(&text)?;
            let manifest_path = input
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("package.json");
            if manifest_path.is_file() {
                let manifest = PackageManifest::from_path(&manifest_path)
                    .map_err(|error| anyhow::anyhow!("cannot enrich imported lockfile: {error}"))?;
                apply_manifest_root_metadata(&mut lockfile, &manifest)
                    .map_err(|error| anyhow::anyhow!("cannot enrich imported lockfile: {error}"))?;
            }
            (lockfile, diagnostics)
        } else {
            (
                bpm::alternate_lock::import(&input).map_err(|e| anyhow::anyhow!(e.to_string()))?,
                Vec::new(),
            )
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
