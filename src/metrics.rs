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

/// Concurrency/throughput counters used to tune bounded-stage concurrency
/// (IMPLEMENTATION §20 / S6.1.1). All counters are cumulative for the run
/// except the depth/overlap "high water marks", which track the maximum
/// value observed rather than a running total, since depth and overlap are
/// instantaneous quantities rather than counts of discrete events.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counters {
    /// Highest number of queued-but-not-yet-started work items observed at
    /// any point during the run (a queue "high water mark").
    pub max_queue_depth: u64,
    /// Highest number of concurrently in-flight work items observed at any
    /// point during the run (an overlap "high water mark").
    pub max_concurrent_overlap: u64,
    /// Total bytes transferred (e.g. downloaded artifact/tarball bytes).
    pub bytes_transferred: u64,
    /// Total store/artifact cache hits.
    pub cache_hits: u64,
    /// Total store/artifact cache misses.
    pub cache_misses: u64,
    /// Total outbound network requests issued.
    pub requests_sent: u64,
    // ── Resolver/prefetch diagnostics ──────────────────────────────────
    /// Packument cache hits (resolver found a Ready entry, no fetch needed).
    pub resolver_cache_hits: u64,
    /// Packument cache waits (resolver blocked on an in-flight prefetch).
    pub resolver_cache_waits: u64,
    /// Inline packument fetches (resolver fetched itself, no prefetch hit).
    pub resolver_inline_fetches: u64,
    /// Background prefetch fetches (prefetch pool fetched on behalf of resolver).
    pub prefetch_fetches: u64,
    /// Total bytes of packument bodies fetched over the network.
    pub packument_bytes: u64,
    /// Nanoseconds the resolver thread spent blocked on network during depscan.
    pub resolver_network_wait_ns: u64,
    /// Packuments fetched during the batch-prefetch closure phase (before DFS).
    pub batch_prefetch_fetches: u64,
}

impl Counters {
    /// Fold `other` into `self`: cumulative counters add; high-water-mark
    /// counters take the larger of the two. Used when merging per-worker
    /// counters back into the command's main metrics.
    pub fn merge(&mut self, other: &Counters) {
        self.max_queue_depth = self.max_queue_depth.max(other.max_queue_depth);
        self.max_concurrent_overlap = self
            .max_concurrent_overlap
            .max(other.max_concurrent_overlap);
        self.bytes_transferred += other.bytes_transferred;
        self.cache_hits += other.cache_hits;
        self.cache_misses += other.cache_misses;
        self.requests_sent += other.requests_sent;
        self.resolver_cache_hits += other.resolver_cache_hits;
        self.resolver_cache_waits += other.resolver_cache_waits;
        self.resolver_inline_fetches += other.resolver_inline_fetches;
        self.prefetch_fetches += other.prefetch_fetches;
        self.packument_bytes += other.packument_bytes;
        self.resolver_network_wait_ns += other.resolver_network_wait_ns;
        self.batch_prefetch_fetches += other.batch_prefetch_fetches;
    }
}

