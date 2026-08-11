---
title: Architecture
---
{% include nav.html %}

# Architecture

BPM is a Rust package manager organized around an immutable artifact store,
deterministic dependency graphs, and reusable project views. This page
records the architecture that is currently shipped. For the measured
cold-vs-warm performance story with cited benchmark numbers, see
[Performance](performance.md).

## Subsystems

1. **Manifest and lockfile reader** — `src/manifest.rs`, `src/lockfile.rs`,
   and `src/npm_lock.rs` parse `package.json`, import npm `package-lock.json`
   v2/v3 packages tables, and write canonical `bpm.lock` v2 files. Imported locks are enriched
   from the sibling manifest so `bpm ci` validates dev, optional, peer, and
   override declarations. `src/npm_lock.rs` also exports npm v3 lockfiles for
   package-lock-authority projects.
2. **Native resolver** — `src/resolver/` resolves registry ranges, tags, and
   exact versions into a deterministic physical graph. Exact requests use the
   registry's version endpoint; ranges and tags use abbreviated install
   metadata, avoiding unnecessary full packument downloads. npm disjunctive
   ranges are supported. It handles supported root overrides, strict or legacy
   peer modes, npm platform filtering, optional reachability, cycles, and local
   workspaces. Non-frozen `bpm install` resolves `package.json` and writes
   `bpm.lock`; frozen installs are resolution-free.
3. **Async resolver** — `src/async_resolver.rs` is the default resolver path.
   It uses tokio and reqwest's async HTTP client to issue concurrent packument
   fetches without stalling the resolution thread. The shared placement core
   keeps output `bpm.lock` byte-identical to the blocking path; only the I/O
   model differs. Set `BPM_ASYNC_RESOLVE=0` to use the blocking kill-switch.
4. **Artifact and metadata stores** — `src/store.rs`, `src/download.rs`,
   `src/archive.rs`, and `src/integrity.rs` provide immutable, verified
   tarballs and extracted package images. `src/metadata/` records artifacts,
   images, derived objects, graphs, plans, projects, leases, and access data
   in SQLite. Publication uses temporary paths, per-object locking, and
   atomic rename.
5. **Remote artifact cache (experimental)** — `src/remote_cache.rs` provides
   an optional read-through cache keyed by SHA-512 digest. Every remote byte
   is rehashed before local atomic publication via the store. Cache misses,
   errors, and corruption fall back to the origin registry. Enabled with
   `--remote-cache HTTPS_URL` or `BPM_REMOTE_CACHE`. See
   [remote-cache-protocol.md](remote-cache-protocol.md).
6. **Graph and plan cache** — `src/graph.rs` computes canonical graph IDs,
   records platform/workspace/override/peer inputs, and stores disposable
   install plans beside the lockfile in `.bpm-state`. Dev-tree omission and
   production lifecycle mode are independent explicit install-profile facts,
   so `NODE_ENV=production --include=dev` cannot reuse normal full-tree
   lifecycle output even though its retained package records match. Plan
   validation checks graph-volume integrity and the live project view.
7. **Reusable graph volumes** — `src/volume.rs` builds a complete graph-keyed
   `node_modules` projection under `graphs/blake3/<id>/` is built in private
   staging and lifecycle-completed before atomic publication. Graph entries are
   isolated Reflink-or-Copy materializations of immutable store images; `.bin`
   entries remain relative symlinks so Node resolves bin scripts correctly.
   Projects never receive writable hardlink, symlink, junction, or relay aliases.
   Nested dependency resolution and relative `.bin` semantics remain npm-
   compatible, including for tools such as Turbopack that require project-local
   realpaths. `BPM_PROJECT_VIEW=relay|local|reflink` is accepted for
   compatibility, but every value selects a safe isolated view:
   `reflink` selects the project-local view via the copy-on-write `Reflink`
   backend, which clones each package file with macOS `clonefile(2)` / Linux
   `FICLONE` (distinct inode, shared data extents) so writes in the project
   view never reach the read-only store image — the same isolation as a full
   copy, with deep-copy fallback on unsupported filesystems. A filesystem-capability probe
   (`probe_fs_capabilities`) confirms reflink at runtime; on unsupported
   filesystems (ext4, HFS+, cross-device) the backend transparently degrades
   to an independent deep copy. Windows uses the same correctness-first
   isolated-copy fallback; no junction or hardlink exposes shared content.
   Published graph metadata stores validated references to immutable top-level
   graph entries, so cold publication and normal attachment do not rehash
   package file bodies. Stale deletion compares a project tree with its prior
   graph source on demand and preserves the entry when that source is missing
   or different.
8. **Materializer** — `src/materializer.rs` supports compatible npm v2/v3 layout
   and strict declared-edge validation. Its isolated Reflink backend falls back
   directly to independent copying; explicit Hardlink/Symlink primitives are
   not selected for graph or project publication. On Windows, safe
   archive symlinks are materialized as copied content, and `.cmd`/`.ps1` bin
   shims are generated.
9. **Platform primitives** — `src/platform.rs` provides `find_executable`,
   `script_command`, and `same_file_identity` shared by lifecycle and CLI
   execution. The platform script command produces `sh -c` on Unix and
   `cmd.exe /D /S /C` on Windows, with `COMSPEC` fallback.
