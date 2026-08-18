# Bloom Package Manager (BPM)

[![CI](https://github.com/lbniese/bpm/actions/workflows/ci.yml/badge.svg)](https://github.com/lbniese/bpm/actions/workflows/ci.yml)
[![Docs](https://img.shields.io/badge/docs-github.io-blue)](https://lbniese.github.io/bpm/)

BPM is an npm-compatible package manager that installs projects faster by
eliminating repeated downloads, repeated extraction, repeated dependency-graph
work, and repeated filesystem materialization. Packages are stored immutably in
a global content-addressed store and shared across projects. Graphs are built
in private staging, lifecycle-completed, and atomically published; each project
receives a real isolated Reflink-or-Copy view, with deep-copy fallback when CoW
is unavailable.

## Recent Changes

- 2026-08-18: Cut warm-install ownership overhead across the board — plan-cache hits skip the redundant metadata refresh, volume-reuse installs lease only the graph object and skip already-recorded registrations, and HTTP/registry clients are constructed lazily; `repeat_install` is ~2x faster than bun, `warm_store` 100.8→57.7ms, `partial_dependency_change` 100.1→57.0ms, and `monorepo_incremental` 94.4→24.6ms, with permanent `plan_validate`/`ownership_refresh` phase metrics for future profiling.
- 2026-08-04: Restored green CI on Windows and Ubuntu; copy and reflink-fallback materialization now preserve source modification times for stable repeat-install reuse, and `v0.3.0` ships as a signed GitHub Release for Apple Silicon and Linux.
- 2026-08-03: Released bpm 0.3.0 — npm-compatible developer and registry-management commands (`init`, `ls`, `view`, `cache`, `whoami`, `token`, `dist-tag`, `owner`, `link`), dev-only install profiles, `package-lock.json` lockfileVersion 2/3 import, directory-clonefile materialization, and parallel graph-package fingerprinting.
- 2026-08-03: Regenerated the cold-path reference baseline at bpm 0.2.1; the headline `large-frontend` `true_cold` install improved to 3.67x pnpm, with per-run registry request counts and phase timings recorded under `bpm_metrics`.
- 2026-08-02: Added dev-only install omission for `bpm install`/`bpm ci` via `--omit=dev`/`--include=dev` (include wins) and `NODE_ENV=production`; filtering is an in-memory runtime projection, so `bpm.lock` and npm `package-lock.json` authority stay complete and byte-unchanged.
- 2026-08-02: Accepted npm `package-lock.json` lockfileVersion 2 and 3 through the shared importer for import, install/CI, audit, and npm-authority mutations; exports canonicalize npm authority to strict lockfileVersion 3.
- 2026-08-02: Hardened Git and HTTP diagnostics, refused credential-bearing mutation redirects, published mutation lockfiles atomically, and completed lifecycle-before-publication graph builds with isolated Reflink-or-Copy views.
- 2026-07-31: Reconciled the README and CLI reference with every shipped command and added an inventory regression enforcing the flat `## Recent Changes` form.
- 2026-07-30: Percent-encoded user-controlled registry path segments (dist-tags, collaborator names, token keys), made token-creation POSTs non-retriable, aligned the Docker base image with the pinned Rust toolchain, and fixed GC reclaimed-byte over-reporting.
- 2026-07-30: Synchronized the release-signing public key embedded in `install.sh` with the checked-in key so signed prebuilt releases verify, with a DER-fingerprint regression test.
- 2026-07-29: Added `bpm owner` (npm `owner` compatibility) to list/add/remove package collaborators via the `/-/package/<escaped>/collaborators/<user>` endpoint.
- 2026-07-28: Added npm-compatible `bpm token`, `bpm dist-tag`, `bpm whoami`, `bpm cache`, `bpm link`/`unlink`, `bpm view`, `bpm ls`, and `bpm init` over the shared `RegistryClient` and HTTP layer, each with unit and args-parsing tests.
- 2026-07-28: Fixed packument deserialization against the real npm registry for legacy `engines` arrays and hardened the mock registry against parallel-test-load flakiness.
- 2026-07-27: Added a persistent resolution-snapshot cache, bounded async registry concurrency (`BPM_RESOLVER_MAX_IN_FLIGHT`), HTTP/2-by-default artifact downloads, and metadata-only image sidecars for cold publication.
- 2026-07-26: Closed the cold-resolver gap — the async resolver now runs on a multi-threaded tokio runtime with singleflighted, prefetched, version-scoped packument fetches and concurrent root/child fan-out, cutting the cold `dependency_resolution` phase several-fold with byte-identical lockfiles.
- 2026-07-26: Added `bpm upgrade` and `bpm dedupe` mutation commands, remote-cache upload (opt-in `PUT` via `BPM_REMOTE_CACHE_PUSH=1`), derived-store persistence in the SQLite metadata repository, a performance narrative page, and NaN-safe `total_cmp` bench sample sorting.
- 2026-07-26: Made `bpm outdated` registry queries concurrent via `std::thread::scope` with deterministic output ordering.
- 2026-07-25: Flipped git-prepare default to ON (`BPM_GIT_PREPARE=0` disables it), added an in-memory LRU packument body cache, and shipped `bpm why` and `bpm outdated` diagnostic commands.
- 2026-07-22: Streamed package-image metadata without loading file payloads, made the Git and patch source caches race-safe across processes, and constrained registry artifact provenance with a single bounded read policy.
- 2026-07-21: Wired the `Reflink` materialize backend to macOS `clonefile`/Linux `FICLONE` with hardlink-to-copy fallback and a `BPM_PROJECT_VIEW=reflink` view, confirmed the dual-resolver placement cores are byte-identical, and added resolver parity and git-prepare test coverage.
- 2026-02-15: Shipped the initial content-addressed store, lockfile-driven `bpm install`/`bpm ci`, the blocking resolver, lifecycle script execution, and `bpm add`/`bpm remove` mutations over hardlink/copy materialization.

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
install reuses the resolve, download, extraction, and lifecycle work. Graphs are
completed privately before publication, and projects receive isolated local
Reflink-or-Copy views, preserving nested dependency and relative `.bin`
semantics without writable hardlink or relay aliases.

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
| `bpm gc` | Garbage-collect unreferenced global-store data |
| `bpm cache [ls\|verify\|clean]` | Inspect, repair, or reclaim the global artifact and metadata cache |
| `bpm fetch <spec\|url>` | Resolve or directly fetch, verify, store, and extract a package; supports offline/preference modes |
| `bpm bench` | Run benchmark fixtures, compare baselines, and emit timing results |
| `bpm import [path]` | Convert a supported external lockfile to canonical `bpm.lock` |
| `bpm init` | Create a validated `package.json` interactively or from flags |
| `bpm publish` | Pack and publish the current package to an npm-compatible registry |
| `bpm audit` | Query registry advisories for versions resolved in the project lock |
| `bpm install [<registry-spec>...]` (`bpm i`, `bpm add`) | Install the selected lock, add registry packages transactionally, or use `-g` to link package bins |
| `bpm link [<name>]` | Register or consume an unscoped or scoped developer link, such as `@scope/pkg` |
| `bpm unlink [<name>]` | Remove a consumed link, or unregister it with `--global` |
| `bpm uninstall <pkg>...` (`bpm remove`, `bpm rm`, `bpm un`) | Remove dependencies transactionally and reinstall the resolved graph |
| `bpm upgrade [<pkg>...]` | Select named dependency closures, or all dependencies when omitted, within declared ranges without editing manifest ranges |
| `bpm dedupe` | Re-resolve to minimize duplicate versions and rewrite the selected lock |
| `bpm ci` | Perform a reproducible frozen install from `bpm.lock` or supported npm v2/v3 lock |
| `bpm bin` | Print the user-level executable-shim directory |
| `bpm root` | Print the project `node_modules` root or global store root |
| `bpm prefix` | Print the project prefix or global BPM prefix |
| `bpm exec <command>` (`bpm x`) | Execute with the nearest project's dependency bins on `PATH` |
| `bpm run <script>` (`bpm run-script`) | Execute a root lifecycle script with npm-compatible environment variables |
| `bpm outdated [<pkg>]` | Show stale locked versions using bounded, deduplicated metadata lookups |
| `bpm view <package> [field]` | Read package metadata from the configured registry |
| `bpm whoami` | Print the registry-authenticated username |
| `bpm token [action]` | List, create, or revoke registry authentication tokens |
| `bpm dist-tag [action]` | List, set, or remove package distribution tags |
| `bpm owner [action]` | List or mutate package owners/collaborators |
| `bpm why <pkg>` | Explain why a package is in the dependency tree |
| `bpm ls [<pkg>]` (`bpm list`) | Render the installed dependency tree |

## Documentation

📖 [Documentation site](https://lbniese.github.io/bpm/) — [Architecture](docs/architecture.md) · [CLI reference](docs/cli.md) · [Development](docs/development.md) · [Contributing](CONTRIBUTING.md)

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
