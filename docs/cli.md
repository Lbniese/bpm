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
  the nearest supported project lock into `node_modules` (see the frozen-installer
  docs). BPM checks each directory upward, preferring a sibling `bpm.lock` over
  `package-lock.json`; a nested `package-lock.json` v3 wins over an ancestor
  `bpm.lock`. If no lockfile exists, it resolves the nearest `package.json` and
  writes `bpm.lock` first; use `--frozen` to require an existing supported lock.
- **`bpm install <target>`** — adds one or more registry targets to the local
  manifest, resolves the complete edited graph, updates the selected lock, and
  installs it. This is equivalent to `bpm add <target>`.
- **`bpm install -g <target>`** — fetches one package (resolved exactly like
  `bpm fetch`) and links its declared executables into a global bin directory.
  Global mode accepts exactly one target.

The global bin directory is chosen in this order: `$BPM_BIN`, then
`~/.local/bin` (if it exists), then `~/bin`. Each declared `bin` becomes a
symlink there pointing at the immutable store image; the linked file is made
executable.

```bash
bpm install cowsay                 # adds `cowsay` to the local project
bpm install lodash@4.17.21         # adds lodash with the default ^ save range
bpm install -g my-cli              # links my-cli's bins into the global bin dir
bpm install -g --registry https://my.registry.dev my-cli
```

Notes:

- Packages whose `package.json` declares no `bin` fail with a clear error
  (`declares no 'bin' executables; nothing to link`) — `install -g <target>` only
  links executables, it does not resolve the package's *dependencies*.
- Re-running a global install is idempotent: an already-correct symlink is left
  in place.
- If the chosen global bin directory is not on your `PATH`, `bpm` prints a hint.

| Flag | Meaning |
|---|---|
| `<target>` | Package spec or exact URL/`file://`/path, resolved like `bpm fetch`. Omit for lockfile install. |
| `--registry <url>` | Registry base URL for spec resolution (bin-install mode only). |
| `--store <dir>` | Store root. Defaults to `$BPM_STORE`, then `$HOME/.bpm`. |
| `--frozen`, `--concurrency`, `--json-metrics`, `--ignore-scripts`, `--legacy-peer-deps` | Apply to the lockfile install mode (no `<target>`). `--frozen` accepts either `bpm.lock` or supported `package-lock.json` v3 and reports drift against the selected lock filename. |
| `--git-prepare` | Run npm-compatible Git build-context `prepare` for Git dependencies using a transient regular+dev closure. Explicitly opt-in; `BPM_GIT_PREPARE=1` is equivalent. |
| `--derived-store` | Reuse lifecycle-derived package images across changed graphs. Explicitly opt-in; `BPM_DERIVED_STORE=1` is equivalent. |

### Direct `package-lock.json` use and `bpm ci`

`bpm install`, `bpm install --frozen`, and `bpm ci` can consume a supported npm
`package-lock.json` v3 directly when no nearer/sibling `bpm.lock` wins. The
package-lock input is read-only: BPM normalizes it in memory, writes install
state in `.bpm-state`, and does not create `bpm.lock`. Native no-lock resolution
still writes `bpm.lock`.

Precedence is deterministic: nearest directory wins, and within the same
directory `bpm.lock` wins over `package-lock.json`. `bpm import` is optional for
teams that want to migrate to BPM's native lock format; it is not required for
install or CI. Package-lock versions 1 and 2 are rejected clearly. Workspace or
`link` package-lock entries and non-link entries without `resolved` are currently
unsupported for direct install and fail before fetching or materializing.

## `bpm import [path] [flags]`

Imports an npm `package-lock.json` (`lockfileVersion` 3 only) into a
canonical `bpm.lock`. The source lockfile is never modified. This migration step
is optional; direct install/CI can read a supported package-lock v3 without
writing `bpm.lock`.

| Argument/flag | Meaning |
|---|---|
| `path` | Input lockfile path. Defaults to `./package-lock.json`. |
| `--out <path>` | Output `bpm.lock` path. Defaults to `bpm.lock` next to the input. |
| `--json` | Emit the resulting lockfile plus diagnostics as JSON to stdout instead of a human summary. |

Unsupported constructs (workspace/`link` entries, `os`/`cpu` platform
constraints) are recorded and reported as warnings/info diagnostics, not
silently dropped. An unsupported `lockfileVersion`, a missing `packages`
table, or a malformed `bin` field fails with a clear, nonzero-exit error.

### Remote artifact cache (experimental)

`bpm fetch`, `bpm install`/`add`, `bpm remove`/`uninstall`, and `bpm ci` accept
`--remote-cache HTTPS_URL` or `BPM_REMOTE_CACHE`. This also applies to the
single-target `bpm install -g` path. The optional `BPM_REMOTE_CACHE_TOKEN` is
isolated from npm
registry credentials. Only known SHA-512 artifact keys are requested; every
response is verified before local publication. Misses, errors, corrupt
responses, and `--offline` preserve normal origin behavior. See
[remote-cache-protocol.md](remote-cache-protocol.md). The prototype does not
share lockfiles, images, graph volumes, or lifecycle-derived output.

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

## Adding and removing dependencies

`bpm install <pkg>` / `bpm i <pkg>` / `bpm add <pkg>` (the default, without
`-g`) is a local dependency mutation: BPM edits `package.json`, resolves the
complete edited graph, writes the selected lock, and installs. Multiple
registry targets may be passed in one transaction.

Save flags:

| Flag | Effect |
|---|---|
| `-D` / `--save-dev` | add to `devDependencies` and remove from `dependencies` |
| `-E` / `--save-exact` | save the resolved version as `X.Y.Z` instead of `^X.Y.Z` |

Save-spec rules: `--save-exact` saves `X.Y.Z`; an explicit supported range
(`^`, `~`, `>`, `<`, `=`, `*`) is preserved verbatim; a bare name, `@latest`, or
an exact version without `--save-exact` saves the default `^X.Y.Z`. Adding to
`dependencies` removes the same name from `devDependencies` and vice-versa; if
the name already lives in `optionalDependencies` or `peerDependencies`, BPM
errors rather than silently moving it.

`bpm remove <pkg>` / `bpm uninstall` / `bpm rm` / `bpm un` removes one or more
names from every root dependency group, re-resolves the whole manifest, and
reinstalls. A name that is not declared is a no-op: neither `package.json` nor
the lock is rewritten. `bpm remove --global` is rejected because global-bin
ownership metadata does not exist yet.

`bpm install -g <pkg>` retains the pre-mutation user-bin linking behavior; `-g`
with no target is an error.

Lock authority is deterministic: a `bpm.lock` project stays a `bpm.lock`
project and a `package-lock.json` v3 project stays an npm v3 project.
For npm-authority projects, BPM exports a strict `lockfileVersion: 3` document
that `npm ci --ignore-scripts` accepts for the supported registry-only corpus.

This first slice supports registry specs only. Git, URL/tarball, `file:`,
`link:`, workspace, patch, and `--save-optional`/`--save-peer`/`--no-save`
mutation are deferred to later source-protocol work and are rejected before
any file is touched.

Crash boundary: parsing, target resolution, graph resolution, export, and the
two-file publication are all completed before either project file is changed,
so any failure there leaves `package.json` and the lock byte-identical. A later
download, materialization, or lifecycle failure may leave the already-published
manifest and lock in place; re-run `bpm install` to retry.