10. **Lifecycle runner** — `src/lifecycle.rs` supplies npm-compatible script
   environments and `--ignore-scripts`. The production lifecycle profile
   injects `NODE_ENV=production` for dependency scripts and Git build-context
   `prepare`, including derived-artifact identity; it remains active when
   `--include=dev` restores the full tree. Arbitrary non-production
   `NODE_ENV` values are deliberately outside the bounded environment.
   Graph-volume installs execute scripts against unpublished isolated staging,
   so derived output persists only after successful lifecycle completion and
   dependencies resolve through the complete volume tree. Malformed manifests
   fail with typed path-specific errors;
   workspace/compatible installs retain the disposable sandbox. The separate
   derived-artifact store remains an explicit future optimization.
11. **CLI and measurement** — `src/cli/` exposes install, ci, import, exec,
   run, fetch, doctor, gc, audit, publish, bench, and uninstall. `bpm install`
   without `-g` and with targets performs local dependency mutation (add):
   it edits `package.json` losslessly through `src/manifest_edit.rs`, resolves
   the complete edited graph, exports the selected lock, and installs.
   `bpm remove`/`bpm uninstall` similarly strips names from all dependency
   groups, re-resolves, and reinstalls. The two-file publisher in
   `src/manifest_edit.rs` ensures pre-publication and publication errors leave
   both files restored. `src/bench.rs` records machine/tool versions, phase
   timings, cache state, and JSON results.

## Global store layout

```text
~/.bpm/
├── artifacts/sha512/<prefix>/<digest>.tgz
├── images/sha512/<prefix>/<digest>/
├── derived/blake3/<prefix>/<digest>/
├── graphs/blake3/<prefix>/<digest>/
├── plans/blake3/<prefix>/<digest>.bin
├── metadata/                         # SQLite metadata and migrations
├── locks/                            # per-object coordination
├── leases/                           # active-install/GC coordination
├── tmp/                              # unpublished temporary objects
└── store.db
```

Published objects are immutable. Integrity is checked before publication;
writers race safely without a global install lock; active leases protect data
from concurrent garbage collection; credentials are not included in cache
keys or diagnostics.

## Hashing and determinism

- **Artifact ID** — SHA-512 of the package tarball, matched against registry
  integrity when supplied.
- **Graph ID** — BLAKE3 of canonical lockfile graph fields plus target
  platform and workspace layout. Root overrides, peer mode/context, package
  sources, platform constraints, lifecycle-affecting metadata, and bin/edge
  mappings participate in the canonical bytes. The default install profile is
  byte-compatible with historical identities; every non-default dev-omission
  or production-lifecycle profile adds an explicit deterministic salt.
- **Install plan** — a versioned plan containing graph identity, materialized
  entries, bins, and lifecycle-derived paths. It is disposable; `bpm.lock` is
  authoritative.

All maps, package paths, workspace discoveries, and serialized fields are
sorted/canonicalized so output does not depend on hash-map order, filesystem
enumeration, task completion order, or network timing.

## Materialization and lifecycle invariants

1. Store images and published graphs are immutable sources; no project path
   aliases a writable shared inode.
2. Graphs are isolated and lifecycle-complete before their marker is published;
   `.bin` scripts retain package-relative symlink semantics.
3. Project attachment is an isolated Reflink-or-Copy view and records validated
   graph-entry ownership references without rereading copied file bodies;
   destructive reconciliation verifies referenced trees on demand.
4. A plan-cache hit skips resolution, fetching, extraction, and lifecycle when
   both the graph volume and project view remain valid.
5. Old volume/plan layouts are invalidated by explicit materializer/layout
   versions; stale deletion still compares the live tree with its immutable
   prior graph source before removal.
6. Dev omission is an in-memory install projection, not a lockfile rewrite:
   the complete lock remains frozen-validation authority and only retained
   records are fetched/materialized. It retains normal/optional runtime edges
   and required peer providers; optional/peer omission is intentionally
   deferred.

## Remaining architectural decisions

- Derived-artifact reuse via `src/derived/store.rs` is **explicitly deferred**.
  The active graph-volume lifecycle strategy must settle before wiring cross-graph
  derived-artifact reuse; it offers no cold-install benefit, so the path stays
  unused rather than being rushed in as a hardening item.
- Refresh same-version benchmark measurements after the isolated project-view
  change; graph reuse still avoids resolution, download, extraction, and
  lifecycle work, while safe attachment performs per-entry/file operations.
- Reflink attachment performance remains an optimization opportunity, but its
  fallback must stay an independent copy on every platform.
- ~~Default-flip the async resolver to `BPM_ASYNC_RESOLVE=1`~~ **DONE**.
  Async resolution is now the default, combines with the streaming
  install path, and retains `BPM_ASYNC_RESOLVE=0` as a blocking kill-switch.
  The blocking and async placement cores were unified into
  `src/resolver/placement.rs` with an I/O-agnostic `PackumentSource` trait
  (`src/resolver/fetch.rs`). Cold-perf improvement vs the old baseline is
  recorded in `benchmarks/baselines/reference.json`.
- Potential conditional `PUT` idempotence with `If-None-Match` remains a remote
  cache protocol refinement; best-effort upload itself is shipped.
