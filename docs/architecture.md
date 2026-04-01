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
   override declarations.
2. **Native resolver** — `src/resolver/` resolves registry ranges, tags, and
   exact versions into a deterministic physical graph. It handles supported
   root overrides, strict or legacy peer modes, npm platform filtering,
   optional reachability, cycles, and local workspaces. Non-frozen `bpm install`
   resolves `package.json` and writes `bpm.lock`; frozen installs are
   resolution-free.
3. **Artifact and metadata stores** — `src/store.rs`, `src/download.rs`,
   `src/archive.rs`, and `src/integrity.rs` provide immutable, verified
   tarballs and extracted package images. `src/metadata/` records artifacts,
   images, derived objects, graphs, plans, projects, leases, and access data
   in SQLite. Publication uses temporary paths, per-object locking, and
   atomic rename.
4. **Graph and plan cache** — `src/graph.rs` computes canonical graph IDs,
   records platform/workspace/override/peer inputs, and stores disposable
   install plans beside the lockfile in `.bpm-state`. Plan validation checks
   graph-volume integrity and the live project view.
5. **Reusable graph volumes** — `src/volume.rs` builds a complete graph-keyed
   `node_modules` projection under `graphs/blake3/<id>/`. Package files are
   hardlinked to immutable store images, while `.bin` entries remain relative
   symlinks so Node resolves bin scripts from their package directory. Ordinary
   projects use shallow top-level relays for the O(top-level) fast path.
   Projects depending on `next` automatically receive a project-local hardlink
   view so tools such as Turbopack do not reject dependency realpaths outside
   the project. `BPM_PROJECT_VIEW=relay|local` overrides that choice.
6. **Materializer** — `src/materializer.rs` supports compatible npm-v3 layout
   and strict declared-edge validation. It has symlink, hardlink, and fallback
   copy backends; package files are never exposed as writable store symlinks
   in graph volumes.
7. **Lifecycle runner** — `src/lifecycle.rs` supplies npm-compatible script
   environments and `--ignore-scripts`. Workspace/compatible installs retain
   the disposable sandbox. Graph-volume installs execute scripts in the
   volume after isolating each package from the store, so derived output
   persists and dependencies resolve through the complete volume tree.
   `src/derived/store.rs` contains the content-addressed derived-artifact
   implementation described by the long-term plan, but the current graph
   lifecycle path is volume-derived rather than publishing through that store;
   reconciling those two strategies is an open hardening decision.
8. **CLI and measurement** — `src/cli/` exposes install, ci, import, exec,
   run, fetch, doctor, gc, audit, publish, and bench. `src/bench.rs` records
   machine/tool versions, phase timings, cache state, and JSON results.

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
- Expand project-local compatibility attachment beyond the automatic Next.js
  case, ideally using filesystem clone/reflink capabilities where available.
- Finish non-Unix project attachment support; Windows currently builds the CLI
  but directory-symlink project attachment remains unsupported.
