# bpm perf-loop harness and methodology

This directory holds the deterministic measurement infrastructure for bpm
performance work, plus notes on the methodology that makes the perf loop
converge instead of wander.

## Why this exists

The real-registry cold-install benchmark has a noise floor (σ ≈ 530ms) larger
than the gap being closed (~424ms vs bun). At that noise level, no optimization
of plausible size can be proven — every change reads as "no improvement" within
the noise. This is the failure mode that kills perf loops regardless of which
model drives them. The harness below cuts the noise floor ~28× so real wins
are visible.

## The harness: `.perf/replay/`

A standalone Rust binary (`bpm-replay`) that replays a captured registry trace
from localhost with fixed deterministic latency. Three subcommands:

- **`capture --db T.db --port 8790`** — reverse proxy to `registry.npmjs.org`
  that records every response body (packuments + tarballs) keyed by URL path.
  Run once per fixture. (Has known keep-alive robustness issues on large traces;
  prefer `import` below.)
- **`import --db T.db --from <store>/metadata-cache.db --store <store>`** —
  builds a trace from bpm's own metadata cache (packuments) and artifact store
  (tarballs). This is the reliable path: run one real `bpm install` to warm the
  cache, then import. Tarball `dist.tarball` URLs are rewritten on serve so bpm
  fetches everything from localhost.
- **`serve --db T.db --latency-ms 20 --port 8791`** — serves the trace with a
  fixed per-response latency (default 20ms). Concurrent (async, latency is
  overlapped not queued), so bpm's real overlap logic runs. Gzip-encodes plain
  bodies on the fly so bpm's decompression path runs as in production.

Build: `cd .perf/replay && cargo build --release`.

## The loop (what makes it converge)

```
0. BASELINE: capture real numbers, `import` a trace, `serve` it.
   ┌────────────────────────────────────────────────────────────┐
   │ 1. PROFILE: where does time actually go? (phase_ms + trace)│
   │ 2. HYPOTHESIS: "X is slow because Y; change Z saves ~Nms." │
   │    Falsifiable. State the expected magnitude.              │
   │ 3. SMALLEST change that tests it.                          │
   │ 4. RE-MEASURE: 10 runs change vs 10 runs baseline, same    │
   │    harness. Compare means; compute significance (σ).       │
   │ 5. DECIDE on data:                                         │
   │    >=2σ improvement, correct, no regressions → KEEP, commit│
   │    <2σ or regression → REVERT, move on.                    │
   │    Never polish a dead end.                                │
   └────────────────────────────────────────────────────────────┘
   repeat, re-profiling each time (the bottleneck moves)
```

The one rule that changes everything: **a change that doesn't beat 2σ gets
reverted within minutes, not hours.** Instrument before hypothesizing; the
serial spine trace (`BPM_TRACE_INLINE=1`) finds the actual blocking chain.

## Results so far

Measured on large-frontend / true_cold (`--ignore-scripts`, fair-footing vs
bun). Replay numbers are wall-clock (python timer) at 20ms fixed latency.

### 2026-08-18 session: warm no-op install (repeat_install)

Phase metrics on a fully-warm repeat install showed every instrumented phase
at ~0ms — the whole wall time was uninstrumented. Step-timer profiling of the
plan-cache-hit path (`[T]` eprintln, since removed) found the metadata refresh
cascade: `acquire_lease` 63ms (300+ lease rows), `plan_validate` 19ms,
`replace_project_graph_ref` 15ms, `record_published_objects_batch` 13ms,
`record_graph` 8ms, `write_durable_registration` 9ms (fsync), `record_access`
8ms — ~75% of the warm path, all to re-assert GC protection that the
protection rules already keep alive without it (project-referenced complete
graphs and their inventories are protected regardless of access age).

**`perf(install): skip redundant metadata refresh on warm cache hits`**
- repeat_install (large-frontend, 15 paired runs vs bun): bpm 59.6ms →
  21.2ms; paired median ratio vs bun 1.90 → 0.42 (bpm now ~2.4× faster).
  stddev 25.2ms → 2.1ms.
- Change: `MetadataRepository::cached_graph_protection_current` (one SQLite
  read: graph complete + project ref + durable registration file match)
  gates `refresh_cached_graph`; the volume's durable inventory must also be
  current (legacy/missing inventory still forces a rebuild — two integration
  tests pin that).
- 37 test suites green; clippy/fmt clean.

**`perf(install): lazy HTTP and registry clients on the plan-cache-hit path`**
- repeat_install: bpm 21.2ms → 9.0ms (bun 18.0ms) — 2× faster than bun,
  stddev 0.6ms. http/registry construction moved after the cache-hit early
  return; also added permanent `plan_validate` + `ownership_refresh` phase
  metrics (warm-path profiling now needs no throwaway timers).

