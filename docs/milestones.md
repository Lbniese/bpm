---
title: Milestones
---
{% include nav.html %}

# Milestones

Status against the plan in `IMPLEMENTATION.md` (local, not committed).
This page is updated as work lands; if it disagrees with the commit history,
the commit history wins.

| Milestone | Deliverables | Status |
|---|---|---|
| Foundation *(not separately numbered)* | `bpm` CLI skeleton, `package.json` parsing, project/repository root detection, `bpm doctor` diagnostics | ✅ Done |
| 0 — Benchmark harness | benchmark CLI, fixture runner, recorded toolchain versions, JSON result format, baseline results | ✅ Done — `bpm bench` |
| 1 — Artifact-store prototype | registry download, integrity verification, immutable archive storage, safe extraction, concurrent cache-safe installation | ✅ Done — `bpm fetch` |
| 2 — Package-lock frozen installer | `package-lock.json` v3 import, graph construction, basic `node_modules` materialization, bin linking | ✅ Done — `bpm import`, `bpm install --frozen` |
| 3 — Graph-plan cache | canonical graph hashing, compiled plan format, graph cache lookup, project state validation | ✅ Done — `.bpm-state` |
| 4 — Reusable graph volumes | graph-volume creation, graph-volume reuse across projects, safe project attachment | ✅ Done — `node_modules` attaches via shallow relays |
| 5 — Lifecycle support | npm-compatible script environment, derived artifact store, native-addon fixture coverage | ✅ Mostly done — sandbox runner, graph-volume lifecycle, `bpm run`; derived-store wiring remains open |
| 6 — Workspaces and optimization | basic npm workspaces, filesystem capability detection, reflink/clone optimization, adaptive concurrency | ✅ Mostly done — workspaces, capability probe, adaptive concurrency, local hardlink compatibility view; general reflink/clone attachment remains open |
| 7 — Cold-path performance | representative benchmark corpus, persistent metadata efficiency, native-resolution profiling, derived lifecycle decision | ⚠️ Partially done — realistic fixture measurements and cold resolver hardening landed; persistent packument metadata cache with ETag revalidation (Step 1A), HTTP/2 transport via reqwest (Step 1B), concurrent metadata prefetch (Phase 2), and streaming resolve→download (Phase 3) delivered. Those were **bpm-vs-prior-bpm** speedups (true_cold `large-frontend` fell from ~55s to ~26s internally), but the checked-in baseline still shows bpm ~6.8× slower than pnpm on `true_cold` `large-frontend` and ~7.7× slower on `native-addon`; the cold resolver remains the bottleneck and is not yet competitive with pnpm. Derived-artifact integration deferred by decision — it is a warm/incremental-path optimization (the derived store is empty on a cold install, so it yields zero cold-path benefit); tracked as a follow-up, not an M7 cold-path item. |

### Post-M6 — registry name resolution (not in the original plan)

`bpm fetch` now resolves an npm-style spec (`lodash`, `lodash@4.17.21`,
`lodash@^4.17.0`, scoped names) against the registry before download, matching
`npm`/`bun` UX, while exact-URL/`file://` targets keep working unchanged.
Delivered: `src/registry.rs` (packument fetch + version selection via `semver`),
`fetch` CLI `--registry` / `BPM_REGISTRY`, and offline integration tests. The
immutable store layer is unchanged — resolution produces a
`(tarball_url, integrity)` pair that the existing store consumes. Full native
graph resolution is now integrated into non-frozen `bpm install`; this section
is retained as historical context for the earlier single-package resolver.

The benchmark harness is implemented and has a checked-in reference baseline
(`benchmarks/baselines/reference.json`). Refresh it whenever the materialization
or lifecycle strategy changes, and do not compare results across different
toolchain/version maps. The current reference baseline (arm64 / macOS 26.5,
node v26.0.0, npm 11.12.1, pnpm 10.13.1, **bpm 0.1.10**) covers five fixtures:
`large-frontend`, `many-small-files`, and `native-addon` across `true_cold`,
`resolved_cold`, and `repeat_install`; plus `minimal` (`repeat_install`) and
`monorepo` (`repeat_install`, `resolved_cold`). Regenerate a single cell with
`PATH="$PWD/target/release:$PATH" ./target/release/bpm bench --fixture <f>
--scenario <s> --runs 7 --tools npm,pnpm,bpm --json <out>` (prepending the fresh
release dir so the recorded `bpm` version and the binary under test match).
Each tool runs from an isolated cache root, and bpm timed runs now also record
outbound registry **request counts** and named **phase timings** (resolve,
download, extract, …) under each tool's `bpm_metrics`, so cold-path profiling is
reproducible from the JSON alone.

