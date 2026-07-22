# Bloom Package Manager (BPM)

[![CI](https://github.com/lbniese/bpm/actions/workflows/ci.yml/badge.svg)](https://github.com/lbniese/bpm/actions/workflows/ci.yml)
[![Docs](https://img.shields.io/badge/docs-github.io-blue)](https://lbniese.github.io/bpm/)

BPM is an npm-compatible package manager that installs projects faster by
eliminating repeated downloads, repeated extraction, repeated dependency-graph
work, and repeated filesystem materialization. Packages are stored immutably in
a global content-addressed store and shared across projects.

## Recent Changes

- 2026-07-22: Constrained registry artifact provenance and bounded every artifact read — registry `dist.tarball` is now validated to HTTP/HTTPS or registry-relative URLs (rejecting `file:`/non-HTTP schemes before download), a single 512 MiB compressed-byte policy governs all download/source/remote-cache reads with limit-plus-one failure and scratch cleanup, and explicit local tarball/`file:` dependencies plus cross-origin CDN tarballs still work (Plan 012 completed)
- 2026-07-21: Reconciled `docs/git-prepare-design.md` with shipped slices 1/3/4/5 (Plan 001 completed)
- 2026-07-21: Added active git-prepare failure/rerun/ref-pinning/identity tests (Plan 003 completed)
- 2026-07-21: Split registry.rs into registry/ package (Plan 004 completed)
- 2026-07-21: Added disjunctive/multiple-dep/transitive parity tests (Plan 002 completed)
- 2026-07-21: Added multi-package parity tests for transitive/peer/cycle/optional graphs (Plan 002 corpus extended)
- 2026-07-21: Wired the `Reflink` materialize backend to macOS `clonefile(2)`/Linux `FICLONE` with a runtime `probe_fs_capabilities` probe and hardlink→copy fallback, and added the `BPM_PROJECT_VIEW=reflink` project view (copy-on-write, store-image isolated; Plan 006 Phases 1–4 + docs; Windows junctions still deferred)
- 2026-07-21: Generalized the local-view trigger with `BPM_LOCAL_VIEW_PACKAGES` (default `next`, env-extensible)
- 2026-07-21: Characterized dual-resolver placement cores and confirmed async/blocking byte-identical output across the full parity corpus (Plan 005 Phase 1; extraction + default-flip deferred as HIGH-risk gated work)

## Quick start

```bash
curl -fsSL https://raw.githubusercontent.com/Lbniese/bpm/main/install.sh | sh

cd my-project
bpm doctor              # inspect project configuration
bpm fetch lodash        # download and cache a package by name (npm/bun-style)
bpm fetch lodash@4.17.21 # or by exact version / semver range
bpm import              # convert package-lock.json to bpm.lock
bpm install --frozen    # materialize node_modules from bpm.lock
```

The installer installs into `/usr/local/bin` by default and will ask for your
`sudo` password only for that final copy step (it builds as your normal user,
where the Rust toolchain lives). Don't prepend `sudo` to the `curl` — that runs
the whole script as root, where Rust isn't installed.

```bash
# Install without sudo (e.g. into ~/.local/bin, which must be on your PATH):
BPM_INSTALL_DIR="$HOME/.local/bin" \
  curl -fsSL https://raw.githubusercontent.com/Lbniese/bpm/main/install.sh | sh
```

If the Rust toolchain is not available, download a pre-built binary from the
[Releases page](https://github.com/lbniese/bpm/releases).

## Why BPM?

Most package managers cache individual packages. BPM caches **complete
dependency graphs** — when two projects resolve the same graph, the second
install reuses every byte of the first. No re-downloading, no re-extracting,
no re-resolving. Ordinary projects attach through shallow graph-volume relays;
Next.js projects automatically receive a local hardlink compatibility view so
Turbopack can keep dependency realpaths inside the project.

- **Immutable by design** — downloaded archives, extracted images, and
  dependency graphs are never mutated; they are built, verified, and published
  atomically.
- **Concurrent by default** — per-artifact locking replaces global install
  locks. Multiple installs run safely in parallel.
- **Deterministic output** — byte-for-byte reproducible lockfiles and metrics,
  independent of hash-map ordering, thread scheduling, or network timing.
  Cached metadata is revalidated with `ETag`/`Last-Modified` and a `304` reuses
  the stored body verbatim, so cache hits and misses resolve identically.
- **Measured performance** — every phase is instrumented. Benchmarks compare
  against npm and pnpm with median/p95/standard deviation reporting.

## Commands

| Command | Description |
|---|---|
| `bpm doctor` | Inspect the nearest `package.json` and report diagnostics |
| `bpm fetch <spec\|url>` | Resolve a package by spec (`lodash`, `lodash@4.17.21`) or fetch a tarball by exact URL, then verify, store, and extract. Supports `--offline`, `--prefer-offline`, `--prefer-online` |
| `bpm install [<spec\|url>]` | Install the project lockfile, or add registry targets to the local manifest; use `bpm install -g <spec>` for global bin linking. Supports `--offline`, `--prefer-offline`, `--prefer-online` |
| `bpm ci` | Reproducible frozen install from `bpm.lock` (npm `ci` compatibility) |
| `bpm import` | Convert npm `package-lock.json` v3 to `bpm.lock` and preserve root manifest metadata |
| `bpm exec <command>` | Execute a local dependency binary with the project bin path |
| `bpm run <script>` | Execute a root package script with npm-compatible environment variables |
| `bpm bench` | Run performance benchmark scenarios and emit timing results |
| `bpm gc` | Garbage-collect unused global store data |

## Documentation

📖 [Documentation site](https://lbniese.github.io/bpm/) — [Architecture](docs/architecture.md) · [CLI reference](docs/cli.md) · [Milestones](docs/milestones.md) · [Development](docs/development.md) · [Contributing](CONTRIBUTING.md)

## Building from source

```bash
git clone https://github.com/lbniese/bpm.git
cd bpm
cargo build --release
./target/release/bpm --version
```

```bash
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

## License

MIT
