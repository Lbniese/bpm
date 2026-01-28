---
title: Architecture
---
{% include nav.html %}

# Architecture

BPM's design splits into six subsystems. The full detail lives in
this page is a map, not a replacement.

## Subsystems

1. **Manifest / lockfile reader** — parses `package.json` and imports npm
   `package-lock.json`, producing BPM's own canonical `bpm.lock`.
   Implemented: manifest parsing (`src/manifest.rs`), project/repository root
   detection (`src/project.rs`), package-lock v3 import (`src/npm_lock.rs`),
   and the `bpm.lock` format (`src/lockfile.rs`).
2. **Resolver** — turns manifest + registry metadata into a concrete
   dependency graph. Not implemented yet; BPM currently installs from an
   already-resolved lockfile rather than resolving ranges itself.
3. **Global artifact store** — content-addressed, immutable storage for
   downloaded tarballs and extracted package images. Implemented in
   `src/store.rs`, `src/download.rs`, `src/archive.rs`, `src/integrity.rs`.
4. **Global graph store** — reusable, immutable dependency-graph volumes
   shared across projects with the same graph ID. Not implemented yet
   (Milestone 4).
5. **Materializer** — projects a dependency graph into a project's
   `node_modules` and bin directory using symlinks, reflinks, hard links, or
   copies as the filesystem allows. Not implemented yet (Milestone 2, phase 2).
6. **Lifecycle runner** — executes npm-compatible lifecycle scripts against
   an isolated build sandbox and publishes results as derived immutable
   artifacts. Not implemented yet (Milestone 5).

## Global store layout

```text
~/.bpm/
├── artifacts/sha512/<prefix>/<digest>.tgz   # downloaded tarballs
├── images/sha512/<prefix>/<digest>/         # extracted package images
├── derived/blake3/<prefix>/<digest>/        # lifecycle-script output (future)
├── graphs/blake3/<prefix>/<digest>/         # reusable graph volumes (future)
├── plans/blake3/<prefix>/<digest>.bin       # compiled install plans (future)
├── metadata/
├── locks/
├── leases/
├── tmp/
└── store.db                                 # SQLite metadata + GC (future)
```

Rules that hold today and will keep holding as the store grows:

- all published store objects are immutable
- temporary writes happen under `tmp/`, never in place
- integrity is verified **before** publication
- publication uses atomic rename
- writers race safely; there is no global installation lock
- locks are scoped per artifact/object, not global

## Hashing rules

- **Artifact ID** — `sha512(package tarball bytes)`, matched against the
  registry-published integrity string when one is supplied.
- **Package instance ID** *(planned)* — `blake3` of the artifact ID, sorted
  direct dependency instance IDs, peer context, platform, and architecture.
- **Graph ID** *(planned)* — `blake3` of the canonical lockfile graph,
  workspace layout, and target platform/architecture/ABI.
- **Install plan ID** *(planned)* — `blake3` of the graph ID, materializer
  version, filesystem capability profile, and lifecycle policy.

Every hash input has (or will have) a canonical serialization so identical
logical inputs always hash identically, independent of construction order.

## Domain model (current)

```rust
struct Sha512Digest([u8; 64]);   // src/integrity.rs
struct Integrity { digest: Sha512Digest }

struct Lockfile {                 // src/lockfile.rs — bpm.lock
    lockfile_version: u32,
    generator: String,
    root: RootEntry,
    packages: Vec<PackageEntry>,  // sorted by node_modules path
}
```

The richer `PackageInstanceId` / `GraphId` / `InstallPlanId` model from
(graph-plan cache) needs it — no placeholder hashing exists yet.
