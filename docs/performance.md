---
title: Performance
---
{% include nav.html %}

# Performance

BPM is designed around caching **complete dependency graphs** in an immutable,
content-addressed store and sharing them across projects. This page narrates
the cold-vs-warm performance story and cites the exact numbers from the
checked-in reference baseline so every claim here is reproducible from the
repository alone.

> **Do not compare these numbers across machines.** The figures below come from
> one checked-in machine/toolchain map (recorded in the baseline's `system` and
> `versions` fields). Absolute times vary by host; the **ratios** between tools
> on the same run are what matter.

## The caching thesis

Most package managers cache individual package tarballs and re-resolve the
dependency graph on every install. BPM caches three layers on top of that:

1. **Immutable artifacts** — every downloaded tarball is verified by SHA-512 and
   stored once, content-addressed. Extraction happens once per unique archive.
2. **Dependency-graph volumes** — once a project's resolved graph is materialized,
   its `node_modules` can be attached from the published graph without
   re-resolving, re-downloading, re-extracting, or rerunning lifecycle scripts.
3. **Project views** — isolated Reflink-or-Copy views attach lifecycle-complete
   graph entries to `node_modules`; unsupported CoW filesystems use deep copy.

The payoff is on the **warm path**: when the graph already exists in the store,
re-attaching `node_modules` avoids download, extraction, resolution, and
lifecycle work, while the project still receives a safe isolated view. The
cold path (first-ever install on a machine with an empty store) still pays the
network and resolution cost — that is the gap the cold-path work targets and
has not yet closed.

## What each scenario means

BPM's bench harness (`bpm bench`) runs each tool against the same fixture under
three cache states:

| Scenario | Meaning |
|---|---|
| `true_cold` | Every cache (download, extraction, graph) is empty. Full network resolution, download, and extraction. This is the worst case and the current bottleneck. |
| `resolved_cold` | A BPM lockfile is present, the store is empty, and the project view is absent. The install is frozen and resolution-free; it isolates artifact download, extraction, and graph construction. |
| `repeat_install` | The complete graph already exists in the store. `node_modules` re-attaches from cached images. This is BPM's headline win. |

Fixtures: `large-frontend` (a sizable real-world frontend dependency graph),
`many-small-files` (many small packages), `native-addon` (packages with native
build steps), `minimal` (tiny graph), and `monorepo` (workspace root).

## Current numbers from the reference baseline

The checked-in baseline (`benchmarks/baselines/reference.json`) was recorded on
**macOS 26.5, arm64** with **node v26.5.0, npm 11.12.1, pnpm 10.13.1,
bpm 0.2.1**, over `number_of_runs: 7`. Median wall-clock milliseconds, with
p95 and standard deviation quoted for the headline cells:

### `large-frontend` — the representative graph

| Scenario | npm (median) | pnpm (median) | bpm (median) | bpm p95 | bpm stddev |
|---|---:|---:|---:|---:|---:|
| `true_cold` | 13230 | 1357 | **4982** | 9329 | 1571 |
| `resolved_cold` | 4041 | 1300 | 2158 | 2211 | 77 |
| `repeat_install` | 1728 | 290 | **241** | 249 | 4 |

This is now a same-version measurement at bpm 0.2.1. On the cold `true_cold` run,
bpm is **~3.67× slower than pnpm** (4982 ms vs 1357 ms) — down from ~6.8× at
the 0.0.1 baseline — after directory-level clonefile materialization
cut the `materialization` phase to ~1500 ms (from ~3925 ms) and
`graph_volume_build` to ~101 ms (from ~1284 ms). The remaining cold gap is the
resolver: `resolver_network_wait` dominates wall-clock. On `repeat_install`, bpm
is 241 ms (vs 7 ms at 0.0.1) because project attachment now performs isolated
per-entry/file work rather than reflinking the whole `node_modules` —
a deliberate safety/isolation tradeoff that is still faster than pnpm (0.83×).

### All fixtures (median wall-clock, bpm vs pnpm)

