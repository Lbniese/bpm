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

## `bpm init [flags]`

Scaffolds a new `package.json` in the current directory (npm `init`
compatibility). Each field is resolved from an explicit flag, a default
(`--yes`), or an interactive prompt where empty input keeps the default.

- The package name defaults to the current directory name, lowercased with
  non-npm characters replaced by `-`.
- `--yes`/`-y` skips all prompts and uses defaults.
- `--force` overwrites an existing `package.json`.
- The name is validated with the same rules as `bpm doctor`; an invalid name
  aborts without writing a file.

```bash
bpm init -y
bpm init --name @scope/lib --license Apache-2.0
bpm init
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
| `--git-prepare` | Run npm-compatible Git build-context `prepare` for Git dependencies using a transient regular+dev closure. **Enabled by default** for Git dependencies; disable with `--no-git-prepare` or `BPM_GIT_PREPARE=0`. |
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

## `bpm link [name] [flags]`

npm `link` compatibility: developer package linking via a global registry under
`$BPM_STORE/links/`.

**`bpm link`** (run inside a package directory) registers the cwd package
globally as a symlink `$BPM_STORE/links/<name>` -> the package directory. The
package name is read from `package.json`.

**`bpm link <name>`** (run inside a consumer project) consumes a registration:
it adds `<name>: "file:$BPM_STORE/links/<name>"` to `package.json` and runs the
normal install, which materializes `node_modules/<name>` -> the registered
target. The dependency is recorded with `"link": true` in `bpm.lock`.

Re-registering a name repoints the global symlink; consumers follow the repoint
on their next `bpm install` (their `package.json` points at the symlink, not the
resolved target, so each resolution re-canonicalizes through the current
registration).

```bash
cd ~/dev/mylib && bpm link          # register mylib globally
cd ~/dev/myapp && bpm link mylib    # consume: node_modules/mylib -> mylib
```

| Flag | Meaning |
|------|---------|
| `<name>` | Omit to register the cwd package; give a name to consume a registration. |
| `--store <dir>` | Store root. Defaults to `$BPM_STORE`, then `$HOME/.bpm`. |
| `--registry <url>` | Registry base URL (passed through to the consume install step). |

> Symlink-based. On Windows, creating directory symlinks requires Developer
> Mode or administrator privileges; enable them before using `bpm link`.

## `bpm unlink [name] [--global] [flags]`

Reverses `bpm link`.

**`bpm unlink <name>`** (in a consumer) removes the consumed dependency from
`package.json` and reinstalls, deleting the `node_modules/<name>` link.

**`bpm unlink --global [<name>]`** unregisters a package from the global
registry (`$BPM_STORE/links/<name>`). With `--global` and no name, unregisters
the cwd package.

```bash
cd ~/dev/myapp && bpm unlink mylib         # stop consuming mylib
cd ~/dev/mylib  && bpm unlink --global     # unregister mylib globally
```

| Flag | Meaning |
|------|---------|
| `<name>` | Package to unlink (required without `--global`). |
| `--global`, `-g` | Unregister from the global registry instead of the project. |
| `--store <dir>` | Store root. Defaults to `$BPM_STORE`, then `$HOME/.bpm`. |
| `--registry <url>` | Registry base URL (passed through to the unconsume reinstall step). |

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

### Resolution snapshot cache

After a successful fresh resolve, BPM stores the validated lockfile in the
selected store keyed by the complete resolver input identity. `--prefer-offline`
and `--offline` reuse that snapshot when the project manifest, workspace state,
registry configuration, peer mode, and target are unchanged. A malformed or
missing snapshot falls back to normal resolution; ordinary installs still
revalidate registry metadata, and `--prefer-online` never uses the snapshot.

```bash
bpm import                        # ./package-lock.json -> ./bpm.lock
bpm import path/to/lock.json --out path/to/bpm.lock --json
```

## `bpm exec <command> [args...]`

Runs a command from the nearest project's `node_modules/.bin` with that
folder prepended to `PATH`, preserving native arguments and the child's exit
status.

## `bpm outdated [target] [flags]`

Shows packages whose locked version is older than the latest version published
to the registry. The output format matches npm's convention:

```
Package              Current  Wanted   Latest
lodash               4.17.21  4.17.21  5.0.0
express              4.18.2   4.19.0   5.1.0
```

- **Current** — the version resolved in the project lockfile.
- **Wanted** — the highest published version that satisfies the declared semver
  range in `package.json`.
- **Latest** — the `latest` dist-tag on the registry.

When no packages are outdated, `bpm outdated` prints "All packages are up to
date." and exits zero.

An optional package name argument limits the check to that one package.
Registry failures for individual packages produce warnings on stderr but do not
stop the command — other packages are still reported.

| Flag | Meaning |
|------|---------|
| `--registry <url>` | Registry base URL. Defaults to `$BPM_REGISTRY`, then the configured npm registry. |
| `--store <dir>` | Store root. Defaults to `$BPM_STORE`, then `$HOME/.bpm`. |
| `--offline` | Never contact the registry; resolve only against cached metadata. |
| `--json` | Emit machine-readable JSON keyed by package name. |

```bash
bpm outdated                       # show all outdated packages
bpm outdated lodash                # check only lodash
bpm outdated --json                # machine-readable output
bpm outdated --offline             # cached metadata only
```

## `bpm view <package> [field] [flags]`

Shows package metadata fetched from the registry (npm `view` compatibility).
Given a package spec (`<name>`, `<name>@<version>`, or `<name>@<range>`), it
fetches the packument, resolves the version (defaulting to `dist-tags.latest`),
and prints the resolved version's metadata:

```
demo-pkg@2.0.0
DEPRECATED: use demo-pkg2 instead

