# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once a 1.0 release is cut.

## [Unreleased]

### Changed

- HTTP transport switched from blocking `ureq` (HTTP/1.1) to
  `reqwest::blocking` over a shared connection pool with HTTP/2 negotiated via
  TLS ALPN. The `HttpClient`/`HttpResponse` API is unchanged, but concurrent
  requests from cloned clients \u2014 notably the install/download worker pool
  \u2014 now multiplex over a single HTTP/2 stream per host instead of opening one
  HTTP/1.1 connection per request. Registry credentials are still applied only
  to the configured host and are marked sensitive so `reqwest` strips them on a
  cross-host redirect (matching browser/curl/npm behavior; previously `ureq`
  never forwarded auth on any redirect). All HTTP header field names are now
  emitted lower-cased on the wire, as HTTP requires, which is invisible to the
  npm registry.

### Added

- Persistent registry-metadata cache (`<store>/.bpm` store now also holds
  `metadata-cache.db`): packument and per-version metadata responses are
  stored durably and revalidated with `ETag` / `Last-Modified` conditional
  requests, so overlapping dependency graphs reuse metadata across runs and
  `304 Not Modified` responses are free. Resolution output stays byte-for-byte
  deterministic regardless of whether a response came from the cache or the
  network.
- npm-compatible metadata cache modes on `bpm fetch`, `bpm install`, and
  `bpm ci`: `--offline` (cache-only, error on miss), `--prefer-offline`
  (serve cached metadata without revalidation), and `--prefer-online`
  (always revalidate). The same modes are honored via `BPM_OFFLINE`,
  `BPM_PREFER_OFFLINE`, and `BPM_PREFER_ONLINE`. This directly closes the
  cold-path "persistent packument/metadata reuse" gap from M7.

## [0.1.10] - 2026-07-17

### Fixed

- Cached package images now invalidate when archive-root normalization changes,
  rebuilding scoped packages such as `@types/react` and `@types/node` with
  root-level `index.d.ts` files required by Next.js.

## [0.1.9] - 2026-07-17

### Added

- Cold benchmark samples now receive fresh per-run stores and tool caches, so
  repeated samples cannot silently become warm installs.
- Next.js projects with workspace links now receive the same project-local
  dependency view as ordinary Next.js projects.

### Fixed

- Next.js dependency checks no longer fall back to `npm install` when the
  required TypeScript, `@types`, and ESLint packages are installed through a
  workspace materialization.
- Package extraction no longer fsyncs every file individually before atomic
  publication, substantially reducing first-install filesystem overhead while
  preserving immutable temporary-image publication.


## [0.1.8] - 2026-07-17

### Added

- M7 benchmark runs now isolate npm, pnpm, Bun, Yarn, and BPM caches, making
  cold-path comparisons fair instead of reusing the developer's global stores.
  Native installs also report dependency-resolution time in JSON metrics.

### Fixed

- Native registry resolution uses abbreviated install metadata for ranges and
  tags, and exact-version endpoints for exact dependencies, avoiding full
  multi-megabyte packuments when they are unnecessary.
- npm disjunctive semver ranges such as `^3.0.0 || ^4.0.0` are now accepted by
  registry, peer, and workspace resolution.
- Scoped packages with a single `bin` declaration now use npm's unscoped
  command name (for example `@scope/tool` exposes `tool`).
- Lifecycle scripts construct PATH with the platform path-list separator.

## [0.1.7] - 2026-07-17

### Fixed

- `bpm run` now prepends `node_modules/.bin` using the platform PATH-list
  separator, so script commands such as `next` and `eslint` resolve on Unix
  and Windows just as they do through `bpm exec`.

## [0.1.6] - 2026-07-17

### Fixed

- `bpm import` now enriches imported npm lockfiles from the sibling
  `package.json`, preserving root dev/optional declarations and supported
  overrides so the generated `bpm.lock` passes `bpm ci` validation.
- Graph-volume executable entries remain relative `.bin` symlinks when package
  files are hardlinked. This preserves package-relative Node resolution for
  CLIs such as Next.js instead of resolving relative requires from
  `node_modules/.bin`.
- Next.js projects automatically receive a project-local hardlink view after
  lifecycle execution, keeping dependency realpaths inside the project for
  Turbopack while retaining graph-volume reuse. `BPM_PROJECT_VIEW=relay|local`
  can override the automatic choice.