What the checked-in baseline actually shows (median wall clock, bpm vs pnpm):

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

BPM's graph-volume path is the clear win on warm/repeat installs (tens of
milliseconds vs hundreds for npm/pnpm) and is already faster than pnpm on small
cold graphs. On **large cold graphs** (`large-frontend`, `native-addon` true /
resolved cold) bpm is still several times slower than pnpm — the cold resolver
is the remaining bottleneck. This is the gap the cold-path work below targets;
it is not yet closed, and the earlier "competitive" / "2.15× faster" wording
referred to bpm-vs-prior-bpm internal speedups, not to a win over pnpm.

## Native resolver — delivered

The resolver foundation described by the M2 brief is now implemented in
`src/resolver/` and wired into non-frozen `bpm install`. It resolves registry
ranges/tags/exact versions, strict or legacy peer modes, supported root
overrides, platform constraints, optional reachability, cycles, and local
workspaces, then writes canonical `bpm.lock` v2 metadata. `bpm install --frozen`
and `bpm ci` remain resolution-free and validate the manifest against the
lockfile.

## Milestone 1 — done

Success criterion: **repeated artifact fetch performs no network or
extraction work.**

Delivered: `src/download.rs`, `src/integrity.rs`, `src/archive.rs`,
`src/store.rs`, `src/metrics.rs`, `bpm fetch`. Verified by:

- `tests/store.rs` — integrity mismatch + tmp cleanup, interrupted writes,
  concurrent writers publish once, corrupt-artifact detection, read-only
  publication, atomic artifact/image reuse
- `tests/extraction.rs` — path traversal, absolute paths, unsafe/safe
  symlinks, executable-bit preservation, malformed archives, duplicate
  entries, unsupported entry types
- `tests/fetch.rs` — subprocess concurrency (single artifact published from
  N concurrent processes), repeated-fetch does no work, `BPM_TRACE`, JSON
  metrics
- a real-network smoke test against `registry.npmjs.org` (not part of CI)

## Milestone 2 — done

Success criterion: **install and run selected fixture projects.**

Phase 1 (delivered): `src/lockfile.rs` (canonical `bpm.lock`),
`src/npm_lock.rs` (npm `package-lock.json` v3 import), `bpm import`.
Verified by unit tests in both modules plus `tests/import.rs` (roundtrip
stability, determinism independent of input JSON key order, sorted output,
version/`bin` validation, link/platform-constraint diagnostics).

Phase 2 (delivered): `bpm install --frozen` (fetch + verify + extract every
locked package through the artifact store with bounded concurrency),
`node_modules` materialization, bin linking, and a runnable fixture project to
prove the success criterion end to end.

Delivered: `src/download.rs` extended with `file://`/local-path artifact
sources (offline fixtures), `src/materializer.rs` (npm-v3-compatible
`node_modules` symlinking + relative `.bin` linking), `src/main.rs` `install`
command, `Metrics::extend` for per-worker metric merging, and `npm_lock`
merging root `devDependencies` into the lockfile's declared set so the frozen
drift check covers dev deps. Verified by:

- `tests/install.rs` — offline end-to-end: top-level + nested `node_modules`
  symlinks, `.bin/<name>` relative symlink with the executable bit, second
  install fully cache-served (no new artifacts), `--frozen` refusal on
  manifest/lockfile drift, `BPM_TRACE` + `--json-metrics` phase output