dist-tags:
  latest: 2.0.0

dependencies:
  lodash: ^4.17.21
  ms: ^2.1.3

bin:
  demo: ./index.js

dist:
  tarball: https://registry.npmjs.org/demo-pkg/-/demo-pkg-2.0.0.tgz
  integrity: sha512-...
  shasum: ...

versions: 3 published
```

Only the resolution-relevant fields bpm extracts are shown (dependencies,
optional/peer dependencies, `bin`, `dist`, `engines`, `os`/`cpu`/`libc`, and
`deprecated`); richer manifest fields such as `description`, `license`, or
`homepage` are not retained by the resolver today.

An optional field selector prints just one value, supporting dotted paths:

```bash
bpm view lodash                      # full metadata for the latest version
bpm view lodash@4.17.21              # a specific version
bpm view lodash@^4.0.0               # the highest version in a range
bpm view lodash dependencies         # just the dependencies map
bpm view lodash dist.tarball         # a single nested field
bpm view lodash versions             # all published versions (one per line)
bpm view lodash dist-tags            # the dist-tags map
```

| Flag | Meaning |
|------|---------|
| `<field>` | Optional dotted field selector (see examples above). |
| `--registry <url>` | Registry base URL. Defaults to `$BPM_REGISTRY`, then the configured npm registry. |
| `--store <dir>` | Store root. Defaults to `$BPM_STORE`, then `$HOME/.bpm`. |
| `--offline` | Never contact the registry; resolve only against cached metadata. |
| `--json` | Emit machine-readable JSON. |

## `bpm whoami [flags]`

npm `whoami` compatibility: print the username authenticated to the configured
registry.

Reads npm config (`$HOME/.npmrc` then the project `.npmrc`) and calls the
registry's `/-/whoami` endpoint, sending the configured bearer token. Prints
the username and exits `0` on success. Exits nonzero with a clear message when
no token is configured for the registry or the registry rejects the
credentials.

```bash
bpm whoami                       # who am I on the default registry?
bpm whoami --registry https://npm.example  # …on a private registry
```

| Flag | Meaning |
|------|---------|
| `--registry <url>` | Registry base URL. Defaults to the config's registry, then `https://registry.npmjs.org`. |

## `bpm token <action> [flags]`

npm `token` compatibility: list, create, and revoke registry authentication
tokens against npm's `/-/npm/v1/tokens` endpoint. All subcommands require an
authenticated session (a bearer token in `.npmrc`).

```bash
bpm token                       # list tokens (alias: list)
bpm token create                # mint a token (prompts for the password)
BPM_PASSWORD=... bpm token create --read-only --cidr 10.0.0.0/8
bpm token revoke abc123         # revoke by the `key` shown by `token list`
```

### `bpm token list`

Prints each token's id (`key`), whether it is read-only, its CIDR whitelist,
and creation time. Add `--json` for machine-readable output.

### `bpm token create`