/// Collection of named phase timings for a single command run.
#[derive(Default)]
pub struct Metrics {
    phases: Vec<(&'static str, Duration)>,
    counters: Counters,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a raw duration under `name`.
    pub fn record(&mut self, name: &'static str, dur: Duration) {
        self.phases.push((name, dur));
    }

    /// Observe a queue-depth sample, updating the high-water mark if `depth`
    /// exceeds the current maximum.
    pub fn observe_queue_depth(&mut self, depth: u64) {
        self.counters.max_queue_depth = self.counters.max_queue_depth.max(depth);
    }

    /// Observe a concurrent-overlap sample, updating the high-water mark if
    /// `overlap` exceeds the current maximum.
    pub fn observe_concurrent_overlap(&mut self, overlap: u64) {
        self.counters.max_concurrent_overlap = self.counters.max_concurrent_overlap.max(overlap);
    }

    /// Add `bytes` to the cumulative transferred-bytes counter.
    pub fn add_bytes_transferred(&mut self, bytes: u64) {
        self.counters.bytes_transferred += bytes;
    }

    /// Increment the cache-hit counter.
    pub fn record_cache_hit(&mut self) {
        self.counters.cache_hits += 1;
    }

    /// Increment the cache-miss counter.
    pub fn record_cache_miss(&mut self) {
        self.counters.cache_misses += 1;
    }

    /// Increment the outbound-request counter.
    pub fn record_request(&mut self) {
        self.counters.requests_sent += 1;
    }

    /// Record resolver/prefetch diagnostic counters from a fresh resolution.
    pub fn record_resolver_diagnostics(
        &mut self,
        cache_hits: u64,
        cache_waits: u64,
        inline_fetches: u64,
        prefetch_fetches: u64,
        packument_bytes: u64,
        network_wait_ns: u64,
    ) {
        self.counters.resolver_cache_hits += cache_hits;
        self.counters.resolver_cache_waits += cache_waits;
        self.counters.resolver_inline_fetches += inline_fetches;
        self.counters.prefetch_fetches += prefetch_fetches;
        self.counters.packument_bytes += packument_bytes;
        self.counters.resolver_network_wait_ns += network_wait_ns;
    }

    /// Record batch-prefetch closure counters (separate from inline per-node
    /// prefetches because the batch phase runs before DFS traversal begins).
    pub fn record_batch_prefetch(&mut self, batch_fetches: u64) {
        self.counters.batch_prefetch_fetches += batch_fetches;
    }

    /// Add `n` to the outbound-request counter. Used to fold a shared HTTP
    /// client's cumulative request count into the command metrics once, after
    /// the request-issuing work is complete.
    pub fn add_requests(&mut self, n: u64) {
        self.counters.requests_sent += n;
    }

    /// Total outbound requests recorded so far.
    pub fn requests_sent(&self) -> u64 {
        self.counters.requests_sent
    }

    /// Snapshot of the current counters.
    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// Append another metric set's phases and counters into this one
    /// (additive for phases and cumulative counters; high-water marks take
    /// the max). Used to merge per-worker timings back into the command's
    /// main metrics after a bounded-concurrency phase completes.
    pub fn extend(&mut self, other: &Metrics) {
        self.phases.extend(other.phases.iter().cloned());
        self.counters.merge(&other.counters);
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

    /// Canonical, deterministic JSON: `{"phases": {name: elapsed_ms, ...},
    /// "total_ms": n, "counters": {max_queue_depth, max_concurrent_overlap,
    /// bytes_transferred, cache_hits, cache_misses, requests_sent}}`.
    pub fn to_json(&self) -> String {
        let mut by_name: BTreeMap<String, f64> = BTreeMap::new();
        let mut total = 0.0f64;
        for (name, dur) in &self.phases {
            let ms = dur.as_secs_f64() * 1000.0;
            *by_name.entry((*name).to_string()).or_insert(0.0) += ms;
            total += ms;
        }
        let mut counters = serde_json::Map::new();
        counters.insert(
            "max_queue_depth".into(),
            serde_json::Value::from(self.counters.max_queue_depth),
        );
        counters.insert(
            "max_concurrent_overlap".into(),
            serde_json::Value::from(self.counters.max_concurrent_overlap),
        );
        counters.insert(
            "bytes_transferred".into(),
            serde_json::Value::from(self.counters.bytes_transferred),
        );
        counters.insert(
            "cache_hits".into(),
            serde_json::Value::from(self.counters.cache_hits),
        );
        counters.insert(
            "cache_misses".into(),
            serde_json::Value::from(self.counters.cache_misses),
        );
        counters.insert(
            "requests_sent".into(),
            serde_json::Value::from(self.counters.requests_sent),
        );
        counters.insert(
            "resolver_cache_hits".into(),
            serde_json::Value::from(self.counters.resolver_cache_hits),
        );
        counters.insert(
            "resolver_cache_waits".into(),
            serde_json::Value::from(self.counters.resolver_cache_waits),
        );
        counters.insert(
            "resolver_inline_fetches".into(),
            serde_json::Value::from(self.counters.resolver_inline_fetches),
        );
        counters.insert(
            "prefetch_fetches".into(),
            serde_json::Value::from(self.counters.prefetch_fetches),
        );
        counters.insert(
            "packument_bytes".into(),
            serde_json::Value::from(self.counters.packument_bytes),
        );
        counters.insert(
            "resolver_network_wait_ms".into(),
            serde_json::Value::from((self.counters.resolver_network_wait_ns as f64) / 1_000_000.0),
        );
        counters.insert(
            "batch_prefetch_fetches".into(),
            serde_json::Value::from(self.counters.batch_prefetch_fetches),
        );
        let mut root = serde_json::Map::new();
        root.insert(
            "phases".to_string(),
            serde_json::to_value(&by_name).expect("phase metrics serialize"),
        );
        root.insert("total_ms".into(), serde_json::Value::from(total));
        root.insert("counters".into(), serde_json::Value::Object(counters));
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

    #[test]
    fn counters_track_high_water_marks_and_cumulative_totals() {
        let mut m = Metrics::new();
        m.observe_queue_depth(3);
        m.observe_queue_depth(7);
        m.observe_queue_depth(2);
        m.observe_concurrent_overlap(4);
        m.observe_concurrent_overlap(1);
        m.add_bytes_transferred(100);
        m.add_bytes_transferred(50);
        m.record_cache_hit();
        m.record_cache_hit();
        m.record_cache_miss();
        m.record_request();
        m.record_request();
        m.record_request();

        let c = m.counters();
        assert_eq!(c.max_queue_depth, 7);
        assert_eq!(c.max_concurrent_overlap, 4);
        assert_eq!(c.bytes_transferred, 150);
        assert_eq!(c.cache_hits, 2);
        assert_eq!(c.cache_misses, 1);
        assert_eq!(c.requests_sent, 3);
    }

    #[test]
    fn extend_merges_counters_with_max_for_high_water_marks() {
        let mut a = Metrics::new();
        a.observe_queue_depth(2);
        a.observe_concurrent_overlap(5);
        a.add_bytes_transferred(10);
        a.record_cache_hit();
        a.record_request();

        let mut b = Metrics::new();
        b.observe_queue_depth(9);
        b.observe_concurrent_overlap(1);
        b.add_bytes_transferred(20);
        b.record_cache_miss();
        b.record_request();

        a.extend(&b);
        let c = a.counters();
        assert_eq!(c.max_queue_depth, 9);
        assert_eq!(c.max_concurrent_overlap, 5);
        assert_eq!(c.bytes_transferred, 30);
        assert_eq!(c.cache_hits, 1);
        assert_eq!(c.cache_misses, 1);
        assert_eq!(c.requests_sent, 2);
    }

    #[test]
    fn json_includes_counters_object() {
        let mut m = Metrics::new();
        m.observe_queue_depth(5);
        m.add_bytes_transferred(1024);
        m.record_cache_hit();
        m.record_request();
        let j = m.to_json();
        assert!(j.contains("\"counters\""));
        assert!(j.contains("\"max_queue_depth\""));
        assert!(j.contains("\"bytes_transferred\""));
        assert!(j.contains("\"cache_hits\""));
        assert!(j.contains("\"requests_sent\""));
    }
}
