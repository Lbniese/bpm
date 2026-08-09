# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Performance

- **Skip redundant index work on the warm path.** The plan-cache fast path
  refreshes the metadata lease on every `repeat_install`. Three sources of
  redundant work on already-indexed immutable objects were eliminated: the
  batched lease reuses stored `(size_bytes, published_at)` from the SQLite
  index instead of re-walking directories with `logical_size` and re-statting
  every file; graph re-publication is skipped entirely for graphs already
  recorded complete (avoiding a full `node_modules` tree walk and redundant
  edge-row rewrites). On `large-frontend` `repeat_install` (7 runs) this cut
  bpm from 76.4 ms to 36.1 ms (-53%), making bpm ~8.9× faster than pnpm on
  that cell (0.11× ratio). stddev also dropped (5.0 → 2.3 ms).

- **Decoupled download concurrency from extract concurrency.** The
  download→extract pipeline previously shared one fs-derived worker count
  (capped at 8 where atomic directory rename is supported). Downloads are
  network-bound, so the download pool is now sized independently
  (`download_worker_count`, bounded by the resolver HTTP ceiling; override with
  `BPM_DOWNLOAD_CONCURRENCY`). On `large-frontend` `true_cold` this cut the bpm
  median from ~4259 ms to ~3985 ms (-6.4%).

- **Batched metadata lease.** The install lease phase now records every artifact,
  image, and derived object in a single SQLite transaction
  (`record_published_objects_batch`) instead of one transaction per object. This
  replaces 2N+D serial `BEGIN IMMEDIATE` + `COMMIT`/fsync round-trips (each
  reopening the connection) with one connection and one fsync, dropping the
  `metadata_lease` phase from ~192–433 ms to ~35 ms. The warm-path headline
  `large-frontend` `repeat_install` fell from ~241 ms (0.2.1) to ~75 ms median —
  about 3.5× faster than pnpm on that cell. Resulting rows are byte-for-byte
  identical to the per-key path; access timestamps now use a single caller-supplied
  `now` for monotonicity.

- **Buffered gzip decode and widened copy buffer for extraction.** Both
  extraction passes now buffer the compressed stream in a 64 KiB `BufReader`,
  and per-file writes use a `BufWriter` plus a 64 KiB copy buffer. The
  `artifact_extract` phase is consistently ~600 ms median, down from ~653 ms
  (~8% on that phase).

### Changed

- **Serial graph-volume materialization.** The graph-volume staging build now
  materializes Pass A on a single thread
  (`materialize_with_backend_serial`). Graph volumes contain nested
  ancestor/descendant package paths that race under parallel
  `create_dir_all` / `clonefile_directory` (EEXIST on the ancestor target); a
  measured parallel build with EEXIST-retries was both slower (macOS `clonefile`
  is kernel-serialized) and higher-variance than serial. The staging tree is
  private and atomically renamed on publish, so Pass-A order does not affect the
  published `node_modules` byte-identity.

## [0.3.0] - 2026-08-03

npm-compatible developer and registry-management commands, dev-only install
profiles, and further cold-path performance on top of 0.2.1.

### Added

- **npm-compatible developer commands**. `bpm init` scaffolds a `package.json`;
  `bpm ls` prints the installed dependency tree; `bpm view` shows registry
  package metadata; `bpm cache` inspects and reclaims the local cache; and
  `bpm whoami` prints the registry-authenticated user.
- **npm-compatible registry-management commands**. `bpm token` lists, creates,
  and revokes registry tokens; `bpm dist-tag` lists, adds, and removes
  distribution tags; and `bpm owner` lists, adds, and removes package owners.
- **npm-compatible developer linking**. `bpm link` performs the global
  two-step developer link.
- **Dev-only install profiles**. `bpm install` and `bpm ci` accept repeatable
  typed `--omit=dev` and `--include=dev` flags (include wins regardless of
  order), with `NODE_ENV=production` as the default omit trigger. Filtering is
  an in-memory runtime projection only, so `bpm.lock` and direct npm
  `package-lock.json` authority stay complete and byte-unchanged; omitted
  installs get separate graph/volume/plan identities and reconcile safely
  with the full tree.
- **npm `package-lock.json` lockfileVersion 2 and 3 import** through the shared
  packages-table importer for import, install/CI, audit, and npm-authority
  mutations; mutations and exports canonicalize npm authority to strict
  lockfileVersion 3.

### Security

- Git clone and fetch failures redact credential-bearing remote forms before
  returning subprocess diagnostics.
- HTTP GET redirects are re-authorized per hop, while POST, PUT, and DELETE
  redirects are refused without replaying credentials or request bodies.

### Fixed

- Upgrade and dedupe publish lockfiles through synchronized atomic replacement.
- Malformed lifecycle package manifests now fail with path-specific typed errors.
- Graph publication completes lifecycle work before exposing a reusable volume;
  graph and project views no longer use writable aliases to shared content.
- Named upgrades preserve unselected dependency closures and return a true
  no-op for unknown-only selections.
- Graph ownership metadata is validated as complete and rebuilt when malformed,
  and aliased project views are rejected.
