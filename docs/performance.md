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
   its `node_modules` can be re-attached from the store via hardlinks/reflinks
   without re-resolving or re-downloading.
3. **Project views** — hardlink (default) and reflink (copy-on-write) views
   attach store images into `node_modules` without copying file bodies.

The payoff is on the **warm path**: when the graph already exists in the store,
re-attaching `node_modules` is a few hardlink/reflink operations instead of a
full download+extract+resolve. The cold path (first-ever install on a machine
with an empty store) still pays the network and resolution cost — that is the
gap the cold-path work targets and has not yet closed.

## What each scenario means

BPM's bench harness (`bpm bench`) runs each tool against the same fixture under
three cache states:

| Scenario | Meaning |
|---|---|
| `true_cold` | Every cache (download, extraction, graph) is empty. Full network resolution, download, and extraction. This is the worst case and the current bottleneck. |
| `resolved_cold` | The immutable artifact store is warm (artifacts already downloaded/extracted), but the resolved graph for *this project* is not yet materialized. Resolution still runs; download/extraction are served from the store. |
| `repeat_install` | The complete graph already exists in the store. `node_modules` re-attaches from cached images. This is BPM's headline win. |

Fixtures: `large-frontend` (a sizable real-world frontend dependency graph),
`many-small-files` (many small packages), `native-addon` (packages with native
build steps), `minimal` (tiny graph), and `monorepo` (workspace root).

## Current numbers from the reference baseline

The checked-in baseline (`benchmarks/baselines/reference.json`) was recorded on
**macOS 26.5, arm64** with **node v26.0.0, npm 11.12.1, pnpm 10.13.1,
bpm 0.0.1**, over `number_of_runs: 7`. Median wall-clock milliseconds, with
p95 and standard deviation quoted for the headline cells:

### `large-frontend` — the representative graph

| Scenario | npm (median) | pnpm (median) | bpm (median) | bpm p95 | bpm stddev |
|---|---:|---:|---:|---:|---:|
| `true_cold` | 11552 | 3819 | **25824** | 28853 | 1472 |
| `resolved_cold` | 4180 | 4406 | 7350 | — | — |
| `repeat_install` | 670 | 313 | **7** | 10 | 1.3 |

The contrast is stark: on `repeat_install` bpm attaches `node_modules` in
**~7 ms** versus npm's ~670 ms and pnpm's ~313 ms — the graph-volume path is a
two-orders-of-magnitude win once the graph exists. On `true_cold`, bpm is
**~6.8× slower than pnpm** (25824 ms vs 3819 ms): the cold resolver is the
remaining bottleneck.

### All fixtures (median wall-clock, bpm vs pnpm)

| Fixture | Scenario | npm | pnpm | bpm | bpm/pnpm |
|---|---|---:|---:|---:|---:|
| large-frontend | repeat_install | 670 | 313 | 7 | **0.02×** |
| large-frontend | resolved_cold | 4180 | 4406 | 7350 | 1.67× |
| large-frontend | true_cold | 11552 | 3819 | 25824 | 6.76× |
| many-small-files | repeat_install | 512 | 275 | 6 | **0.02×** |
| many-small-files | resolved_cold | 518 | 430 | 160 | 0.37× |
| many-small-files | true_cold | 540 | 443 | 198 | 0.45× |
| minimal | repeat_install | 532 | 281 | 6 | **0.02×** |
| monorepo | repeat_install | 545 | 239 | 11 | 0.05× |
| monorepo | resolved_cold | 526 | 230 | 325 | 1.41× |
| native-addon | repeat_install | 520 | 285 | 7 | **0.02×** |
| native-addon | resolved_cold | 549 | 493 | 662 | 1.34× |
| native-addon | true_cold | 955 | 507 | 3894 | 7.68× |

Reading the table: on **warm/repeat** installs bpm is one to two orders of
magnitude faster than pnpm (tens of ms vs hundreds). On **small cold graphs**
(`many-small-files`) bpm already beats pnpm. On **large cold graphs**
(`large-frontend`, `native-addon` true/resolved cold) bpm is still several times
slower than pnpm — that is the gap the cold-path work targets.

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
reproducible from the JSON alone. See  → M7 for the
full benchmark methodology.

## Known gap: cold resolution

The cold path is the primary outstanding performance bottleneck. The structural
prerequisite — unifying the blocking and async resolvers behind a single
I/O-agnostic placement core — is done (`src/resolver/placement.rs`), and async
resolution is now default-on (`BPM_ASYNC_RESOLVE=0` is the kill-switch). Closing
the actual network fan-out gap (fetching sibling dependency packuments
concurrently during graph expansion) is tracked as follow-up work; see
[`plans/`](https://github.com/lbniese/bpm/tree/main/plans) for the current
cold-resolver plan.

## Refreshing this page

Whenever `benchmarks/baselines/reference.json` is regenerated, this page must be
refreshed so every quoted number still traces to the baseline. Benchmark claims
stay tied to checked-in results
and always include median/p95/stddev — do not edit the numbers here without
regenerating the baseline and recording the new machine/toolchain map.
