# Contributing to BPM

for humans and coding agents alike. Read it, along with

## Before you start

   documentation.
2. Look at existing tests and benchmarks for the area you're touching.
3. Classify the change:

   ```text
   A. Required for current milestone
   B. Required for npm compatibility of an existing fixture
   C. Required for measurement or debugging
   D. Nice to have
   ```

   Only A, B, and C are implemented without an explicit request — see

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

Every change must pass, from inside the container:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo build --release --workspace
```

For performance-sensitive changes, benchmark before and after with a
release build and report median, p95, and standard deviation — a single
faster run is not evidence.

## Tests are part of the change, not an afterthought

extraction, graph, materializer, lifecycle) must be tested against.
Determinism regressions (hash-map iteration order, filesystem enumeration
order, network completion order, locale) need an explicit test, not just a
manual check.

## Commit messages

```text
feat(store): add atomic artifact publication
perf(materializer): batch sorted directory creation
fix(extract): reject symlink path traversal
docs(site): add CLI reference page
bench(ci): add warm graph reuse scenario
```

Keep pull requests focused: one objective, its affected milestone,
correctness tests, relevant benchmark results if applicable, and explicit
notes about anything left unsupported. Avoid combining unrelated refactors
with feature work.

## Security and correctness come first

Do not weaken integrity checks, extraction protections, or store isolation
to improve a benchmark number. A performance regression is reverted,
optimized before merge, or accepted with an explicit documented trade-off —
never silently shipped.
