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

The benchmark harness is implemented and has a checked-in reference baseline.
Refresh it whenever the materialization or lifecycle strategy changes, and do
not compare results across different toolchain/version maps.

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
