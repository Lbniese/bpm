---
title: CLI reference
---
{% include nav.html %}

# CLI reference

This reflects the CLI as implemented today, including lockfile resolution,
network configuration, execution, garbage collection, publishing, and audit
commands.

## `bpm --version`

Prints the built-in package version.

## `bpm doctor [--json]`

Locates the nearest `package.json` (project root) and the repository root
(nearest `.git`, falling back to the project root), parses the manifest,
and reports structured diagnostics: missing/invalid manifest fields,
lifecycle scripts, native addons, workspace/override usage, and
declared-dependency counts.

- Exit code is nonzero if any diagnostic has `error` severity.
- `--json` emits the same report as canonical, deterministic JSON instead of
  human-readable text.

```bash
bpm doctor
bpm doctor --json
```

## `bpm fetch <target> [flags]`

Fetches a package by **npm-style spec** or **exact URL**. For a spec, BPM
resolves the name against the registry (like `npm`/`bun`), reads the tarball URL
and integrity from the packument, then downloads, verifies its SHA-512
integrity, stores it immutably, and (by default) extracts it once into a package
image. For an exact URL or `file://`/local path, BPM downloads it directly.

Accepted targets:

| Target | Behavior |
|---|---|
| `lodash` | resolve `dist-tags.latest` from the registry |
| `lodash@4.17.21` | exact version |
| `lodash@^4.17.0`, `@~`, `@>=`, `@4.x`, `@*` | highest published version matching the semver range |
| `@scope/pkg`, `@scope/pkg@1.0.0` | scoped names |
| `https://.../pkg.tgz`, `file:///abs/x.tgz`, `./x.tgz` | fetched directly (no resolution) |

| Flag | Meaning |
|---|---|
| `--registry <url>` | Registry base URL for spec resolution. Defaults to `$BPM_REGISTRY`, then `https://registry.npmjs.org`. Ignored for URL/path targets. |
| `--integrity sha512-<base64>` | Expected integrity. For a spec this overrides the registry's `dist.integrity`; for a URL it enables verification and cache-hit reuse without re-downloading. |
| `--store <dir>` | Store root. Defaults to `$BPM_STORE`, then `$HOME/.bpm`. |
| `--no-extract` | Only download/verify/store the archive; skip extraction. |
| `--json-metrics <path>` | Write phase-timing metrics as canonical JSON to `path`. |

Environment: `BPM_TRACE=1` prints a CSV phase trace to stderr; `BPM_REGISTRY`
sets the default registry.

```bash
bpm fetch lodash --store /tmp/store
bpm fetch lodash@4.17.21 --registry https://registry.npmjs.org
bpm fetch https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz \
    --integrity sha512-XXXX...
```

Repeated `fetch` of the same artifact/integrity performs no network or
extraction work (a spec is re-resolved each run, but the tarball itself is
served from the immutable store) — this is the Milestone 1 success criterion.

## `bpm install [target] [flags]`

Two modes:

- **`bpm install` (no argument)** — installs the locked dependency graph from
  `bpm.lock` into `node_modules` (see the frozen-installer docs). If no lockfile
  exists, it resolves the nearest `package.json` and writes `bpm.lock` first;
  use `--frozen` to require an existing lockfile.
- **`bpm install <target>`** — fetches a single package (resolved exactly like
  `bpm fetch`) and links its declared executables into a global bin directory so
  they appear on your `PATH`. This is handy for quickly grabbing a CLI tool
  (e.g. `bpm install cowsay`).

The bin directory is chosen in this order: `$BPM_BIN`, then `~/.local/bin`
(if it exists), then `~/bin`. Each declared `bin` becomes a symlink there
pointing at the immutable store image; the linked file is made executable.

```bash
bpm install cowsay                 # links `cowsay` + `cowthink` into ~/.local/bin
bpm install lodash@4.17.21         # resolves an exact version, then links bins
bpm install --registry https://my.registry.dev my-cli
```

Notes:

- Packages whose `package.json` declares no `bin` fail with a clear error
  (`declares no 'bin' executables; nothing to link`) — `install <target>` only
  links executables, it does not resolve the package's *dependencies*.
- Re-running is idempotent: an already-correct symlink is left in place.
- If the chosen bin directory is not on your `PATH`, `bpm` prints a hint.

| Flag | Meaning |
|---|---|
| `<target>` | Package spec or exact URL/`file://`/path, resolved like `bpm fetch`. Omit for lockfile install. |
| `--registry <url>` | Registry base URL for spec resolution (bin-install mode only). |
| `--store <dir>` | Store root. Defaults to `$BPM_STORE`, then `$HOME/.bpm`. |
| `--frozen`, `--concurrency`, `--json-metrics`, `--ignore-scripts`, `--legacy-peer-deps` | Apply to the lockfile install mode (no `<target>`). |

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

## `bpm exec <command> [args...]`

Runs a command from the nearest project's `node_modules/.bin` with that
folder prepended to `PATH`, preserving native arguments and the child's exit
status.

## `bpm gc [flags]`

Removes unreferenced store objects older than 30 days. Use `--older-than 30d` to
change the grace period or `--max-size 50GB` to reclaim eligible objects until
the store is within a size cap. Active leases and graphs attached to projects
are always retained.

## Exit codes

`0` on success. Nonzero on any hard error (missing/invalid input, integrity
mismatch, unsupported lockfile version) or when `bpm doctor` finds an
`error`-severity diagnostic. Error messages are structured and actionable,
never a bare "installation failed".

## Publish and audit

`bpm publish` creates an npm-compatible package attachment from the current
project and uploads it using the configured registry credentials. `bpm audit`
posts the project's dependency inventory to the registry advisory endpoint;
use `--json` for the raw advisory response.

`bpm import` accepts npm `package-lock.json` plus the supported text forms of
Yarn, pnpm, and Bun lockfiles and writes the canonical `bpm.lock`.