**`perf(install): skip redundant ownership writes on volume-reuse installs`**
- warm_store 100.8 → 57.7ms (bun 45.4); second_project_same_graph 92.9 →
  83.1ms (bun 65.3); partial_dependency_change 100.1 → 57.0ms (bun 45.4);
  monorepo_incremental 94.4 → 24.6ms (bun 14.5).
- Same protection predicate extended to `finalize_project`, plus a
  graph-only object lease on the volume-reuse path (the volume's hardlinks
  keep artifact inodes alive; only the volume itself is read), gated by the
  new public `volume::graph_volume_reuse_expected` marker precheck.

### Where the remaining warm gaps live (structural, not waste)

- bpm's npm-compatible nested layout materializes ~7006 files for
  large-frontend where bun's layout does ~3146 — ~2.2× the clone work.
  Directory-level `clonefile` is already in use (9 syscalls; per-file
  `cp -c` of the same trees takes 1.3s), so attach is at the APFS floor;
  closing the last warm_store/second_project gap means layout changes
  (pnpm-style isolated store + symlinks), a design decision, not a patch.
- second_project additionally pays required first-registration durability
  writes (registration file fsync + ref replace) — correct by design.
- Startup floor (`bpm --version`) is ~10ms; monorepo_incremental's 10ms gap
  on a 3-package graph is mostly that floor. Worth a look at static-init
  cost in a future session.
- Cold path (true_cold 1919ms vs bun 1495ms, monorepo_cold) untouched this
  session; the replay harness is the tool.

### Wins landed

**`8985330 perf(resolver): prefetch one transitive level during DFS`**
- `dependency_resolution`: 1620ms → 1134ms (−486ms, 26σ on replay)
- total wall (real registry): 1919ms → 1837ms (−82ms)
- requests unchanged (69); lockfile byte-identical; all tests pass.
- Found by tracing the serial spine (13-fetch chain: `@babel/compat-data →
  browserslist → caniuse-lite → electron-to-chromium`), then targeting it with
  DFS lookahead depth 0→1 gated to metadata-only (the artifact-hint gate was
  learned from iteration 1's failure).

### Falsified fast (each killed in ≤10 min by the harness)

| Iter | Hypothesis | Result |
| --- | --- | --- |
| 1 | Deeper root lookahead (metadata + artifacts) | wall 2.3× worse — artifact flood. Reverted. |
| 2 | Metadata-only root lookahead 2→3 | 0.38σ — within noise. Reverted. |
| 4 | DFS lookahead depth 2 | +27ms, 1.5σ — diminishing returns. Reverted. |
| — | Extract concurrency 8→16 | no improvement; extract isn't worker-bound. |

### Current phase balance (after iter-3, replay wall ≈ 751ms)

- pipeline_wall ~460ms (download+extract, largest critical-path chunk)
- materialization+project_attachment ~315ms (fs-bound linking)
- dependency_resolution ~280ms (overlaps pipeline; mostly optimized)
- startup ~35ms

No single dominant lever remains; phases are balanced. Next sessions should
re-profile and target the largest current phase.

## Known side-bugs (separate from perf, not addressed here)

- `monorepo` fixture: `bpm install` at workspace root resolves **0 packages**
  (workspace deps not picked up). Affects the fixture's usability for
  cross-tool comparison.
- `many-small-files` fixture: smoke check requires a `generate` step that isn't
  part of install, so external tools fail smoke validation.
- `batch_prefetch_fetches` reads 0 in metrics on the async streaming path
  (folded into `prefetch_fetches`); the prefetcher works, the number is
  mis-attributed.

## Reproducing

```sh
# Build bpm and the harness.
cargo build --release
(cd .perf/replay && cargo build --release)

# Warm the metadata cache + artifact store with one real install.
rm -rf /tmp/warm/{project,store,home} && mkdir -p /tmp/warm/{project,store,home}
cp -r fixtures/large-frontend/* /tmp/warm/project/
(cd /tmp/warm/project && HOME=/tmp/warm/home \
  ./target/release/bpm install --ignore-scripts --store /tmp/warm/store)

# Import a trace (packuments + tarballs) from that warm cache.
.perf/replay/target/release/bpm-replay import \
  --db .perf/trace-large-frontend.db \
  --from /tmp/warm/store/metadata-cache.db \
  --store /tmp/warm/store

# Serve and benchmark deterministically.
.perf/replay/target/release/bpm-replay serve \
  --db .perf/trace-large-frontend.db --latency-ms 20 --port 8791 &
rm -rf /tmp/p/{project,store,home} && mkdir -p /tmp/p/{project,store,home}
cp -r fixtures/large-frontend/* /tmp/p/project/
(cd /tmp/p/project && HOME=/tmp/p/home \
  ./target/release/bpm install --ignore-scripts \
  --registry http://127.0.0.1:8791/ --store /tmp/p/store \
  --json-metrics /tmp/p/m.json)
```
