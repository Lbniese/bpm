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

BPM is in early implementation. What exists today and works end to end:

- **`bpm doctor`** — locates a project's `package.json` and reports
  structured, deterministic diagnostics.
- **`bpm fetch <spec|url>`** — resolves an npm-style package spec (`lodash`,
  `lodash@4.17.21`, `@scope/pkg@^1`) against the registry, or fetches a
  tarball by exact URL / `file://` path; verifies its SHA-512 integrity,
  stores it immutably, and safely extracts it exactly once. Repeated fetches
  of the same artifact do no network or extraction work.
- **`bpm import [package-lock.json]`** — imports an npm
  `package-lock.json` (`lockfileVersion` 3) into a canonical, deterministic
  `bpm.lock`.

See [Milestones](milestones.md) for what's done and what's next, and the
[CLI reference](cli.md) for exact usage.

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
make docker-up
make docker-shell
cargo build --release
```

See [Development](development.md) for the full containerized workflow and
validation commands.
