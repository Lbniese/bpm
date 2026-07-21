# Git `prepare` compatibility design

> Status: **Slices 1, 3, 4, and 5 shipped** behind the `--git-prepare` flag
> (env `BPM_GIT_PREPARE=1`), default-off, at commit `21b87fc`. This document
> remains the design authority; §10 records each slice's shipped status. Slice 2
> (ordered caller-supplied phase list) is effectively realized in lifecycle
> (`LIFECYCLE_PHASES` / `PREPARE_PHASES`); confirm the derived-domain separator
> detail against `src/derived/key.rs` before asserting it. Slice 6 (oracle
> parity + default-on decision) is partially in place: an active BPM contract
> test exists (`tests/git_prepare_characterization.rs`), the npm oracle runs in
> CI as `--ignored`, and default-on remains **not** flipped.
>
> The oracle is the executable contract for the recorded toolchain. If npm's
> observed behavior changes on a supported target, the oracle fails first and
> this document must be revised before any production slice lands.

## 1. Status and recorded toolchain

| Tool | Version observed |
|---|---|
| npm | 11.12.1 |
| node | v26.0.0 |
| git | 2.50.1 (Apple Git-155) |

Reference documentation (treated as background; the local oracle is the
authoritative contract for the versions above):

- npm lifecycle scripts: <https://docs.npmjs.com/cli/v11/using-npm/scripts> (accessed 2026-07-19)
- npm package-lock v3 `packages` map and `resolved`/`integrity` semantics: <https://docs.npmjs.com/cli/v11/configuring-npm/package-lock-json> (accessed 2026-07-19)

## 2. Oracle fixture and commands

The oracle (`tests/git_prepare_characterization.rs`, `#[ignore]`) builds a fully
local fixture at runtime:

- A Git package repository `gitpkg` with a runtime dependency `regulartool`
  (`file:./vendor/regulartool`) and a dev dependency `devtool`
  (`file:./vendor/devtool`), **both committed inside the repository** so they
  survive npm's clone-into-temp preparation step.
- Six lifecycle scripts — `preprepare`, `prepare`, `postprepare`, `preinstall`,
  `install`, `postinstall` — each invoking `node record.js <phase>`.
