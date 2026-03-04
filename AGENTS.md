# Repository Guidelines

## Project Structure & Module Organization

This repository is a Rust crate for `bpm`, an npm-compatible package manager.
Primary source code lives in `src/`, with CLI entry points under `src/cli/` and
core subsystems such as resolver, store, materializer, metadata, and GC logic in
their own modules. Integration tests live in `tests/`, shared test helpers in
`tests/common/`, and sample package fixtures in `fixtures/`.

## Build, Test, and Development Commands

- `cargo build` or `make build`: compile the binary in debug mode.
- `cargo test` or `make test`: run the full test suite.
- `cargo run -- <args>` or `make run ARGS="doctor"`: run the CLI locally.
- `cargo fmt` or `make fmt`: apply standard Rust formatting.
- `cargo fmt -- --check` or `make fmt-check`: verify formatting without changes.
- `cargo clippy --all-targets --all-features -- -D warnings` or `make clippy`:
  run lint checks.
- `cargo build --release && ./target/release/bpm bench --runs 3 --json results.json`:
  build and run benchmarks.

## Coding Style & Naming Conventions

Use Rust 2021 conventions and keep formatting `rustfmt`-clean. Prefer
snake_case for modules, functions, and files; use `CamelCase` for types and
traits. Keep CLI code in `src/cli/` and prefer small, focused modules over large
god files. Run `cargo fmt` before committing; `clippy` should be warning-free.

## Testing Guidelines

Use Rust integration tests in `tests/` for end-to-end behavior and regression
coverage. Name test files after the area they cover, for example
`tests/install.rs` or `tests/manifest_parsing.rs`. Add fixtures under
`fixtures/` when scenarios need real package layouts or lockfiles. Run targeted
tests with `cargo test install` or a single file with `cargo test --test fetch`.

## Commit & Pull Request Guidelines

Recent commits use a conventional style such as `feat(...)`, `build(...)`, and
`fix(...)`, with a short scope and imperative subject line. Keep commits focused
and descriptive. Pull requests should summarize the change, explain any user
visible impact, link related issues when applicable, and include screenshots or
logs only when behavior or output changes.

## Agent-Specific Instructions

Do not overwrite an existing `AGENTS.md`. Keep changes aligned with the current
Cargo and Makefile workflow unless the repository itself changes.