Mints a new token. npm requires re-authentication with the account password to
mint a token. Set the password from the environment for automation or enter it
at a hidden prompt in an interactive terminal. The two-factor OTP, if the
account requires it, comes from `$BPM_OTP` or a hidden prompt via
`--prompt-otp`. Prints the new token (the full secret is shown once); add
`--json` for machine-readable output.

### `bpm token revoke <id>`

Revokes the token whose `key` (shown by `bpm token list`) equals `<id>`.

### Flags

| Flag | Meaning |
|------|---------|
| `--registry <url>` | Registry base URL. Defaults to the config's registry, then `https://registry.npmjs.org`. |
| `--read-only` | (`create`) Mint a read-only token that cannot publish. |
| `--cidr <CIDR>` | (`create`) CIDR whitelist entry; repeatable. |
| `--prompt-otp` | (`create`/`revoke`) Prompt for a two-factor OTP with hidden input. The OTP is otherwise read from `$BPM_OTP`. |
| `--json` | (`list`/`create`) Emit machine-readable JSON. |

Passwords and OTPs are never accepted on the command line: argv is visible to
process listings and persists in shell history. The `create` password is read
from nonempty `$BPM_PASSWORD`, or from a hidden `Password: ` prompt when stdin
is a terminal; a noninteractive `create` without `$BPM_PASSWORD` fails before
the network. The OTP comes from nonempty `$BPM_OTP`, or from a hidden prompt
only when `--prompt-otp` is set. Empty values are rejected; OTPs with
surrounding whitespace are rejected rather than silently trimmed.

## `bpm dist-tag <action> [args] [flags]`

npm `dist-tag` compatibility: list, set, and remove a package's distribution
tags (named pointers like `latest`, `beta`, `next`).

```bash
bpm dist-tag ls lodash                  # list lodash's tags
bpm dist-tag ls                          # list tags for the local package
bpm dist-tag add mypkg@1.2.3 next        # point `next` at 1.2.3
bpm dist-tag add mypkg@1.2.3             # `add` defaults the tag to `latest`
bpm dist-tag rm mypkg old                # remove the `old` tag
```

### `bpm dist-tag ls [pkg]`

Prints `tag: version` for each dist-tag (sorted). With no package, reads the
`name` from the local `package.json`. Listing is a public read — no
authentication required for public packages. Add `--json` for machine-readable
output.

### `bpm dist-tag add <pkg>@<version> [tag]`

Points `tag` (default `latest`) at `version` via the registry's
``/-/package/<name>/dist-tags/<tag>`` endpoint. Requires an authenticated
session with publish rights. (For scoped packages the name is percent-encoded,
e.g. `@scope/name` → `@scope%2Fname`.)

### `bpm dist-tag rm <pkg> <tag>`

Removes `tag` from `pkg`. Requires publish rights.

### Flags

| Flag | Meaning |
|------|---------|
| `--registry <url>` | Registry base URL. Defaults to the config's registry, then `https://registry.npmjs.org`. |
| `--json` | (`ls`) Emit machine-readable JSON. |

## `bpm owner <action> [args] [flags]`

npm `owner` compatibility: list, add, and remove a package's
owners/collaborators.

```bash
bpm owner ls lodash                  # list lodash's maintainers
bpm owner ls                         # list maintainers for the local package
bpm owner add alice mypkg            # grant `alice` write access on mypkg
bpm owner add alice                  # add to the local package's name
bpm owner rm alice mypkg             # remove `alice` from mypkg
```

### `bpm owner ls [pkg]`

Prints each maintainer as `name <email>` (omitting the email when absent),
matching npm's `owner ls` output. Reads the packument's top-level
`maintainers` field (the full packument, not the abbreviated install metadata,
which omits it), so public packages need no authentication. With no package,
reads the `name` from the local `package.json`. Add `--json` for
machine-readable output.

### `bpm owner add <user> [pkg]`

Grants `user` write access on `pkg` via the registry's
``/-/package/<name>/collaborators/<user>`` endpoint (body
`{"permissions":"write"}`). With no package, targets the local `package.json`
name. Requires an authenticated session with owner rights. (For scoped
packages the name is percent-encoded, e.g. `@scope/name` → `@scope%2Fname`.)

### `bpm owner rm <user> [pkg]`