- `record.js` appends one TSV line per invocation: the phase name, a context
  tag (`BUILD` while running inside the cloned source, `FINAL` while running
  inside the consumer's `node_modules`), and live `require()` probes for
  `devtool` and `regulartool`.
- `build-dist.js` (the `prepare` body) requires **both** tools and writes a
  distributable `dist/built.js` carrying their resolved `MARKER` values.
- Three reachable commits: good (REV 1), prepare-throws, and good (REV 2); a
  branch `stable` and tag `v1.0.0` pinned at the REV-1 commit.

A consumer project depends on `gitpkg` through `git+file://<repo>#<ref>`. Each
case uses its own consumer temp directory and isolated `--cache`, run with
`npm install --cache <tmp> --no-fund --no-audit`. No public network and no
credentials are used.

Run the oracle with:

```text
cargo test --test git_prepare_characterization -- --ignored --nocapture --test-threads=1
```

It prints the recorded tool versions once and asserts every stable fact below.

## 3. Phase ordering table

For a default install of an immutable-commit Git dependency (Case 1), the
observed lifecycle order, split by npm's two execution contexts:

| # | Context | Phase | Notes |
|---|---|---|---|
| 1 | BUILD (cloned source) | `preinstall` | npm's documented lifecycle order |
| 2 | BUILD | `install` | |
| 3 | BUILD | `postinstall` | |
| 4 | BUILD | `preprepare` | |
| 5 | BUILD | `prepare` | lifecycle slot |
| 6 | BUILD | `postprepare` | |
| 7 | BUILD | `prepare` | **extra** prepare invocation as the prepared tree is finalized |
| 8 | FINAL (consumer `node_modules`) | `preinstall` | |
| 9 | FINAL | `install` | |
| 10 | FINAL | `postinstall` | |

Two facts dominate the design:

1. **Preparation runs the full lifecycle once, plus one extra `prepare`, in the
   BUILD context** — not merely a single `prepare` script.
2. **The FINAL consumer context runs only `preinstall`/`install`/`postinstall`.
   The `prepare` family does not re-run on the installed (already-prepared)
   tree.** This is what makes prepare-generated output the contract that ships
   to consumers.

The extra BUILD-context `prepare` (row 7) is treated as an npm implementation
detail; the oracle asserts the six canonical phases run in order and that the
FINAL context omits the prepare family, but it does not assert the exact count
of BUILD-context `prepare` invocations beyond "at least the canonical one".

## 4. Build-time dependency visibility table

Visibility is recorded per phase by a live `require()` probe inside `record.js`.

| Context | Phase | `regulartool` (runtime) | `devtool` (dev) |
|---|---|---|---|
| BUILD | `preinstall` | yes | **yes** |
| BUILD | `install` | yes | **yes** |
| BUILD | `postinstall` | yes | **yes** |
| BUILD | `preprepare` | yes | **yes** |
| BUILD | `prepare` | yes | **yes** |
| BUILD | `postprepare` | yes | **yes** |
| FINAL | `preinstall` | yes | no |
| FINAL | `install` | yes | no |
| FINAL | `postinstall` | yes | no |

Conclusions:

- **Every BUILD-context phase sees dev dependencies.** npm's preparation step
  runs `npm install --include=dev` on the cloned source, so `devDependencies`
  are installed and resolvable throughout preparation, not only inside
  `prepare`.
- **The FINAL consumer install never sees dev dependencies.** They are stripped
  before the prepared tree is linked into the consumer's `node_modules`.
- `dist/built.js` ships `regular:"RT", dev:"DT"`, proving `prepare` itself
  resolved both tools. The consumer can `require("gitpkg")` and read that
  prepared output; the consumer cannot `require("devtool")`.

The committed `vendor/devtool` source travels inside the package (it appears at
`node_modules/gitpkg/vendor/devtool`), but it is **not** installed as a
resolvable dependency — `require("devtool")` from the consumer fails.

## 5. `--ignore-scripts`, failure, rerun, and mutable-reference behavior

### 5.1 `--ignore-scripts` (Case 2)

`npm install --ignore-scripts` exits 0 but:

- runs **no** scripts — no `phases.log` is written;
- ships **no** generated output — `dist/built.js` is absent;
- still places the raw source at `node_modules/gitpkg`;
- leaves the package unusable if it needs prepared output:
  `require("gitpkg")` throws because `index.js` re-exports a missing `./dist`.

Takeaway: `--ignore-scripts` is a source-level, not a prepare-level, switch. BPM
must match this exactly — under `--ignore-scripts`, ship the raw source and run
no prepare.

### 5.2 Prepare failure (Case 4)

A `prepare` that throws exits non-zero, surfaces the original error
(`INTENTIONAL_PREPARE_FAILURE`), and leaves **no** `node_modules/gitpkg`. No
partial generated output reaches the final tree. The install is atomic: a
failed prepare rolls the package back entirely.

Takeaway: prepare executes in an isolated build root; on failure BPM publishes
no prepared image and the package is not materialized.

### 5.3 Unchanged rerun (Case 3)

A second `npm install` with the same lock and cache appends nothing to
`phases.log`. npm does not re-run lifecycle or prepare when the prepared
package is already cached.

Takeaway: prepared images are content-addressed and reusable; a hit must not
re-execute prepare.

### 5.4 Mutable reference pinning (Case 5)

Specifying `#stable` (branch) or `#v1.0.0` (tag) resolves to the underlying
commit and writes the **full 40-character SHA** into
`package-lock.json`'s `resolved` (`git+file://...#<sha>`). The mutable ref is
never retained.

### 5.5 Changed-commit identity (Case 6)

A new commit (REV 2) produces a new lock pin (the new SHA) and ships the new
prepared output (`REV: 2`). The commit identity is the package identity;
changing the prepare script at a new commit changes both the lock pin and the
shipped artifact.

## 6. Differences from BPM's current implementation

Exact references are against commit `21b87fc`.

| Concern | npm (observed) | BPM today | Reference |
|---|---|---|---|
| `prepare` recognized as install-bearing | yes | yes, in metadata only | `src/resolver/mod.rs` (`has_install_script` includes `"prepare"`) |
| `prepare` executed | yes (BUILD context) | yes, when `--git-prepare`/`BPM_GIT_PREPARE=1` is set, via `prepare_git_packages` (`src/lifecycle.rs:501`) running the `PREPARE_PHASES` set (`src/lifecycle.rs:38`); default-off | — |
| Derived key folds phase set | n/a | fixed three-phase | `src/derived/key.rs` `LIFECYCLE_PHASES = [...]`; hashed in derived key |
| Dev dependency visibility during prepare | full | full during prepare, absent from final graph | `workspace_metadata` copies regular/optional/peer only in final graph |
| Immutable commit pinning | always full SHA | yes — branch/tag/sha resolved to a full SHA at resolution time (`resolve_git_commit`, `src/resolver/sources.rs`); lock stores `resolved_commit` (`src/lockfile.rs:122`) | — |
| Mutable-ref resolution | ref → SHA at resolve time | ref → SHA at resolve time (same as npm) | — |
| Extra `prepare` invocation on finalization | yes | n/a (no prepare on finalization) | — |
| Atomic rollback on prepare failure | yes | yes (prepare failure publishes no image, install fails) | — |

The two structural gaps identified earlier are now closed behind the `--git-prepare` flag:
(a) lifecycle now supports the `PREPARE_PHASES` set (`src/lifecycle.rs:38`) for
Git preparation, and (b) the lock records a resolved immutable SHA (`resolved_commit`,
`src/lockfile.rs:122`) rather than the user reference. The default-on decision
remains open; see Slice 6 in §10.

## 7. Security and integrity implications

- **Immutable commit pinning is a security boundary.** A mutable branch/tag can
  move between resolution and a later frozen install, silently changing the
  bytes that get built. Resolving every Git ref to a 40-character SHA at
  resolution time and storing that SHA in lock identity is required before any
  prepare output can be trusted.
- **The source image must remain pristine.** npm writes generated files into the
  cloned source, but BPM's immutable store must not mutate the fetched source
  artifact. Prepared output is a **separate** object; the source artifact is
  read-only input to its key.
- **Transient dev dependencies must not leak.** Dev tooling is available only
  during preparation and must never appear in the consumer's resolvable
  `node_modules`. The prepared image must exclude the injected `node_modules`.
- **Script policy is part of identity.** `--ignore-scripts` skips prepare and
  ships raw source. A prepared image created under one script policy must never
  satisfy a lookup made under a different policy (see §8.6).
- **Prepare failure is total.** No partial output is published; the install
  fails atomically. This prevents a half-prepared package from being linked.
- **No remote publication yet.** Prepared images are local, content-addressed
  objects. Plan 006 explicitly excludes derived/prepared output until the key
  and portability contract here has shipped and been reviewed.

## 8. Selected BPM architecture

The architecture below preserves BPM's invariants: immutable source artifacts,
content-addressed derived/prepared output, deterministic lock identity, and
exact `--ignore-scripts` semantics.

### 8.1 Resolve mutable Git refs to immutable commits at resolution time

`resolve_git_source` (`src/resolver/mod.rs:1234`) resolves every Git source to
an immutable commit before recording it. The lock schema gains a resolved
commit field:

```text
LockSource::Git { url, reference, resolved_commit: String }
```

`reference` retains the user's original request (for diagnostics and
re-resolution); `resolved_commit` is the authoritative 40-hex SHA. Lock
identity, frozen drift, and cache keys use `resolved_commit`. This is a lock
schema addition with round-trip tests (slice 1).

