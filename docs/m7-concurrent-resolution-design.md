# M7 Phase 2/3 — Concurrent resolution (design / scope)

Status: **Phase 2 delivered** (concurrent metadata prefetch); Phase 3
(streaming resolve→download→extract) remains designed but not yet
implemented. This document was the implementation spec; the Phase 2 section
below is now historical/record, and Phase 3 is the outstanding scope.

Phase 1 (Step 1A persistent metadata cache + Step 1B `reqwest` HTTP/2 transport)
is landed in commit `90b7f2d`. Phase 2/3 is what turns that foundation into real
resolution speedups by overlapping registry metadata fetches (and, optionally,
tarball downloads) during graph expansion.

## Goal and non-goals

**Goal.** Reduce cold-path install wall time by overlapping the network I/O
inside dependency-graph resolution, which today runs one blocking round-trip per
package.

**Non-goals.**

- Do not change the resolved graph (byte-identical `bpm.lock` for the same
  inputs, regardless of thread count or fetch completion order).
- Do not change the public resolver API surface (`resolve_manifest_*` →
  `Lockfile`) in a way that affects the one production caller.
- Do not convert the codebase to async. Everything stays synchronous on
  `reqwest::blocking` + `std::thread`.

## Current architecture (why resolution is the bottleneck)

Install (`src/cli/install.rs::run`) runs in three phases:

1. **Resolve** (`install.rs:78`) — `resolver::resolve_manifest_with_options`
   fully resolves the manifest into a `Lockfile` and **blocks until the entire
   graph is done**.
2. **Download + Extract** (`install.rs:139–251`) — already concurrent: an
   `std::thread::scope` runs `workers` downloader threads feeding a bounded
   `sync_channel` that feeds `extraction_workers` extractor threads. This phase
   already benefits from the HTTP/2 multiplexed pool landed in Step 1B.
3. **Materialize** (`install.rs:278`) — link/extract into the project view.

The resolver (`src/resolver/mod.rs`) is **recursive depth-first**:
`GraphResolver::resolve_dependency` (line 373) fetches a packument, selects a
version, inserts a node, then recurses into each child inside a `for` loop over
`dependencies` (a `BTreeMap`, so deterministic order) — see the child loops at
lines 441–447 (workspace) and 558–566 (registry). The root loop at
`mod.rs:200–205` also processes root deps sequentially. So the whole graph is
traversed one blocking round-trip at a time. That is the target.

The registry round-trip goes `resolve_dependency` →
`RegistryClient::packument_for` (`registry.rs:386`) → `fetch_with_cache`
(`registry.rs:626`), which already consults an in-memory
`packument_cache: Arc<Mutex<BTreeMap<String, Packument>>>` (`registry.rs:297`)
and the persistent `MetadataCache` (Step 1A) before hitting the network.

## Core insight: fetch ≠ placement

Resolution has two separable concerns:

- **Fetch** — "get the packument for package X". Pure I/O, independent across
  packages, and the expensive part.
- **Placement** — "given packuments, pick versions, dedupe against visible
  ancestors, handle peer/override/placement rules, insert the node". Must be
  deterministic and depends on already-placed ancestors/siblings.

Placement is inherently ordered and stateful. Fetch is not. Phase 2 overlaps
fetches while leaving placement exactly as it is today. This is what keeps the
output byte-identical.

## Determinism guarantee (the hard constraint)

The output `bpm.lock` must be byte-for-byte identical regardless of how many
threads fetch concurrently or in what order they complete. This holds today and
must keep holding because:

- All traversal collections are `BTreeMap`/`BTreeSet` (ordered by key, not by
  insertion/completion time).
- `resolve_dependency` reads packuments by *spec*, not by *timing* — the
  packument content is a pure function of the spec, so a prefetched and a
  synchronously-fetched result are identical.
- `lock.sort_packages()` (`mod.rs:357`) canonicalizes final ordering.
- The bounded peer backtracking uses `versions.sort()` (`mod.rs:516`).

Rule for any Phase 2/3 change: **concurrency is allowed in fetch; placement must
remain a deterministic sequential read of the (now warmer) cache.** A regression
test must assert byte-identical lockfiles across concurrency levels.

## Phase 2 — concurrent metadata fetching (prefetch)

Lowest-risk, highest-value step. Do this first.

### Design

Add a best-effort **prefetch pool** that shares the existing pooled
`HttpClient` (already `Clone` and Arc-interned, so worker threads multiplex over
one HTTP/2 connection per host — exactly what Step 1B enabled).

- New module `src/registry/prefetch.rs` (or a field on `RegistryClient`): a
  fixed-size worker pool (`std::thread::scope`-friendly, or long-lived worker
  threads fed by a `crossbeam`/`std::sync::mpsc` channel) that runs
  `packument_for(spec)` and stores the result.
- New method `RegistryClient::prefetch(spec: &PackageSpec)`: non-blocking
  submit. Idempotent — no-op if the spec is already cached or in-flight.
- Make the in-memory `packument_cache` the synchronization point by extending
  its entry to one of `Ready(Packument)` / `InFlight`. A synchronous
  `packument_for` that misses does the fetch itself; one that hits an `InFlight`
  entry blocks on a condvar until `Ready`. This dedupes the depth-first miss
  race (main thread reaching a spec before its prefetch finishes) without double
  fetches. Dedup is an efficiency optimization, not a correctness requirement
  (duplicate fetches write identical data).
