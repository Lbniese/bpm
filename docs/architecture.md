---
title: Architecture
---
{% include nav.html %}

# Architecture

BPM is a Rust package manager organized around an immutable artifact store,
deterministic dependency graphs, and reusable project views. The detailed
product direction remains in [`IMPLEMENTATION.md`](../IMPLEMENTATION.md); this
page records the architecture that is currently shipped.

## Subsystems

1. **Manifest and lockfile reader** — `src/manifest.rs`, `src/lockfile.rs`,
   and `src/npm_lock.rs` parse `package.json`, import npm `package-lock.json`
   v3, and write canonical `bpm.lock` v2 files. Imported locks are enriched
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
3. **Async resolver (experimental)** — `src/async_resolver.rs` is a drop-in
   async counterpart of the native resolver. It uses tokio and reqwest's async
   HTTP client to issue concurrent packument fetches without stalling the
   resolution thread. The output `bpm.lock` is byte-identical to the blocking
   path; only the I/O model differs. Enabled with `BPM_ASYNC_RESOLVE=1`. Still
   experimental; the blocking resolver remains the default.
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
   install plans beside the lockfile in `.bpm-state`. Plan validation checks
   graph-volume integrity and the live project view.
7. **Reusable graph volumes** — `src/volume.rs` builds a complete graph-keyed
   `node_modules` projection under `graphs/blake3/<id>/`. Package files are
   hardlinked to immutable store images, while `.bin` entries remain relative
   symlinks so Node resolves bin scripts from their package directory. Ordinary
   projects use shallow top-level relays for the O(top-level) fast path.
   Projects depending on `next` automatically receive a project-local hardlink
   view so tools such as Turbopack and Next.js do not reject dependency
   realpaths outside the project; the auto-detection set defaults to `next` and
   is extended via `BPM_LOCAL_VIEW_PACKAGES` (comma-separated package names,
   merged with the built-in default so Next.js installs never regress).
   Workspace-linked installs use the same hardlink backend for registry
   packages. `BPM_PROJECT_VIEW=relay|local|reflink` overrides that choice
   (`reflink` selects the local view via the CoW reflink backend, which falls
   back to hardlink then copy when the filesystem lacks reflink support). A
   `Reflink` materialize backend variant and a filesystem-capability probe
   (`probe_fs_capabilities`) are available; the actual `clonefile`/`FICLONE`
   syscall wiring is not yet linked (the crate keeps a minimal dependency set),
   so `Reflink` currently degrades to the hardlink→copy chain and `Auto`
   selection is unchanged. Windows uses a correctness-first local hardlink/copy
   view; junctions and reflink/clone performance remain deferred.
8. **Materializer** — `src/materializer.rs` supports compatible npm-v3 layout
   and strict declared-edge validation. It has symlink, hardlink, and fallback
   copy backends; package files are never exposed as writable store symlinks
   in graph volumes. On Windows, safe archive symlinks are materialized as
   copied content, and `.cmd`/`.ps1` bin shims are generated.
9. **Platform primitives** — `src/platform.rs` provides `find_executable`,
   `script_command`, and `same_file_identity` shared by lifecycle and CLI
   execution. The platform script command produces `sh -c` on Unix and
   `cmd.exe /D /S /C` on Windows, with `COMSPEC` fallback.
10. **Lifecycle runner** — `src/lifecycle.rs` supplies npm-compatible script
   environments and `--ignore-scripts`. Workspace/compatible installs retain
   the disposable sandbox. Graph-volume installs execute scripts in the
   volume after isolating each package from the store, so derived output
   persists and dependencies resolve through the complete volume tree.
   `src/derived/store.rs` contains the content-addressed derived-artifact
   implementation described by the long-term plan, but the current graph
   lifecycle path is volume-derived rather than publishing through that store;
   reconciling those two strategies is an open hardening decision.
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
  mappings participate in the canonical bytes.
- **Install plan** — a versioned plan containing graph identity, materialized
  entries, bins, and lifecycle-derived paths. It is disposable; `bpm.lock` is
  authoritative.

All maps, package paths, workspace discoveries, and serialized fields are
sorted/canonicalized so output does not depend on hash-map order, filesystem
enumeration, task completion order, or network timing.

## Materialization and lifecycle invariants

1. A store image is never mutated through a project path.
2. Graph package files are hardlinked or copied up before lifecycle scripts can
   mutate them; `.bin` scripts retain package-relative symlink semantics.
3. Ordinary project attachment is shallow and reusable; the local compatibility
   view is selected when realpath containment is required by the toolchain.
4. A plan-cache hit skips resolution, fetching, materialization, and lifecycle
   when both the graph volume and project view remain valid.
5. Old volume/plan layouts are invalidated by explicit materializer/layout
   versions rather than changing the canonical graph header or frozen-lockfile
   identity.

## Remaining architectural decisions

- Integrate graph lifecycle execution with `src/derived/store.rs` for
  cross-graph derived-artifact reuse, or formally commit to graph-keyed volume
  derivation and retire the unused path.
- Wire a `clonefile`/`FICLONE` syscall binding (e.g. the `libc` crate) into the
  `Reflink` materialize backend and `probe_fs_capabilities` so `Auto` can select
  CoW reflink on supporting filesystems (macOS APFS, Linux btrfs/xfs). The
  backend variant, capability probe, and `BPM_PROJECT_VIEW=reflink` plumbing
  have landed; only the syscall call sites and `Auto` selection remain.
- Windows junction/reflink attachment performance (currently correctness-first
  local hardlink/copy).
- Combine the async resolver with the streaming install path for maximum cold
  overlap.
- Default-flip the async resolver to `BPM_ASYNC_RESOLVE=1` once the A/B
  evidence and streaming composition are settled.
- Upload support and conditional-PUT idempotent writes for the remote cache.