### 8.2 Keep the fetched source artifact pristine

The hosted/git-archive tarball remains the immutable source artifact. Prepare
never writes into it. It is an input to the prepared-image key.

### 8.3 Build a transient preparation graph containing regular + dev dependencies

A new builder constructs a **transient preparation closure** for the Git
package: its `dependencies`, `optionalDependencies`, `peerDependencies`, **and
`devDependencies`**, resolved against the same registry/workspace machinery.
This graph exists only to run prepare; it is never merged into the final
project graph and never materialized into the consumer's `node_modules`.

### 8.4 Execute the oracle-defined prepare sequence in an isolated build root

Prepare runs in a disposable build root populated from the pristine source
artifact, with the transient preparation closure linked under a private
`node_modules`. The phase order is the oracle order: `preinstall`, `install`,
`postinstall`, `preprepare`, `prepare`, `postprepare`. (The extra finalization
`prepare` npm emits is an npm-internal step; BPM runs the canonical lifecycle
sequence and produces the same distributable output.) A failure publishes
nothing and fails the install atomically (§5.2).

### 8.5 Snapshot only the package's own prepared tree

After prepare completes, snapshot **only the Git package's own tree** (the
pristine files plus prepare-generated output such as `dist/`), explicitly
excluding the injected `node_modules`. This snapshot is the prepared image the
consumer links against.