Removes `user` from `pkg`'s collaborators via the same endpoint (DELETE).
Requires owner rights.

### Flags

| Flag | Meaning |
|------|---------|
| `--registry <url>` | Registry base URL. Defaults to the config's registry, then `https://registry.npmjs.org`. |
| `--json` | (`ls`) Emit machine-readable JSON. |

## `bpm why <package>`

Shows why a package is present in the dependency tree by walking the lockfile
in reverse: for each installed version of the target package, it reports which
packages (or the root project) declare a dependency on it.

The output format follows npm's convention:

```
lodash@4.17.21
  root: lodash@^4.17.0
```

A transitive dependency shows which package requires it:

```
accepts@1.3.8
  express@4.18.2 requires accepts@^1.3.8
```

If multiple packages depend on the same target, each is listed. If a package
has no dependents (orphaned or direct dependency without a root entry), the
output shows `<no parents>`.

The command is lockfile-local and read-only — it never contacts the registry
or modifies any files. A lockfile (`bpm.lock` or supported `package-lock.json`
v3) must exist.

```bash
bpm why lodash               # show why lodash is installed
bpm why accepts              # show who depends on accepts
```

## `bpm ls [flags]`

Lists installed packages as a dependency tree (npm `ls` compatibility). The
tree is built from the project lockfile (`bpm.lock` or `package-lock.json`)
and rendered with `name@version` nodes:

```
test-project@1.0.0
├── express@4.18.2
│   └── accepts@1.3.8
└── lodash@4.17.21
```

By default, packages that appear more than once in the graph (diamonds and
shared transitives) are expanded once and shown collapsed elsewhere, matching
`npm ls`. Pass `--all` to expand every occurrence.

Edges come from the resolver metadata (`resolution.packages[path].dependencies[].target`)
when present and otherwise from npm's `node_modules` lookup order over the
compatibility `dependencies` map, so the tree reflects the physical install.
The command is lockfile-local and read-only — it never contacts the registry
or modifies any files.

An optional positional package name filters the output to only the paths that
lead to that package (like `npm ls <pkg>`).

| Flag | Meaning |
|------|---------|
| `[name]` | Only show paths leading to packages matching this name. |
| `--all` / `-a` | Expand every occurrence instead of deduplicating. |
| `--depth <n>` | Limit the tree to `n` levels below the root (default: unlimited). |
| `--json` | Emit machine-readable JSON (npm `ls --json` shape). |

```bash
bpm ls                        # full dependency tree
bpm ls accepts                # only the path(s) to accepts
bpm ls --all                  # expand every occurrence
bpm ls --depth 0              # direct dependencies only
bpm ls --json                 # machine-readable output
```

## `bpm gc [flags]`

Removes unreferenced store objects older than 30 days. Use `--older-than 30d` to
change the grace period or `--max-size 50GB` to reclaim eligible objects until
the store is within a size cap. Active leases and graphs attached to projects
are always retained.

## `bpm cache [action] [flags]`

