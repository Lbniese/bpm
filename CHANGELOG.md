# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once a 1.0 release is cut.

## [Unreleased]

No unreleased changes.

## [0.1.3] - 2026-06-28

### Added

- Deterministic dependency resolution with peer, platform, workspace, and
  override handling, plus npm-compatible metadata and registry configuration.
- Persistent metadata, garbage collection, derived artifacts, compatible
  materialization, alternate lockfile import, and expanded install workflows.
- CLI workflows for fetch, install, import, exec, run, gc, doctor, benchmark,
  publish, and audit, with network and resolver regression coverage.
- Benchmark fixtures and profiles covering cold, warm, monorepo, lifecycle,
  native-addon, and incremental scenarios.

## [0.1.2] - 2026-06-23

### Added

- `bpm install <pkg>` now links the package's binaries globally (bin-linking
  for single-package installs), matching the behavior already available for
  lockfile-driven installs.

## [0.1.1] - 2026-06-18

### Added

- `bpm fetch` (and the underlying `src/registry.rs`) now resolves npm-style
  package specs — bare names (`lodash`), exact versions (`lodash@4.17.21`),
  semver ranges (`lodash@^4.17.0`), and scoped names (`@scope/pkg`) — against
  the npm registry using semver-based version selection, matching npm/bun
  UX. Exact tarball URLs, `file://` targets, and bare local paths keep their
  existing behavior; the immutable artifact store is unchanged.
- Standalone `site/index.html` landing page, with a hardened `pages.yml`
  workflow that skips deployment when `site/` is absent.

### Fixed

- `bpm fetch`/registry resolution no longer fails with a
  `RelativeUrlWithoutBase` error when resolving npm package names.

## [0.1.0] - 2026-06-03

Initial release.

### Added

- `bpm` CLI skeleton, `package.json` parsing, project/repository-root
  detection, and `bpm doctor` diagnostics.
- Immutable, content-addressed artifact store: download, SHA-512
  verification, safe extraction, and concurrent cache-safe installs.
- `package-lock.json` v3 import and the frozen installer, including
  `node_modules` materialization and bin linking.
- Canonical, blake3-based graph IDs computed over the lockfile graph and
  target platform.
- Compiled install-plan cache with project-state validation, so an
  unchanged repeat install skips resolution and materialization.
- Reusable graph volumes shared across projects with the same graph id, via
  shallow, safe project attachment that never exposes the store as
  writable.
- npm-compatible lifecycle script execution in isolated sandboxes — scripts
  can never mutate the immutable store or graph volume.
- `bpm run`, with an npm-compatible environment.
- Basic npm workspaces support, with deterministic glob discovery folded
  into the graph id.
- Install-timing benchmark harness with machine-stamped baselines.

[Unreleased]: https://github.com/lbniese/bpm/compare/v0.1.3...HEAD
[0.1.3]: https://github.com/lbniese/bpm/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/lbniese/bpm/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/lbniese/bpm/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/lbniese/bpm/releases/tag/v0.1.0