- Copy and reflink-fallback materialization now preserve source modification
  times (npm-compatible), so repeat-install reuse stays stable on filesystems
  without copy-on-write.

### Changed

- Project ownership reuses published graph-entry identities instead of
  rehashing copied file bodies during normal attachment; live-tree hashing is
  retained for conservative stale-view deletion.
- **Directory clonefile materialization**. Whole package trees are
  cloned in a single `clonefile` syscall on macOS — the major cold-install
  materialization win.
- **Parallel graph-package fingerprinting**. Per-package
  `tree_fingerprint` runs in parallel during publish, a modest (~21%)
  identity-build improvement.

## [0.2.1] - 2026-07-28

Supersedes 0.2.0: the release signing keypair was rotated after the 0.2.0
private key could not be recovered. No functional or code changes; the
shipped binaries are identical to 0.2.0.

### Changed

- Rotated `.github/release-signing-public.pem`; published `SHA256SUMS` is now
  signed with the replacement key.

## [0.2.0] - 2026-07-28

Cold-path performance, mutation and diagnostic commands, and remote-cache
push on top of 0.1.0.

### Added

- **Mutation and diagnostic commands**. `bpm upgrade` and `bpm dedupe` rewrite
  lock state within declared ranges without editing `package.json` ranges;
  `bpm why` traces why a package is in the graph, and `bpm outdated` reports
  available updates (registry dist-tags are queried concurrently).
- **Remote-cache push**. A best-effort `PUT` uploads verified raw `.tgz`
  artifacts keyed by SHA-512 so other machines can install without
  re-downloading.
- **Persistent resolution snapshot cache**. Successful fresh resolves store
  a snapshot keyed by manifest, workspace, registry configuration, peer
  mode, and target platform; `--prefer-offline`/`--offline` installs reuse
  it after validating the cached lockfile. The store path is process-safe
  (PID-namespaced temp), and aged snapshots are pruned during GC.
- **Derived metadata store**. The derived package store is wired into the
  SQLite repository so computed metadata persists across runs.
- **git-prepare on by default**, and reflink auto-selection for local
  project views.

### Changed

- **HTTP/2 by default and overlapping downloads**. The transport
  multiplexes artifact bodies over HTTP/2 (`BPM_HTTP2=0` opts out), the
  fetch/extract receiver mutex is held only across `recv` so downloaders
  overlap, and async registry concurrency is bounded by a semaphore
  (`BPM_RESOLVER_MAX_IN_FLIGHT`, default 32).
- **Faster cold resolution**. Prefetches now overlap on a multi-threaded
  runtime, exact-version packument fetches are singleflighted and cached
  under a version-scoped key, and packument bodies are held in an
  in-memory LRU; packument fan-out and dist-tag queries run concurrently.
- **Leaner cold publication**. Image publication writes a metadata-only
  index (`BPMIDX01`) instead of duplicating file payloads, the
  materializer batches directory creation, and npm-shaped archives skip a
  redundant pre-extraction scan.
- **Streaming remote-cache push** bodies instead of double-buffering.

### Fixed

- **npm v3 lockfile import** skips workspace metadata entries recorded
  under project-relative paths.
- **`.npmignore`/`.gitignore`** glob patterns are honored during publish.
- **Security audits** use the bulk advisory endpoint and deduplicate
  counts by advisory id.
- **Derived script environments** are bounded.

## [0.1.0] - 2026-07-25

Reliability, correctness, and performance improvements on top of the initial public release.

### Added

- **Full frozen-lock correctness**. Frozen installs now fail fast on changed
  dependency specifications (not just missing/added names), and the diff shown by
  `bpm install --frozen` / `bpm ci` includes explicit spec drift details.
- **Hardened async resolver metadata pipeline**. Async metadata fetch now supports
  correct `200`/`304` caching semantics, shared retry behavior, bounded overflow
  handling, and end-to-end request persistence across the streaming install path.
- **Production-safe cache ownership**. Installed graph state now persists durable
  ownership and lock-based leases so GC and rebuild operations remain correct under
  concurrent operations and process crashes.
- **Race-safe source caches**. Git/patch source caches now publish deterministically
  under advisory locking to avoid concurrent cache corruption.
- **Leaner materialization validation**. Package-image metadata can be decoded from
  `.bpi` sidecars without reading file payloads, reducing disk pressure during
  install.
- **Tooling readiness**. Release assets keep using deterministic artifact checksums,
  and installer/CI paths include stronger verification and regression controls for
  release integrity.

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
- **Reusable graph volumes** (0.0.1 historical behavior). A second project that
  resolved the same graph reused every byte of the first through shallow
  project relays, with a local hardlink compatibility view for tools (e.g.
  Turbopack) that rejected dependency realpaths outside the project.
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

[Unreleased]: https://github.com/lbniese/bpm/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/lbniese/bpm/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/lbniese/bpm/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/lbniese/bpm/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/lbniese/bpm/releases/tag/v0.1.0
[0.0.1]: https://github.com/lbniese/bpm/releases/tag/v0.0.1
