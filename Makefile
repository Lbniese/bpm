SHELL := /bin/bash

.PHONY: build test run lint fmt fmt-check clippy bench audit deny

build:
	cargo build

test:
	cargo test

run:
	cargo run -- $(ARGS)

lint: clippy

fmt:
	cargo fmt

fmt-check:
	cargo fmt -- --check

clippy:
	cargo clippy --all-targets --all-features -- -D warnings

bench:
	cargo build --release && ./target/release/bpm bench --runs 3 --json results.json $(ARGS)

audit:
	cargo deny check advisories

# Full local cargo-deny gate: advisories (the CI hard gate) plus the
# licenses/bans/sources stretch tables. Catches new RUSTSEC advisories at the
# desk before they can turn CI red — `make audit` runs the CI-matching
# advisories-only subset.
deny:
	cargo deny check
