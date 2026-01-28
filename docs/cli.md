---
title: CLI reference
---
{% include nav.html %}

# CLI reference

This reflects the CLI as implemented today. Flags and commands not listed
surface (`bpm install --frozen`, `bpm run`, `bpm exec`, `bpm gc`), which
arrives with later milestones.

## `bpm --version`

Prints the built-in package version.

## `bpm doctor [--json]`

Locates the nearest `package.json` (project root) and the repository root
(nearest `.git`, falling back to the project root), parses the manifest,
and reports structured diagnostics: missing/invalid manifest fields,
lifecycle scripts, native addons, unsupported workspace/override usage, and
declared-dependency counts.

- Exit code is nonzero if any diagnostic has `error` severity.
- `--json` emits the same report as canonical, deterministic JSON instead of
  human-readable text.

```bash
bpm doctor
bpm doctor --json
```

## `bpm fetch <url> [flags]`

Downloads a package tarball by **exact URL**, verifies its SHA-512
integrity, stores it immutably, and (by default) extracts it once into a
package image.

| Flag | Meaning |
|---|---|
| `--integrity sha512-<base64>` | Expected integrity. Enables verification and cache-hit reuse without re-downloading. Without it, BPM must download to learn the digest. |
| `--store <dir>` | Store root. Defaults to `$BPM_STORE`, then `$HOME/.bpm`. |
| `--no-extract` | Only download/verify/store the archive; skip extraction. |
| `--json-metrics <path>` | Write phase-timing metrics as canonical JSON to `path`. |

Environment: `BPM_TRACE=1` prints a CSV phase trace to stderr.

```bash
bpm fetch https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz \
    --integrity sha512-XXXX... \
    --store /tmp/store
```

Repeated `fetch` of the same artifact/integrity performs no network or
extraction work — this is the Milestone 1 success criterion.

## `bpm import [path] [flags]`

Imports an npm `package-lock.json` (`lockfileVersion` 3 only) into a
canonical `bpm.lock`. The source lockfile is never modified.

| Argument/flag | Meaning |
|---|---|
| `path` | Input lockfile path. Defaults to `./package-lock.json`. |
| `--out <path>` | Output `bpm.lock` path. Defaults to `bpm.lock` next to the input. |
| `--json` | Emit the resulting lockfile plus diagnostics as JSON to stdout instead of a human summary. |

Unsupported constructs (workspace/`link` entries, `os`/`cpu` platform
constraints) are recorded and reported as warnings/info diagnostics, not
silently dropped. An unsupported `lockfileVersion`, a missing `packages`
table, or a malformed `bin` field fails with a clear, nonzero-exit error.

```bash
bpm import                        # ./package-lock.json -> ./bpm.lock
bpm import path/to/lock.json --out path/to/bpm.lock --json
```

## Exit codes

`0` on success. Nonzero on any hard error (missing/invalid input, integrity
mismatch, unsupported lockfile version) or when `bpm doctor` finds an
`error`-severity diagnostic. Error messages are structured and actionable
never a bare "installation failed".
