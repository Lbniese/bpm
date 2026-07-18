---
title: Development
---
{% include nav.html %}

# Development

## Development

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

A Docker/Dev Container configuration is included for reproducible builds.
Start it with:

```bash
docker compose up -d --build
docker compose exec dev bash
```

## Required validation before any change is considered done

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo build --release --workspace
```

Performance work must use release builds — debug-build timings are not
comparable evidence. Report median, p95, and variance when benchmarking.

## Working rules (summary)

The authoritative rules live in [`AGENTS.md`](AGENTS.md) at the repository
root. In short:

- **Benchmark before optimizing.** Identify the scenario, record a
  baseline, profile, make the smallest change, rerun, report median/p95/
  variance.
- **Determinism.** No hash-map iteration order, filesystem enumeration
  order, network completion order, thread scheduling, or locale may leak
  into output. Sort canonical inputs before hashing/serializing, and add a
  regression test.
- **Immutability.** Published store objects (archives, images, graph
  volumes, compiled plans, derived artifacts) are never modified in place.
- **No global locks.** Prefer per-artifact/per-graph locks and atomic
  create-or-reuse operations.
- **Filesystem operations are expensive.** Avoid redundant `stat`,
  `exists`, `canonicalize`, `mkdir`, `chmod`, `rename`, or recursive scans.
- **Bounded, explicit, observable concurrency** everywhere (network,
  hashing, decompression, extraction, filesystem, lifecycle scripts).
- **Fail clearly.** Unsupported behavior affecting resolution, security,
  integrity, scripts, auth, platform selection, or reproducibility must
  return a structured, actionable error — not a silent divergence or a bare
  "installation failed".
- **Protect credentials.** Never print registry tokens, auth headers,
  authenticated URLs, or `.npmrc` secrets.

## Scope control

Before implementing a feature, classify it:

```text
A. Required for current milestone
B. Required for npm compatibility of an existing fixture
C. Required for measurement or debugging
D. Nice to have
```

Only A, B, and C are implemented without an explicit request.

## Testing expectations by area

Store changes need interrupted-write/concurrent-writer/corruption/read-only/
atomic-reuse coverage; extraction changes need path-traversal/absolute-path/
symlink/permission/malformed-archive/duplicate-entry coverage; graph changes
need stable-ID/peer-context/platform/workspace/ordering coverage; materializer
changes need isolation/resolution/bin-link/repeat-install/reuse/read-only
coverage; lifecycle changes need script-order/environment/failure-propagation/
`--ignore-scripts`/reuse/immutability coverage; resolver and networking changes
need environment/offline/retry/redirect/auth/timeout/cache/concurrency coverage.

## Contributing

See [`CONTRIBUTING.md`](https://github.com/lbniese/bpm/blob/main/CONTRIBUTING.md)
in the repository root for the pull request checklist and commit message
conventions.

## Publishing this repository and its docs site

This repository is git-initialized locally with CI (`.github/workflows/ci.yml`)
and a GitHub Pages deployment workflow (`.github/workflows/pages.yml`)
already in place, but creating the actual GitHub repository, pushing, and
enabling Pages requires GitHub credentials that a sandboxed build
environment does not have. Do this once, from a machine with `git`/`gh`
authenticated against your GitHub account:

1. **Create the empty GitHub repository** (no README/license/gitignore —
   this repo already has them):

   ```bash
   gh repo create lbniese/bpm --public --source=. --remote=origin --push
   ```

   or create it via the GitHub web UI and then:

   ```bash
   git remote add origin git@github.com:lbniese/bpm.git
   git push -u origin main
   ```

2. **Enable GitHub Pages via GitHub Actions.** In the repository's
   **Settings → Pages**, set *Source* to **GitHub Actions**. The next push
   to `docs/**` (or a manual run of the "Deploy docs to GitHub Pages"
   workflow) will build and publish the site to
   `https://lbniese.github.io/bpm/`.

3. **Set your real git identity** if you care about commit authorship — the
   initial local commits were made with a placeholder identity:

   ```bash
   git config user.name "Your Name"
   git config user.email "you@example.com"
   ```