- unit tests in `download.rs` (file:// digest + streaming), `materializer.rs`
  (relative bin targets, idempotent re-run, stale-symlink replace, bin
  collision keeps first, link-entry skip), and `npm_lock.rs` (devDeps merge)
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
  --all-features -- -D warnings`, `cargo test --workspace` all green (106 tests)
- a manual run that installs a `file://` tarball and executes the linked bin
  (`node_modules/.bin/hello` prints its output) — the success criterion

## Milestone 0 — done

Success criterion: **installer work is evaluated against a real benchmark
baseline, not ad-hoc timings.**

Delivered: `bpm bench` (CLI, four scenarios, fixture runner, JSON result
format). The harness runs any installed tool manager on PATH — `npm`, `pnpm`,
and `bpm` — against an identical, integrity-bearing lockfile so a scenario is
reproducible. For `bpm`, the run executes the real installer (`bpm import` +
`bpm install --frozen`). The exact toolchain versions are recorded per result so
runs are only comparable when their versions match, and `--save-baseline` writes
a machine-stamped baseline to `benchmarks/baselines/`. The harness measures bpm;
it does not rank or market tools against each other. Verified by:

- `tests/bench.rs` — offline plumbing: stats determinism independent of input
  order, `versions` map roundtrips through serialization, missing tools are
  skipped (not fatal), the available tools are advertised
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
  --all-features -- -D warnings`, `cargo test --workspace` all green (112 tests)

Note: benchmark execution needs the network (the registry), like the `bpm
fetch` real-network smoke test, so it is not part of CI by default. Generate a
baseline with `bpm bench --fixture minimal --save-baseline benchmarks/baselines`.

## Milestone 3 — done

Success criterion: **unchanged repeated install skips resolution and plan
construction.**

Delivered: `src/graph.rs` — a canonical `GraphId` (blake3 of a byte-stable
encoding of the lockfile graph + platform), a compiled `InstallPlan` (the
deterministic record of materialization operations), and plan cache lookup +
project-state validation. The installer now:

1. computes the graph id from `bpm.lock`;
2. reads `.bpm-state` and validates it against the current graph id and the
   live `node_modules` symlinks (a missing/changed symlink invalidates the
   plan);
3. on a valid cached plan, skips fetch/extract/materialize entirely
   (`plan_cache_hit`);
4. otherwise installs and writes a fresh `.bpm-state`.

Verified by:

- `src/graph.rs` tests — graph id stable across construction order, changes
  when a dependency version changes, plan roundtrips through disk, absent plan
  is a miss not an error, version/graph/state drift each invalidate correctly
- `tests/install.rs` — a repeat install emits "nothing to install" and records
  `plan_cache_hit` (not `plan_cache_miss`) in `--json-metrics`; deleting a
  materialized symlink invalidates the plan and forces a full re-install that
  restores it
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
  --all-features -- -D warnings`, `cargo test --workspace` all green (120 tests)

## Milestone 4 — done

Success criterion: **a second project with the same graph performs minimal
filesystem work.**

Delivered: `src/volume.rs` — reusable graph volumes. A graph volume is an
immutable, complete `node_modules` projection held in the store at
`graphs/blake3/<prefix>/<graph-id>/`, keyed by the graph id. Package files are
hardlinked to store images and `.bin` entries remain relative symlinks, so Node
keeps package-relative bin semantics. Building it is a one-time, graph-keyed,
idempotent operation; any project that shares the graph id reuses it.

Project attachment is shallow and safe by default: the project's `node_modules`
gets top-level relays into the volume, never a wholesale `node_modules` symlink.
Projects depending on Next.js automatically use a project-local hardlink view
because Turbopack rejects dependency realpaths outside the project; the relay
or local view can be selected with `BPM_PROJECT_VIEW`.

Verified by:

- `tests/install.rs::second_project_with_same_graph_reuses_the_volume` — a
  second project with an identical `bpm.lock` (same graph id), sharing the
  store, installs with `"graph volume reused"` (no rebuild) and a working
  `node_modules` (packages, nested dep, and bin all resolve through the volume)
- `tests/install.rs::plan_cache_invalidates_when_a_symlink_disappears` —
  deleting a project-side relay invalidates the cached plan; the next install
  re-attaches and restores it (the volume itself is untouched, since project
  paths are relays, never the durable store entry)
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
  --all-features -- -D warnings`, `cargo test --workspace` all green (121 tests)

## Milestone 5 — done

Delivered: `src/lifecycle.rs` — lifecycle script execution. Permitted scripts
(`preinstall`, `install`, `postinstall`) run with an npm-compatible environment
and `--ignore-scripts`. Graph-volume installs isolate each script-bearing
package from its immutable store image, execute in the complete volume tree,
and persist derived output in the graph volume; workspace/compatible installs
retain the disposable sandbox. A summary is printed and `bpm run <script>`
uses the same environment. `src/derived/store.rs` implements the longer-term
content-addressed derived-artifact model but is not yet the active graph
lifecycle backend.

## Milestone 6 — done

Delivered: `src/workspace.rs` — npm workspace discovery. The standard
`"workspaces"` field (array of globs or `{ "packages": [...] }`) is parsed;
glob patterns expand deterministically (sorted), and only dirs containing a
`package.json` qualify. The workspace layout is folded into the **graph id**
via `graph_id_for_project`, so a workspace-tree change invalidates the cached
plan and volume. A filesystem-capability probe (`probe_fs_capabilities` →
symlink + reflink support) is included for future materialization optimization.
Verified by unit tests in `src/workspace.rs` (discovery, empty-layout,
canonical-bytes stability/mutation, capability probe).

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
  --all-features -- -D warnings`, `cargo test --workspace` all green (126 tests)

## Milestone 7 — in progress

The first M7 measurements cover `large-frontend`, `many-small-files`,
`native-addon`, and `monorepo` across cold, warm, repeat, graph-reuse, and
incremental scenarios. They confirm BPM's graph-volume path is already highly
competitive after the graph exists, while cold native resolution and first-time
artifact extraction are the current bottlenecks. The checked-in reference
baseline now includes the representative `large-frontend` scenarios alongside
the synthetic pairs (see above); the remaining representative fixtures
(`many-small-files`, `native-addon`, `monorepo` at full scenario coverage) are
follow-ups. Future comparisons must retain toolchain versions.

Cold-path hardening now:

- requests npm's abbreviated install metadata for range/tag resolution;
- fetches exact versions from the registry's version endpoint instead of the
  full package history;
- accepts npm disjunctive semver ranges such as `^3.0.0 || ^4.0.0`;
- records native dependency-resolution time in `--json-metrics`;
- isolates every cold benchmark sample with fresh per-tool caches and stores;
- avoids per-file extraction fsyncs before atomic image publication;
- uses project-local hardlink views for Next.js workspace installs, with an
  end-to-end `bpm install` → `bpm exec next build` regression;
- invalidates cached package images and graph volumes after archive-root layout
  changes, covering scoped `@types` packages used by Next.js.

### Derived-artifact integration — deferred by decision

`src/derived/` (`DerivedStore` + `derived_key`) implements a content-addressed
cache for lifecycle-derived images and is unit-tested in isolation, but is not
wired into the lifecycle path. M7 deferred it explicitly as a *decision*:
content-addressed vs graph-keyed. **Decision: defer.** Rationale:

1. **It is not a cold-path optimization.** M7 is the cold-path milestone. On a
   true cold install the derived store is empty, so it provides zero cold-path
   benefit — every package's lifecycle runs regardless. Its value is on the
   *incremental/warm* path: reusing a package's derived output when the graph
   changes but that package's build inputs did not.
2. **The cold path is now resolve-bound, not build-bound.** After Phases 1–3,
   `true_cold` `large-frontend` is ~26s (median; see the checked-in baseline) and lifecycle is a small fraction of
   it; there is no cold-path headroom left for the derived store to capture.
3. **The integration has a real design gap to close first.**
   `DerivedStore::ensure` runs its build callback against a staging copy of the
   package's *source image* (its own files), but lifecycle scripts need their
   dependency subtree present (`node_modules`) to resolve. The current
   graph-volume path supplies deps via the materialized volume tree; the derived
   path would have to re-materialize the package's dep subtree into staging
   (duplicating materialization) or source from the volume directory (which
   would wrongly fold deps into the derived image and key). Solvable, but its
   own design effort — not a wiring task.
4. **Correctness sensitivity.** Lifecycle output drives native addons and
   generated code; a cache key that is too coarse yields stale or wrong output.
   The store already validates published trees by digest, but pinning the key
   inputs precisely (target + runtime + source-tree identity + script + env,
   excluding everything that should not invalidate) needs dedicated coverage
   and is higher-risk than the M7 performance work.

The current graph-volume path already persists derived output in the volume and
reuses it for repeat installs of the *same* graph (`ensure_graph_volume` returns
`cached` when graph id + layout match). **Lifecycle is now skipped when the
volume is reused** — a second project attaching to an existing graph volume no
longer re-runs `preinstall`/`install`/`postinstall`; the install path scans each
package's manifest to record which volume entries are derived copies (so the
plan still validates) and skips execution, observable as
`lifecycle_skipped_cached_volume` in `--json-metrics`. The derived store's
additional value — reuse across *changed* graphs where a package's inputs are
unchanged — is a genuine warm-path win worth a dedicated milestone (likely an
M5 lifecycle follow-on).

