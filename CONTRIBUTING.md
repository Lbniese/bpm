# Contributing to BPM

This guide applies to human and automated contributors alike. Read it, along
with [`AGENTS.md`](AGENTS.md), for the current architecture, milestone docs,
and repository conventions.

## Before you start

1. Read the repository guidance in `AGENTS.md` and current design documents
   under `docs/` for the area you are touching.
2. Look at existing tests and benchmarks for the area you're touching.
3. Classify the change:

   ```text
   A. Required for current milestone
   B. Required for npm compatibility of an existing fixture
   C. Required for measurement or debugging
   D. Nice to have
   ```

   Only A, B, and C are implemented without an explicit request — see the
   project maintainers for scope D approval.

## Building and testing

```bash
cargo build
cargo test
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

For performance work, use release builds:

```bash
cargo build --release
make bench
```

## Required validation

Every change must pass, from inside the container or on the host:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo build --release --workspace
cargo deny check advisories      # requires: cargo install cargo-deny --locked
```

For performance-sensitive changes, benchmark before and after with a
release build and report median, p95, and standard deviation — a single
faster run is not evidence.

## Tests are part of the change, not an afterthought

Store, extraction, graph, materializer, lifecycle, networking, resolver,
and CLI changes must include success plus negative, interruption, and reuse
tests appropriate to the area.  Determinism regressions (hash-map iteration
order, filesystem enumeration order, network completion order, locale) need
an explicit test, not just a manual check.

## Commit messages

```text
feat(store): add atomic artifact publication
perf(materializer): batch sorted directory creation
fix(extract): reject symlink path traversal
docs(site): add CLI reference page
bench(ci): add warm graph reuse scenario
```

Keep commits focused: one objective per commit, correctness tests,
relevant benchmark results if applicable, and explicit notes about anything
left unsupported. Avoid combining unrelated refactors with feature work.

## Security and correctness come first

Do not weaken integrity checks, extraction protections, or store isolation
to improve a benchmark number. A performance regression is reverted,
optimized before merge, or accepted with an explicit documented trade-off —
never silently shipped.
