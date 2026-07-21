# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.1] - 2026-07-21

First public release. BPM is an npm-compatible, performance-focused package
manager that stores packages immutably in a global content-addressed store and
shares complete dependency graphs across projects, eliminating repeated
downloads, extraction, resolution, and materialization.

### Added

- **Content-addressed artifact store** (`bpm fetch`). Registry tarballs are
  downloaded, integrity-verified, and extracted once into an immutable store;
  repeated fetches perform no network or extraction work. Safe extraction
  rejects path traversal, absolute paths, and unsafe symlinks, and preserves
  the executable bit.
- **Frozen installs** (`bpm install --frozen`, `bpm ci`). Reproducible install
  from a lockfile with bounded-concurrency fetch/verify/extract, `node_modules`
  materialization, and relative `.bin` linking.
- **Lockfile import** (`bpm import`). Converts npm `package-lock.json` v3 to the
  canonical `bpm.lock` deterministically, independent of input JSON key order.
- **Native dependency-graph resolution**. Non-frozen `bpm install` resolves
  registry ranges, tags, exact versions, strict/legacy peer dependencies,
  platform constraints, overrides, optional reachability, cycles, and
  workspaces, then writes canonical `bpm.lock` metadata.
- **Graph-plan cache**. A canonical graph id (blake3 over a byte-stable
  encoding of the lockfile graph and platform) keys a compiled install plan;
  an unchanged repeated install skips resolution and materialization entirely.
- **Reusable graph volumes**. A second project that resolves the same graph
  reuses every byte of the first through shallow project relays, with a local
  hardlink compatibility view for tools (e.g. Turbopack) that reject dependency
  realpaths outside the project.
- **Lifecycle scripts** (`bpm run`). npm-compatible `preinstall`/`install`/
  `postinstall` execution with a disposable sandbox; scripts are skipped when a
  cached graph volume is reused.
- **Workspaces**. Standard `"workspaces"` glob discovery folded into the graph
  id, plus a filesystem capability probe (symlink and reflink support).
- **Cold-path performance**: persistent metadata cache with
  `ETag`/`Last-Modified` revalidation, a shared pooled HTTP client, concurrent
  registry-metadata prefetch during graph expansion, and a streaming
  resolve→download pipeline that overlaps extraction with resolution.
- **Measured benchmarks** (`bpm bench`). A harness comparing npm, pnpm, and bpm
  against identical integrity-bearing fixtures, reporting median/p95/stddev
  plus bpm's outbound request counts and per-phase timings. A checked-in
  reference baseline is included.
- **Cache modes**. `--offline`, `--prefer-offline`, and `--prefer-online` on
  `bpm fetch`, `bpm install`, and `bpm ci` (and matching `BPM_OFFLINE`,
  `BPM_PREFER_OFFLINE`, `BPM_PREFER_ONLINE`).
- **CLI surface**: `bpm doctor`, `bpm fetch`, `bpm install`, `bpm ci`,
  `bpm import`, `bpm exec`, `bpm run`, `bpm bench`, `bpm gc`, `bpm audit`, and
  `bpm publish`.
- **Cross-platform install** (`install.sh`) and pre-built release binaries for
  macOS (arm64/x86_64) and Linux (x86_64/arm64).

### Security

- Centralized URL redaction across all diagnostic paths; validation of every
  package and bin path before mutation; git-source argument hardening against
  argument injection; and integrity verification before publication.

[Unreleased]: https://github.com/lbniese/bpm/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/lbniese/bpm/releases/tag/v0.0.1