### Concurrent metadata prefetch — delivered (Phase 2)

Registry packument fetches during dependency-graph expansion now overlap.
When the resolver places a node, it submits best-effort prefetches for that
node's registry-typed children to a small background worker pool that shares
the HTTP/2 client, so sibling packuments are already in flight (or cached)
by the time depth-first placement reaches them. The placement algorithm and
its ordering are unchanged, so `bpm.lock` stays byte-for-byte identical with
prefetch on or off (covered by `prefetch_does_not_change_the_resolved_lockfile`),
and `InFlight` cache slots deduplicate a prefetch and the synchronous fetch to
one request. Default worker count is capped low (4); `BPM_PREFETCH_WORKERS`
overrides or disables it.

Prefetch only affects the **fresh-resolve** path (`true_cold`: no lockfile).
The lockfile-present path (`resolved_cold`) reads the lockfile and skips
resolution entirely, so prefetch is a no-op there. Measured on `true_cold`
`large-frontend` (where resolution actually runs): prefetch-disabled ≈ 50.8s
median vs prefetch-enabled ≈ 25.5s median — roughly a **2× speedup** of the
resolve phase.

### Streaming resolve → download — delivered (Phase 3)

A fresh install now downloads each package the instant the resolver places it,
overlapping the download/extract pipeline with the rest of graph resolution.
The resolver gained an optional `ResolveSink`
(`resolve_manifest_with_options_sink`) that emits each resolved registry-typed
node `(path, url, integrity)` as it is placed; the install pipeline consumes the
stream over a bounded channel (natural backpressure). Determinism is unchanged:
the sink only *observes* placement, so `bpm.lock` is byte-for-byte identical to
a sequential resolve
(`streaming_sink_emits_every_downloadable_node_and_keeps_the_lockfile_identical`),
and downloads are integrity-keyed and idempotent. `BPM_STREAM_INSTALL=0` falls
back to resolve-then-download for benchmarking or regression isolation.

