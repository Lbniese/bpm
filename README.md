# Bloom Package Manager (BPM)

[![CI](https://github.com/lbniese/bpm/actions/workflows/ci.yml/badge.svg)](https://github.com/lbniese/bpm/actions/workflows/ci.yml)
[![Docs](https://img.shields.io/badge/docs-github.io-blue)](https://lbniese.github.io/bpm/)

BPM is an npm-compatible package manager that installs projects faster by
eliminating repeated downloads, repeated extraction, repeated dependency-graph
work, and repeated filesystem materialization. Packages are stored immutably in
a global content-addressed store and shared across projects.

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
| `bpm upgrade [<pkg>...]` | Re-resolve within declared ranges without editing manifest ranges |
| `bpm dedupe` | Re-resolve to minimize duplicate versions and rewrite the selected lock |
| `bpm ci` | Perform a reproducible frozen install from `bpm.lock` or supported npm v3 lock |
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