- The benchmark harness now exercises native BPM resolution in true-cold
  scenarios instead of requiring a pre-generated npm lockfile.

## [0.1.5] - 2026-07-17

### Fixed

- Self-referencing packages now resolve correctly. A package that requires
  its own subpaths (for example `next` issuing `require('next/...')` from its
  own code) previously failed because graph-volume packages were symlinked
  into the content-addressed store, so Node's `realpath` resolved inside the
  store, which has no `node_modules`. Graph-volume packages are now
  materialized as hardlinks (real directories) whose `realpath` lands in the
  volume, where the package's siblings are reachable.
- Lifecycle scripts now run successfully. Postinstall scripts such as
  `napi-postinstall` previously failed with `Cannot find module ...` and
  discarded their output: scripts ran in a disposable sandbox holding only the
  package image, so the package's own dependencies did not resolve and any
  files a script wrote were thrown away. Scripts now execute in place against
  the package's directory inside the graph volume, where they resolve through
  the volume's complete `node_modules` tree (npm semantics), and derived
  content persists in the install.

### Changed

- Graph volumes now materialize packages as hardlinks that share inodes with
  the content-addressed store, instead of symlinks into the store. This is a
  one-time, on-disk layout change: existing volumes and compiled install plans
  are invalidated on upgrade, and the next `bpm install` rebuilds the volume by
  hardlinking from the existing store images (no re-download).
- Lifecycle scripts run against the graph volume and are isolated per-package
  from the immutable store: each package's own files are copied to independent
  inodes (with nested dependencies preserved) before its scripts run, so
  postinstall mutations stay local and can never reach a store image. Re-runs
  leave already-derived content intact, and a plan-cache hit skips lifecycle
  entirely. Installs without a graph volume (npm workspaces) retain the
  disposable-sandbox path.
- `bpm doctor` now notes that dependency `overrides` are honored during
  resolution.
- Package version bumped to `0.1.5` so `bpm --version` reports the release
  version.

## [0.1.4] - 2026-07-17

### Added

- Performant dependency installation: a shared packument cache in the
  registry client avoids re-fetching the same transitive package metadata once
  per physical placement, and the concurrent install loop now drains its
  bounded download channel after a worker error so a single fetch failure no
  longer strands workers and turns into an install hang.
- npm-compatible platform filtering via a dedicated `resolver::platform` module.
  Operating system, CPU, and libc declarations are evaluated independently
  with npm's `checkList` rule against an explicit `TargetPlatform`, so
  resolution is reproducible across machines. Optional packages that are
  incompatible with the target are skipped (and surfaced as stable diagnostics);
  required packages that cannot run on the target fail fast. A package reached
  through both optional and required paths is upgraded to required, matching
  npm's reachability semantics.
- Version-qualified dependency `overrides`: a rule such as `transitive@1.0.0`
  or `transitive@^1` no longer overrides `transitive@2`, with semver range
  intersection checking so range-qualified rules only apply to intersecting
  requests.
- `bpm install` now records the resolution target in the lockfile and skips
  platform-incompatible optional packages during materialization.
- Canonical graph id bumped to `bpm-graph-v2` so it incorporates the resolved
  target, override map, and per-package platform constraints.

### Changed

- The frozen-install guard now verifies that the lockfile's override map and
  optional/dev dependency maps match the manifest, in addition to the declared
  dependency set.
- `bpm import` reports `package-lock.json` platform constraints as enforced
  rather than recorded-only.
- Package version bumped to `0.1.4` so `bpm --version` reports the release
  version.

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

[Unreleased]: https://github.com/lbniese/bpm/compare/v0.1.10...HEAD
[0.1.10]: https://github.com/Lbniese/bpm/compare/v0.1.9...v0.1.10
[0.1.9]: https://github.com/Lbniese/bpm/compare/v0.1.8...v0.1.9
[0.1.8]: https://github.com/Lbniese/bpm/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/Lbniese/bpm/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/Lbniese/bpm/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/lbniese/bpm/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/lbniese/bpm/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/lbniese/bpm/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/lbniese/bpm/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/lbniese/bpm/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/lbniese/bpm/releases/tag/v0.1.0