| Fixture | Scenario | npm | pnpm | bpm | bpm/pnpm |
|---|---|---:|---:|---:|---:|
| large-frontend | repeat_install | 1728 | 290 | 241 | **0.83×** |
| large-frontend | resolved_cold | 4041 | 1300 | 2158 | 1.66× |
| large-frontend | true_cold | 13230 | 1357 | 4982 | 3.67× |
| many-small-files | repeat_install | 501 | 244 | 26 | **0.11×** |
| many-small-files | resolved_cold | 1061 | 396 | 189 | 0.48× |
| many-small-files | true_cold | 1237 | 399 | 293 | 0.73× |
| minimal | repeat_install | 972 | 241 | 26 | **0.11×** |
| monorepo | repeat_install | 1131 | 201 | 87 | 0.43× |
| monorepo | resolved_cold | 1380 | 199 | 323 | 1.62× |
| native-addon | repeat_install | 456 | 255 | 57 | **0.22×** |
| native-addon | resolved_cold | 485 | 436 | 314 | 0.72× |
| native-addon | true_cold | 888 | 436 | 1013 | 2.32× |

Reading the table: bpm is faster than pnpm on the small fixtures even when cold
(`many-small-files true_cold` 0.73×) and stays under pnpm on every
`repeat_install`/`resolved_cold` cell. The large cold graphs are where the cold
resolver still bites — `large-frontend true_cold` 3.67× and `native-addon
true_cold` 2.32×. The warm-path ratios (`repeat_install`) reflect the isolated
per-entry attachment work rather than the former whole-tree reflink,
so they are no longer near-zero but remain below pnpm.

## 0.3.0 progress (batched metadata lease)

The `metadata_lease` phase — which holds one renewable SQLite lease over every
artifact and image an install reads — previously issued one transaction per
object (2N+D serial `BEGIN IMMEDIATE` + `COMMIT`/fsync calls, each reopening the
connection). It now records all objects in a single transaction
(`record_published_objects_batch`), dropping the phase from ~192–433 ms to ~35 ms
(measured via `bpm install --json-metrics`).

The win is largest on the warm path, where `metadata_lease` is a bigger share of
total wall-clock: `large-frontend` `repeat_install` fell from ~241 ms (0.2.1
reference) to ~75 ms median over 7 runs — about **3.5× faster than pnpm** on that
cell. `resolved_cold` improved more modestly (~1554 → ~1482 ms) because
materialization dominates there. `true_cold` remains network-bound
(`resolver_network_wait` is the dominant phase); the cold resolver's observed
peak HTTP concurrency (~27) sits below its configured cap (32), so the remaining
cold gap is registry latency and graph shape, not a local concurrency ceiling.

A machine-stamped 0.3.0 snapshot is checked in at
`benchmarks/baselines/arm64-20260806.json`; the 0.2.1 `reference.json` is
preserved as the historical comparison point.

## Warm-path progress (skip redundant index work)

The plan-cache fast path (the `repeat_install` scenario) refreshes the
metadata lease on every warm install. Three sources of redundant work on
already-indexed immutable objects were identified and eliminated:

1. **Recursive size walks.** `record_published_objects_batch` called
   `logical_size` — a recursive directory walk — for every artifact, image,
   and derived object, even though the store is content-addressed and
   immutable. The batch now queries existing `(size_bytes, published_at)` from
   the SQLite index (`existing_object_records`) and reuses them for keys
   already present, skipping both the directory walk and the per-key
   `symlink_metadata` stat. Only keys new to the index touch the filesystem.

2. **Graph re-publication.** `record_graph_with_inventory` re-walked the entire
   graph volume's `node_modules` tree with `logical_size` and re-`DELETE`d +
   re-`INSERT`ed every `graph_artifacts`/`graph_derived` row on every warm
   install, even though a complete graph's membership is immutable. It now
   checks `graph_already_complete` first and returns immediately if the graph
   is already recorded complete.

3. **Cold-path download concurrency.** (See below.)

Measured on `large-frontend` `repeat_install` (7 runs, macOS arm64, node
v26.5.0, pnpm 11.14.0):