- Prefetch populates **both** the in-memory cache and the persistent
  `MetadataCache`, so prefetched results survive across runs (Step 1A
  compounds).

### Trigger points

In `resolve_dependency`, **after a node is inserted and before its children
loop**, the children set is known. Submit `prefetch(child_spec)` for each child:

- Registry path: between `mod.rs:551` (node insert) and the `mod.rs:558–566`
  child loop. Build the child `PackageSpec` the same way
  `registry_request`/`parse_spec` already do.
- Workspace path: between `mod.rs:436` (insert) and the `mod.rs:441–447` loop.

The root loop (`mod.rs:200–205`) can additionally prefetch all root deps up
front so the first wave of fetches overlaps immediately.

### What does not change

- Placement order, the recursion structure, overrides, peer logic, and the
  lockfile assembly. `packument_for`'s return value is unchanged.
- The one production caller (`install.rs:78`) is untouched.

### Risks / open decisions

- **Cache in-flight state** adds a little locking complexity; if deemed
  not-worth-it, ship dedup-less first (duplicates are correct, just wasteful)
  and add dedup if benchmarks show duplicate fetches.
- **Pool lifecycle**: prefer scoped workers tied to the resolve call over a
  global pool, so a failed/aborted resolution tears workers down cleanly.
- **Bounded queue**: cap the prefetch backlog so a huge fan-out graph does not
  queue tens of thousands of fetches; drop-prefetch-on-overflow is safe because
  `packument_for` self-heals on miss.
- **Error handling**: prefetch failures must not surface as resolution errors on
  their own; the synchronous `packument_for` path is the single source of truth
  for error reporting (it will re-fetch and return the real error).

### Expected win

Overlaps the latency of sibling/subtree packument fetches. Largest benefit on
wide, shallow graphs and cold (network-bound) installs. Negligible on hot
(store-only) installs, where Step 1A already wins.

## Phase 3 — streaming resolve → download → extract

Larger, higher-risk. Decide after measuring Phase 2.

### Design

Overlap **download** with **resolution** (today they are strictly sequential:
resolve fully, then download). Because the store is content-addressed
(integrity-keyed), a package can be downloaded as soon as *that node* is
resolved, before the rest of the graph is known.

- New resolver entry point `resolve_manifest_with_sink(manifest, registry, …,
  sink)` that pushes each resolved node `(name, tarball_url, integrity, source)`
  to a channel the moment it is placed (around the inserts at `mod.rs:436` /
  `mod.rs:551`), while still building and returning the complete `Lockfile`.
- The install download pipeline consumes the sink instead of waiting for the
  full `Lockfile` + `build_install_work` (`install.rs:705`) to complete. The
  existing downloader/extractor worker scope (`install.rs:141–251`) is reused;
  only its input source changes from "fully-built work list" to "stream".
- Finalization (peer post-pass at `mod.rs:244+`, lockfile write, materialize,
  frozen validation) still waits for resolution to finish — streaming only
  overlaps the *download* of early packages with the *resolution* of later ones.

### Risks / open decisions

- **Error/cancellation**: if resolution fails mid-graph, some downloads may have
  started. Need a cancel signal to the download scope and store-cleanup of
  partial downloads (store is content-addressed, so leftover partials are
  harmless and GC-able, but should be torn down).
- **API shape**: a sink-based variant is a new public function; keep the existing
  `resolve_manifest_with_options` intact for `bpm fetch`, lock generation, and
  non-install callers.
- **Backpressure**: a bounded sink prevents resolution from running arbitrarily
  far ahead of download and consuming unbounded memory.
- **Determinism**: still byte-identical — the lockfile is assembled the same way;
  downloads are idempotent and integrity-keyed.

### Expected win

Only matters when resolution and download are both non-trivial in duration
(medium/large cold installs). On hot installs or when download dominates,
negligible. Measure before committing to this phase.

## Validation plan

1. **Determinism regression test**: resolve the same non-trivial fixture graph
   N times with prefetch disabled vs enabled (and, for Phase 3, streaming on vs
   off) and assert byte-identical `bpm.lock` bytes. Reuse the existing
   `resolves_transitive_registry_graph_deterministically` test
   (`mod.rs:1499`) as the baseline.
2. **Offline suite**: must stay 100% green (no behavior change).
3. **Live-network benchmark**: `resolved_cold` / `true_cold` on `large-frontend`
   and `many-small-files`, bpm-only, before vs after, median/p95. Compare
   against the Phase 1 reference (bpm already 4.77s vs npm 7.61s on
   large-frontend cold at the Phase 1 checkpoint).
4. **Clippy + fmt clean**; no new direct deps unless justified.

## Sequencing and recommendation

1. **Phase 2 (prefetch)** first — self-contained, preserves the public API and
   determinism by construction, reuses the Step 1B pool. Estimated ~1–2 days.
2. **Measure.** If cold installs become download-bound rather than
   resolve-bound, stop. If resolution still dominates, proceed.
3. **Phase 3 (streaming)** only if measurement justifies it — larger surface,
   cancellation/backpressure work, ~3–5 days.

Recommended first concrete task (Phase 2): add the prefetch pool + in-flight
cache dedup behind a `RegistryClient::prefetch` method with a feature flag or
concurrency setting (default on, off for `--offline`/tests that assert exact
fetch counts), wire the two trigger points in `resolve_dependency`, and add the
determinism regression test.
