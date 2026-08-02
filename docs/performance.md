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
**macOS 26.5, arm64** with **node v26.0.0, npm 11.12.1, pnpm 10.13.1,
bpm 0.0.1**, over `number_of_runs: 7`. Median wall-clock milliseconds, with
p95 and standard deviation quoted for the headline cells:

### `large-frontend` — the representative graph

| Scenario | npm (median) | pnpm (median) | bpm (median) | bpm p95 | bpm stddev |
|---|---:|---:|---:|---:|---:|
| `true_cold` | 11552 | 3819 | **25824** | 28853 | 1472 |
| `resolved_cold` | 4180 | 4406 | 7350 | — | — |
| `repeat_install` | 670 | 313 | **7** | 10 | 1.3 |

These are historical 0.0.1 measurements, not a current measurement of the
isolated project-view implementation. The post-Plan-026 path still avoids
network, extraction, resolution, and lifecycle work when reusing a graph, but
project attachment performs isolated per-entry/file work. A same-version
benchmark is required before claiming a current repeat-install time or speedup.
On the historical `true_cold` run, bpm was **~6.8× slower than pnpm** (25824 ms
vs 3819 ms): the cold resolver was the remaining bottleneck.

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

Reading the table: these historical 0.0.1 results describe the former
materialization behavior. They remain useful baseline evidence, but must not be
generalized to current warm-install timings after the isolation change. The
historical large cold graphs were several times slower than pnpm; refreshing the
baseline is follow-up measurement rather than a shipped performance claim.

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

## Known gap: cold resolution

The cold path is the primary performance target. The
`resolved_cold` scenario isolates the artifact pipeline; `true_cold` adds
fresh dependency resolution. The resolver now uses the async path by default,
with bounded concurrent exact-version packument prefetches and in-flight
request sharing during one-level graph expansion; metadata-cache writes are
best-effort and asynchronous;
`BPM_ASYNC_RESOLVE=0` remains the blocking kill-switch. Plan 036's shipped
measurements reduced the `large-frontend` `true_cold` dependency-resolution
phase from roughly 18.9 seconds to roughly 3.2–4.7 seconds. The checked-in
benchmark table above remains the historical baseline and has not been
regenerated, so these newer measurements are post-baseline observations rather
than replacements for its numbers.

Repeated fresh resolves can reuse a validated resolution snapshot with
`--prefer-offline` or `--offline`; normal installs continue registry validation
and therefore do not change npm-compatible freshness semantics.

## Refreshing this page

Whenever `benchmarks/baselines/reference.json` is regenerated, this page must be
refreshed so every quoted number still traces to the baseline. Benchmark claims
stay tied to checked-in results
and always include median/p95/stddev — do not edit the numbers here without
regenerating the baseline and recording the new machine/toolchain map.
