use std::path::PathBuf;

use anyhow::{Context, Result};
use bpm::gc::policy::{parse_byte_size, parse_duration, GcPolicy, DEFAULT_GRACE};
use bpm::metadata::MetadataRepository;

pub(crate) fn run(
    older_than: Option<String>,
    max_size: Option<String>,
    store: Option<PathBuf>,
) -> Result<()> {
    let grace = older_than
        .as_deref()
        .map(parse_duration)
        .transpose()
        .map_err(|error| anyhow::anyhow!(error))?
        .unwrap_or(DEFAULT_GRACE);
    let max_size_bytes = max_size
        .as_deref()
        .map(parse_byte_size)
        .transpose()
        .map_err(|error| anyhow::anyhow!(error))?;
    let root = store.unwrap_or_else(|| {
        std::env::var_os("BPM_STORE")
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs_home().join(".bpm"))
    });
    let repository = MetadataRepository::open(&root)
        .with_context(|| format!("open store {}", root.display()))?;
    let report = repository.collect(GcPolicy {
        grace,
        max_size_bytes,
    })?;
    let evaluation = report
        .evaluation
        .as_ref()
        .expect("collector always evaluates");
    println!(
        "reclaimed {} object(s), {} bytes; {} object(s) selected",
        report.deleted,
        report.deleted_bytes,
        evaluation.selected.len()
    );
    if !evaluation.cap_reachable {
        eprintln!(
            "warning: max-size cannot be reached without deleting protected or recent objects"
        );
    }
    Ok(())
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
