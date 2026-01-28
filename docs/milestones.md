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
| 3 — Graph-plan cache | canonical graph hashing, compiled plan format, graph cache lookup, project state validation | ⬜ Not started |
| 4 — Reusable graph volumes | graph-volume creation, graph-volume reuse across projects, safe project attachment | ⬜ Not started |
| 5 — Lifecycle support | npm-compatible script environment, derived artifact store, native-addon fixture coverage | ⬜ Not started |
| 6 — Workspaces and optimization | basic npm workspaces, filesystem capability detection, reflink/clone optimization, adaptive concurrency | ⬜ Not started |

Note: the project's actual **Milestone 0** (benchmark harness) has not been
built yet. Earlier CLI-bootstrapping work (doctor/manifest/project-root) is
foundational scaffolding, not a substitute for it — no installer
optimization work should be evaluated without a real benchmark baseline

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