### 8.6 Key the prepared image by every build-visible input

A new versioned key domain `bpm-prepared-source-v1` hashes, in canonical
order:

- source artifact digest (SHA-512 of the git-archive tarball);
- `resolved_commit` (the immutable pin from §8.1);
- transient preparation closure digest (regular + dev, per §8.3);
- target descriptor (`os`, `architecture`, `family`, `abi`);
- runtime identity (canonicalized executable digest, version, modules ABI,
  N-API version);
- **ordered** phase names and their exact commands;
- bounded environment (the same env-clear + allowlist contract as today);
- runner version and a policy version that folds the script-policy
  (`--ignore-scripts` vs default) so a raw-source lookup can never collide with
  a prepared-image lookup.

A cache hit reuses the prepared image and never re-executes prepare (§5.3).

### 8.7 Feed the prepared image into final materialization

Final graph materialization treats the prepared image as the package's source.
The FINAL-context install phases (`preinstall`, `install`, `postinstall`) run
against it, with only runtime dependencies visible (§4). The prepare family
does not re-run (§3, row 8–10).

### 8.8 `--ignore-scripts` follows oracle behavior

Under `--ignore-scripts`, BPM **skips prepare entirely and ships the raw
source**, matching §5.1 exactly. The script-policy field in the key (§8.6) makes
this safe: a raw-source materialization and a prepared materialization have
distinct identity and never satisfy each other's lookups.

### 8.9 Composition with the current `DerivedStore`

The prepared image is a **separate object kind** from `DerivedStore` lifecycle
output, not a fourth phase bolted onto the existing derived store:

- **Different role.** `DerivedStore` caches post-extract lifecycle output for an
  already-installed package. A prepared image is a source-level transform that
  *substitutes for* the immutable source artifact. Conflating them would couple
  two independently-versioned contracts.
- **Different key inputs.** The derived key's `dependency_graph` is the runtime
  closure; the prepared key needs the transient regular+dev closure and the
  resolved commit. Folding both into one struct would force every existing
  derived key to grow new fields and would entangle the actively-evolving
  derived-store work (which this spike is forbidden to modify).
- **GC consequences.** Prepared images are content-addressed and reusable across
  projects for the same commit; they are reclaimed by the same LRU/size policy
  as other store objects but carry distinct metadata so GC can account for them
  separately and so a future remote-cache decision (Plan 006) can opt in
  independently.

