//! Structured phase metrics (IMPLEMENTATION §20).
//!
//! Records named-phase durations and renders them deterministically. JSON
//! output sorts phases by name (independent of insertion order, locale, or map
//! iteration order) so metrics are reproducible. Distinct phases may repeat in
//! time (e.g. multiple downloads); totals are summed.
//!
//! Surfaced to users via:
//! - `BPM_TRACE=1` prints a CSV trace to stderr
//! - `--json-metrics <file>` writes canonical JSON

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::time::{Duration, Instant};

/// Collection of named phase timings for a single command run.
#[derive(Default)]
pub struct Metrics {
    phases: Vec<(&'static str, Duration)>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a raw duration under `name`.
    pub fn record(&mut self, name: &'static str, dur: Duration) {
        self.phases.push((name, dur));
    }

    /// Append another metric set's phases into this one (additive). Used to
    /// merge per-worker timings back into the command's main metrics after a
    /// bounded-concurrency phase completes.
    pub fn extend(&mut self, other: &Metrics) {
        self.phases.extend(other.phases.iter().cloned());
    }

    /// Run `f`, recording the elapsed time under `name`, and return its result
    /// (which may itself be a `Result`). Records the phase on both success and
    /// failure so partial metrics are still inspectable.
    pub fn measure<R>(&mut self, name: &'static str, f: impl FnOnce() -> R) -> R {
        let start = Instant::now();
        let r = f();
        self.phases.push((name, start.elapsed()));
        r
    }

    /// Whether any phase was actually executed (non-empty run).
    pub fn has_phases(&self) -> bool {
        !self.phases.is_empty()
    }

    /// Canonical, deterministic JSON: `{"phases": {name: elapsed_ms, ...}, "total_ms": n}`.
    pub fn to_json(&self) -> String {
        let mut by_name: BTreeMap<String, f64> = BTreeMap::new();
        let mut total = 0.0f64;
        for (name, dur) in &self.phases {
            let ms = dur.as_secs_f64() * 1000.0;
            *by_name.entry((*name).to_string()).or_insert(0.0) += ms;
            total += ms;
        }
        let mut root = serde_json::Map::new();
        root.insert(
            "phases".to_string(),
            serde_json::to_value(&by_name).expect("phase metrics serialize"),
        );
        root.insert("total_ms".into(), serde_json::Value::from(total));
        serde_json::to_string_pretty(&root).expect("metrics serialize")
    }

    /// Write a CSV trace (`name,elapsed_ms` per line) to `w`.
    pub fn print_trace(&self, w: &mut impl Write) -> io::Result<()> {
        writeln!(w, "phase,elapsed_ms")?;
        for (name, dur) in &self.phases {
            writeln!(w, "{name},{}", dur.as_secs_f64() * 1000.0)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_deterministic_across_insertion_order() {
        let mut a = Metrics::new();
        a.record("integrity_verify", Duration::from_micros(500));
        a.record("artifact_download", Duration::from_millis(2));

        let mut b = Metrics::new();
        b.record("artifact_download", Duration::from_millis(2));
        b.record("integrity_verify", Duration::from_micros(500));

        assert_eq!(a.to_json(), b.to_json());
    }

    #[test]
    fn json_references_phase_names() {
        let mut m = Metrics::new();
        m.record("artifact_download", Duration::from_millis(1));
        let j = m.to_json();
        assert!(j.contains("\"artifact_download\""));
        assert!(j.contains("\"total_ms\""));
    }
}
