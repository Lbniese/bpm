SHELL := /bin/bash

.PHONY: build test run lint fmt fmt-check clippy bench audit

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