### 8.10 Required derived-key model change

Even though prepared images live in their own domain, the spike confirms a
pre-existing weakness: `src/derived/key.rs:12` hard-codes the phase list, so
`src/derived/key.rs:124` hashes a fixed three-phase script set. Any future
feature that changes which phases run (Git prepare being the first) would reuse
stale derived output. Slice 2 therefore:

- replaces the fixed `LIFECYCLE_PHASES` constant in `src/derived/key.rs` with an
  **ordered phase input** passed by the caller;
- bumps the key domain separator (e.g. `bpm-derived-v1` → `bpm-derived-v2`) so
  every existing derived key is invalidated deterministically;
- passes the runtime three-phase list explicitly from `src/lifecycle.rs`, so
  today's derived-store behavior is unchanged in substance while becoming
  phase-input-driven.

This unblocks a later, optional unification of the derived and prepared key
models without forcing it now.

## 9. Rejected alternatives

- **Globally add `prepare` to `LIFECYCLE_PHASES`.** Rejected: it would run
  prepare against the final runtime graph (no dev dependencies), producing
  output npm never produces, and would not isolate the source image.
- **Retain Git dev dependencies in the final project graph.** Rejected: violates
  §4 and npm's final-tree contract; would leak dev tooling into consumer
  resolution.
- **Mutate the immutable source image to hold prepared output.** Rejected:
  breaks the store's content-immutability invariant and corrupts reuse across
  consumers.
- **Run prepare without commit pinning.** Rejected: a mutable ref can move
  between resolution and a frozen rerun, silently changing built bytes (§7).
- **Key only by package name/version/reference.** Rejected: ignores the
  transient closure, target, runtime, phase order, and script policy, so two
  builds with different dev tooling or policies would collide.
- **Hash the `prepare` script but not its transient dependency closure.**
  Rejected: the closure is build-visible (§4); changing a dev tool version would
  otherwise reuse a stale prepared image.
- **Publish injected `node_modules` inside the prepared image.** Rejected:
  leaks dev dependencies into the consumer tree (§4, §7).
- **Run the prepare family again in the FINAL context.** Rejected: the oracle
  shows the FINAL context runs only install phases (§3); re-running prepare would
  double-build and could diverge from npm.
- **Reuse the existing `DerivedStore` for prepared images without a schema/key
  change.** Rejected: the dependency-closure and phase-set inputs differ, so it
  would key on the wrong graph and wrong phase list.

## 10. Production implementation slices

Each slice is one PR-sized plan with its own acceptance gate and STOP
conditions. The slices are ordered; later slices depend on earlier ones.

### Slice 1 — Immutable Git commit resolution and lock schema

> Status: **Shipped** at `21b87fc` — see `src/resolver/sources.rs` (git commit resolution) and `src/lockfile.rs:122`.

- **Files:** `src/resolver/mod.rs` (`resolve_git_source`), `src/lockfile.rs`
  (`LockSource::Git` serialization + round-trip), resolver tests.
- **Behavior:** resolve branch/tag/sha → 40-hex SHA at resolution time; store
  both `reference` and `resolved_commit`; lock identity uses `resolved_commit`.
- **Tests:** branch/tag/sha all pin to the same SHA; lock byte-stability;
  round-trip through `bpm import`/`bpm ci`; frozen drift unchanged.
- **Verify:** `cargo test -p bpm resolver lockfile && cargo test --test import`.
- **STOP:** if the current lock schema cannot represent `resolved_commit`
  without a breaking migration not anticipated here.

### Slice 2 — Ordered lifecycle phase/key model with old-key invalidation

> Status: **Partially shipped** — `PREPARE_PHASES` exists (`src/lifecycle.rs:38`) and lifecycle passes caller-supplied phase lists; the derived-key domain-separator bump is not confirmed — verify `src/derived/key.rs` before asserting.