| | bpm median | bpm stddev | bpm/pnpm |
|---|---:|---:|---:|
| Before (0.3.0 batched lease) | 76.4 ms | 5.0 ms | 0.24× |
| After (skip indexed size walks) | 53.8 ms | 2.6 ms | 0.17× |
| After (skip graph re-publication) | 39.5 ms | 2.4 ms | 0.12× |
| After (fetch size+timestamp, no per-key stat) | 36.1 ms | 2.3 ms | **0.11×** |

bpm is now ~8.9× faster than pnpm on the warm path for this graph (pnpm
unchanged at ~318 ms). The improvement holds across fixtures: `minimal` 0.08×,
`many-small-files` 0.08×.

Two further cold-path changes shipped alongside:

- **Decoupled download concurrency from extract concurrency.** The
  download→extract pipeline previously shared one fs-derived worker count
  (capped at 8 on filesystems supporting atomic directory rename). Downloads
  are network-bound, so the download pool is now sized independently
  (`download_worker_count`, bounded by the resolver HTTP ceiling, default 32;
  override with `BPM_DOWNLOAD_CONCURRENCY`). On `large-frontend` `true_cold`
  this cut the bpm median from ~4259 ms to ~3985 ms (-6.4%, ratio 2.57× →
  ~2.3×). Profiling shows the download pool is not the binding constraint on
  this graph — the serial resolver placement rate feeds downloaders at ~8
  concurrent downloads regardless of pool size — so further cold gains require
  faster placement or earlier download start, not a larger pool.

- **Buffered gzip decode and widened copy buffer for extraction.** Both the
  main extraction pass and the prefix-detection scan now buffer the compressed
  stream in a 64 KiB `BufReader`, and per-file writes use a `BufWriter` plus a
  64 KiB copy buffer instead of `io::copy`'s 8 KiB default. The
  `artifact_extract` phase is consistently ~600 ms median across 14 runs, down
  from ~653 ms (~8% on that phase).

## How to reproduce

Regenerate a single cell (prepending the fresh release dir so the recorded
`bpm` version and the binary under test match):

```bash
cargo build --release
PATH="$PWD/target/release:$PATH" \
  ./target/release/bpm bench \
    --fixture large-frontend \
    --scenario true_cold \
    --runs 7 \
    --tools npm,pnpm,bpm \
    --json /tmp/bench-large-frontend-cold.json
```

Each tool runs from an isolated cache root, and bpm's timed runs also record
outbound registry **request counts** and named **phase timings** (resolve,
download, extract, …) under each tool's `bpm_metrics`, so cold-path profiling is
reproducible from the JSON alone. On pnpm 11+, the harness writes a temporary
`allowBuilds` policy for the fixture's known native build dependencies, keeping
lifecycle scripts enabled while avoiding pnpm's strict unreviewed-build exit.
See [`benchmarks/baselines/reference.json`](../benchmarks/baselines/reference.json) for the full benchmark methodology.

## Known gap: cold resolution

The cold path is the primary performance target. The
`resolved_cold` scenario isolates the artifact pipeline; `true_cold` adds
fresh dependency resolution. The resolver now uses the async path by default,
with bounded concurrent exact-version packument prefetches and in-flight
request sharing during one-level graph expansion; metadata-cache writes are
best-effort and asynchronous;
`BPM_ASYNC_RESOLVE=0` remains the blocking kill-switch. The shipped
measurements reduced the `large-frontend` `true_cold` dependency-resolution
phase from roughly 18.9 seconds to roughly 3.2–4.7 seconds; the regenerated
baseline above now records that phase at a ~2.9 s median for the same cell
(`dependency_resolution`), consistent with those post-baseline observations.

Repeated fresh resolves can reuse a validated resolution snapshot with
`--prefer-offline` or `--offline`; normal installs continue registry validation
and therefore do not change npm-compatible freshness semantics.

## Refreshing this page

Whenever `benchmarks/baselines/reference.json` is regenerated, this page must be
refreshed so every quoted number still traces to the baseline. Benchmark claims
stay tied to checked-in results and always include median/p95/stddev — do not edit the numbers here without
regenerating the baseline and recording the new machine/toolchain map.
