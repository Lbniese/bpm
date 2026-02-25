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
no re-resolving.

- **Immutable by design** — downloaded archives, extracted images, and
  dependency graphs are never mutated; they are built, verified, and published
  atomically.
- **Concurrent by default** — per-artifact locking replaces global install
  locks. Multiple installs run safely in parallel.
- **Deterministic output** — byte-for-byte reproducible lockfiles and metrics,
  independent of hash-map ordering, thread scheduling, or network timing.
- **Measured performance** — every phase is instrumented. Benchmarks compare
  against npm and pnpm with median/p95/standard deviation reporting.

## Commands

| Command | Description |
|---|---|
| `bpm doctor` | Inspect the nearest `package.json` and report diagnostics |
| `bpm fetch <spec\|url>` | Resolve a package by spec (`lodash`, `lodash@4.17.21`) or fetch a tarball by exact URL, then verify, store, and extract |
| `bpm install [<spec\|url>]` | Install from `bpm.lock`, or fetch a package and link its bins globally (`bpm install cowsay`) |
| `bpm import` | Convert npm `package-lock.json` v3 to `bpm.lock` |
| `bpm bench` | Run performance benchmark scenarios and emit timing results |

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