- **Files:** `src/derived/key.rs`, `src/lifecycle.rs` (pass explicit phase list),
  derived-key tests.
- **Behavior:** replace the fixed `LIFECYCLE_PHASES` const with an ordered
  caller-supplied phase list; bump the domain separator to invalidate old keys;
  runtime behavior unchanged.
- **Tests:** every existing derived-key invalidation test still passes; a
  changed phase list changes the key; old domain keys are not reused.
- **Verify:** `cargo test derived && cargo test --test lifecycle`.
- **STOP:** if invalidating existing derived keys in the field is unacceptable
  (then gate the new domain behind a flag and migrate separately).

### Slice 3 — Transient regular+dev preparation graph builder

> Status: **Shipped** — `src/resolver/prepare_graph.rs` `build_prepare_closure`, re-exported `src/resolver/mod.rs`.

- **Files:** `src/resolver/prepare_graph.rs`, resolver tests.
- **Behavior:** build the regular+optional+peer+**dev** closure for one Git
  package against the same registry/workspace machinery, without mutating the
  final project graph.
- **Tests:** closure contains devDependencies; final graph still excludes them;
  deterministic across runs.
- **Verify:** `cargo test -p bpm resolver::prepare_graph`.
- **STOP:** if dev-dependency resolution requires semantics the resolver does
  not yet expose.

### Slice 4 — Prepared-image build/snapshot/publish behind an experimental flag

> Status: **Shipped** behind `--git-prepare` — `src/lifecycle.rs:501` `prepare_git_packages`.

- **Files:** prepared-image store/key modules; `src/lifecycle.rs:501` orchestration; integration tests.
- **Behavior:** run the oracle phase order in an isolated build root with the
  transient closure linked, snapshot the package tree excluding injected
  `node_modules`, publish under `bpm-prepared-source-v1`.
- **Tests:** prepare output matches the oracle's `dist/built.js` contract;
  failure publishes nothing; rerun reuses the image; `--ignore-scripts` ships
  raw source and misses the prepared cache.
- **Verify:** `cargo test --test git_prepare_characterization -- --ignored`
  still passes; new prepared-image unit tests pass.
- **STOP:** if the prepared snapshot cannot exclude injected `node_modules`
  with the current store abstraction.

### Slice 5 — Final graph consumption and lifecycle ordering

> Status: **Shipped** behind `--git-prepare` — wired at `src/cli/install.rs:1366` (gated on flag + `!ignore-scripts` + `!direct_materialization`).

- **Files:** `src/lifecycle.rs`, materialization wiring, install orchestration.
- **Behavior:** when a package has a prepared image, materialize it instead of
  the raw source and run only FINAL-context install phases with runtime-only
  visibility.
- **Tests:** consumer sees prepared output and can `require` the package;
  `devtool` is not resolvable from the consumer.
- **Verify:** `cargo test --test install --test lifecycle`.
- **STOP:** if final materialization cannot consume a prepared image without
  duplicating the source attach path.

### Slice 6 — Oracle parity integration tests and default-on decision

> Status: **Partially shipped** — active BPM contract test at `tests/git_prepare_characterization.rs`; npm oracle `#[ignore]`'d, run in CI via `git-prepare-oracle` job (`.github/workflows/ci.yml`); default-on **not** flipped.

- **Files:** `tests/git_prepare_characterization.rs` (promote cases from
  ignored characterization to active parity where deterministic), docs.
- **Behavior:** BPM's installed tree matches npm's for each oracle case; decide
  whether prepare ships default-on or remains flag-gated based on parity
  evidence.
- **Verify:** full `cargo test --workspace`; manual local-registry workflow
  comparing BPM and npm trees.
- **STOP:** if BPM cannot match npm's prepared tree for a supported
  registry-only Git dependency.

## 11. Open questions

None. Every phase-order, dev-dependency, commit-identity, object-kind,
key-input, and ignore-scripts decision is resolved above. Implementation begins
with Slice 1.
