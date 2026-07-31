---
title: BPM — Bloom Package Manager
---
{% include nav.html %}

# Bloom Package Manager (BPM)

BPM is an npm-compatible package manager focused on **installation
performance**, **global storage reuse**, and **deterministic dependency
graphs**.

BPM is not trying to compete with npm, pnpm, Yarn, or Bun on breadth of
features. Its first goal is narrower:

> Install existing npm-compatible projects faster by eliminating repeated
> downloads, repeated extraction, repeated dependency-graph work, and
> repeated filesystem materialization.

This site tracks the implementation as it lands. The plan of record is
in the repository.

## Current status

BPM is in active development. Representative end-to-end capabilities include:

- **Installation and deterministic locks** — `bpm install` and `bpm ci`
  resolve or consume canonical lockfiles, support workspaces and lifecycle
  scripts, and reuse immutable artifacts, package images, and graph volumes.
- **Dependency mutation** — install/add, remove/uninstall, upgrade, and dedupe
  update supported registry dependencies while preserving lock authority.
- **Execution and storage operations** — run/exec commands, verified fetches,
  local and remote cache reuse, cache inspection, and garbage collection.
- **Registry operations** — view/outdated queries plus publish, audit, whoami,
  token, dist-tag, and owner administration.

This is intentionally narrower than full npm feature parity. See the
[CLI reference](cli.md) for commands, supported cases, and exact usage.

## Design principles

- **Global data is immutable.** Downloaded archives, extracted package
  images, dependency graphs, and compiled install plans are never mutated
  in place; they are built in a temporary location, verified, and published
  atomically.
- **No global installation lock.** Concurrency safety comes from
  per-artifact locks and atomic create-or-reuse operations, not one lock
  around the whole install.
- **Determinism first.** Output must not depend on hash-map iteration
  order, filesystem enumeration order, network completion order, thread
  scheduling, or locale. Canonical inputs are sorted before hashing or
  serialization, and this is covered by regression tests.
- **Fail clearly.** Unsupported behavior that could affect resolution,
  security, integrity, scripts, or reproducibility is reported as a
  structured, actionable error — never silently ignored.

See [Architecture](architecture.md) for the subsystem breakdown.

## Getting the code

```bash
git clone https://github.com/lbniese/bpm.git
cd bpm
docker compose up -d --build
docker compose exec dev bash
cargo build --release
```

See [Development](development.md) for the full containerized workflow and
validation commands.