npm `cache` compatibility: inspect and reclaim the global artifact + metadata
cache (the store root shared by [`bpm gc`](#bpm-gc-flags)).

**`bpm cache`** / **`bpm cache ls`** prints a read-only size + object-count
breakdown that reconciles exactly to the displayed total: every regular file
under the store root is classified into one category — `artifacts`, `images`,
`derived`, `graphs`, `plans`, `snapshots` (resolution snapshots), `metadata`
(`store.db` + `metadata-cache.db` + migrations), `scratch` (`tmp` + `locks`),
`leases`, `links`, or an explicit `other` bucket for unrecognized files. Totals
are regular-file bytes; symlink targets are not followed, so a symlink never
inflates the count.

**`bpm cache verify`** runs a repair + garbage-collection pass with the default
policy (objects older than 30 days eligible) and reports reclaimed space and any
repaired index/lock anomalies.

**`bpm cache clean`** reclaims **every** unreferenced object (no grace period,
zero-byte cap). Protected installs are preserved by their durable leases and
project registrations, so a clean never breaks a running or registered install —
but everything else is removed and will be re-fetched on next use.

```bash
bpm cache                       # show cache sizes and counts
bpm cache ls --store /opt/cache # inspect a non-default store
bpm cache verify                # gc + repair, then report
bpm cache clean                 # reclaim all unreferenced objects
```

| Flag | Meaning |
|------|---------|
| `[action]` | `ls`/`list` (default), `verify`, or `clean`. |
| `--store <dir>` | Store root. Defaults to `$BPM_STORE`, then `$HOME/.bpm`. |

> `bpm cache clean` is a stricter form of `bpm gc --older-than 0 --max-size 0`;
> `bpm cache verify` is `bpm gc` with the default policy plus a repair report.

## Exit codes

`0` on success. Nonzero on any hard error (missing/invalid input, integrity
mismatch, unsupported lockfile version) or when `bpm doctor` finds an
`error`-severity diagnostic. Error messages are structured and actionable,
never a bare "installation failed".

## Publish and audit

`bpm publish` creates an npm-compatible package attachment from the current
project and uploads it using the configured registry credentials. If the
account requires two-factor authentication, set the OTP from `$BPM_OTP` or pass
`--prompt-otp` to enter it at a hidden prompt; it is never accepted as an argv
value. `bpm audit` posts the project's resolved dependency inventory to the
registry advisory endpoint; use `--json` for the raw advisory response.

The audited inventory comes from `bpm.lock`, or from npm
`package-lock.json` (lockfile version 3) when no `bpm.lock` exists.
Declaration-only `package.json` data is **not** audited because it lacks
resolved versions. Audit fails closed: a missing, unreadable, malformed, or
unsupported-version lockfile is a hard error (exit nonzero), and a malformed
or wrong-shaped advisory response is also a hard error rather than a silent
zero-vulnerability success. A dependency-free valid lock and a valid empty
advisory object (`{}`) remain a successful zero-finding result. Use
`--audit-level` to control the severity threshold at which nonzero findings
fail the command.

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

## `bpm upgrade [pkg...]`

Re-resolves the manifest within its declared ranges and bumps locked versions
to the newest satisfying ones, rewriting `bpm.lock` (and `package-lock.json`
for npm-authority projects). **Never edits the ranges in `package.json`**
(npm default). Named packages are reported specifically; the whole graph is
re-resolved because it must stay globally consistent. A named package that is
not declared in `package.json` produces a warning and is skipped (non-fatal).
Omit the package list to upgrade everything within its declared ranges.

```bash
bpm upgrade lodash
bpm upgrade            # upgrade all within their ranges
```

## `bpm dedupe`

Re-resolves the manifest to minimize duplicate package versions and rewrites
`bpm.lock`. BPM's resolver already minimizes duplicates during initial
resolution (it unifies versions wherever the declared ranges permit), so
`dedupe` on an already-minimal graph reports `already minimal` and is
byte-stable. If the lockfile had drifted from a clean resolve, `dedupe`
reports the reduction it applied.

```bash
bpm dedupe
```

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

### Streaming install (default)

By default, `bpm install` overlaps tarball downloads with resolution: as soon
as the resolver places each registry package in the graph, its tarball download
and extraction start on worker threads, so downloads make progress while the
rest of the graph is resolved.

Set `BPM_STREAM_INSTALL=0` to resolve the whole graph before downloading
anything — useful for benchmarking or isolating streaming-related regressions.

### Async resolver (default)

By default, `bpm install` uses the non-blocking async resolver and combines it
with the streaming install path. The resolver never stalls on inline packument
fetches, and its output `bpm.lock` is byte-identical to the blocking path.

With streaming enabled (also the default), the async resolver feeds each placed
package to the download pipeline via a non-blocking sink, combining concurrent
packument fetches with overlapped downloads. Missing pipeline units (from
channel backpressure) are fetched in a sequential pass after resolution
completes.

Set `BPM_ASYNC_RESOLVE=0` or `BPM_ASYNC_RESOLVE=false` to force the blocking
resolver as a diagnostic kill-switch. Independently, `BPM_STREAM_INSTALL=0`
disables download overlap while retaining async resolution unless the async
kill-switch is also set.

`BPM_RESOLVER_MAX_IN_FLIGHT` bounds concurrent async registry requests (default
`32`, clamped to `1..64`). Lower it for a constrained registry or raise it for
high-latency HTTP/1.1 environments; lockfile placement remains deterministic.

Artifact downloads use HTTP/2 via ALPN by default so concurrent response bodies
can share a connection. Set `BPM_HTTP2=0` to force HTTP/1.1 when diagnosing a
registry or transport compatibility issue.