Measured on `true_cold` `large-frontend`, same-binary A/B (6 runs each):
streaming disabled ≈ 26.9s median vs streaming enabled ≈ 23.5s median — about
**12% faster** (≈ 3.4s saved), min agreeing at ≈ 12%. The benefit is bounded by
how much of the install is download vs resolution: once downloads are fully
hidden behind resolution, the install becomes resolve-bound.

### HTTP/2 transport — delivered (Step 1B)

The blocking `ureq` (HTTP/1.1) transport was replaced with `reqwest::blocking`
over a shared connection pool, negotiating HTTP/2 over TLS via ALPN. The
`HttpClient`/`HttpResponse` surface is unchanged, so the resolver, registry,
download, publish, and audit call sites are untouched. Because the install /
download worker pool shares one pooled client, concurrent tarball fetches now
multiplex over a single HTTP/2 stream per host instead of opening one HTTP/1.1
connection per request, reducing TLS handshake and connection overhead on the
cold path. Retry semantics (transient-status and connect/timeout backoff,
bounded error-body draining, `Retry-After`) are preserved.

### Persistent metadata cache — delivered

Packument and per-version metadata responses are now cached durably in
`<store>/metadata-cache.db` and revalidated with `ETag` / `Last-Modified`
conditional requests (`If-None-Match` / `If-Modified-Since`). A `304 Not
Modified` reuses the stored body verbatim, so resolution is deterministic
whether served from cache or network. Delivered: `src/metadata_cache.rs`
(`MetadataCache`, `CacheMode`), `RegistryClient::with_metadata_cache`, and
npm-compatible `--offline` / `--prefer-offline` / `--prefer-online` flags on
`bpm fetch`, `bpm install`, and `bpm ci` (plus `BPM_OFFLINE` /
`BPM_PREFER_OFFLINE` / `BPM_PREFER_ONLINE`). The cache is best-effort for
online modes and fails the install only in `--offline` mode on a genuine miss.

### Async resolver A/B — no default-flip (2026-07-20)

The async resolver (`BPM_ASYNC_RESOLVE=1`, `src/async_resolver.rs`) was
benchmarked against the blocking resolver on `true_cold` `large-frontend` and
`true_cold` `native-addon` (7 runs each). Results: `large-frontend` showed
1.05× speedup (median 24,992 → 23,873 ms), below the 1.10× bar;
`native-addon` showed 1.23× speedup (6,768 → 5,514 ms), meeting the bar.
The default remains blocking (`BPM_ASYNC_RESOLVE=1` opt-in) because both
fixtures must meet the 10% minimum. See commit `6cb572b` for the full evidence table. The async resolver stays available for continued
measurement and refinement.
