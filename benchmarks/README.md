# benchmarks/

The benchmark harness lives in `src/bench.rs` and runs via `bpm bench`. It
measures install performance across repeatable scenarios. It is a measurement
tool, not a broad ranking: ordinary runs record each requested manager, while
the opt-in superiority gate makes only an explicitly named, same-protocol cell
claimable.

## Running

```bash
# List available scenarios and fixtures:
bpm bench --list

# Run all scenarios for the minimal fixture:
bpm bench --fixture minimal --runs 3

# Run one scenario, measuring whichever tools are installed:
bpm bench --fixture minimal --scenario resolved_cold --tools npm,bpm

# Write a machine-stamped baseline JSON to <dir>/<machine>-<yyyymmdd>.json:
bpm bench --fixture minimal --save-baseline benchmarks/baselines

# Strict, paired BPM-vs-pnpm gate for one comparable cell:
bpm bench --fixture large-frontend --scenario true_cold --runs 7 \
  --tools pnpm,bpm --require-tools --ignore-scripts \
  --require-faster-than pnpm --max-median-ratio 0.95 --max-p95-ratio 1.0
```

The strict gate is deliberately explicit: it requires seven rounds, requested
and available BPM/pnpm tools, `--require-tools`, scripts-off lifecycle policy,
and no parity proxy. Scripts-off is required because BPM preserves npm's
scripts-on default while pnpm's default lifecycle policy differs; the gate
normalizes that policy without changing normal `bpm install`. It pairs the
logical round samples and checks both the median and p95 `bpm_ms / pnpm_ms`
ratios. Passing proves only the named
fixture/scenario/protocol cell; it is not a general superiority claim.

Results record the exact toolchain versions under `versions`. This includes
`node` and each scored tool, plus `npm` whenever the harness invokes npm for BPM
lock setup or untimed fixture smoke validation—even when npm is not a scored
tool. Every logical sample receives a private `HOME`, XDG cache/config/data/
state roots, `PNPM_HOME`, an empty user `.npmrc`, an empty per-sample global
`.npmrc`, and tool-specific cache paths. Each benchmark project gets an
explicit public-registry `.npmrc` for normal samples or that sample's loopback
proxy registry for parity samples. Cold samples use a fresh cache below that
sample home; warm/hot samples use an isolated shared cache and store root for
that tool only, while retaining the private home/config roots. Inherited
package-manager config and registry/auth overrides are removed by name (without
reading their values). Every normal sample command—setup, seed, timed install,
untimed fixture smoke, and the untimed pnpm build-policy `--version` probe—also
removes every inherited, case-insensitive `COREPACK_*` name, including env-file,
registry, auth, policy, and network overrides. Each receives only a private
`COREPACK_HOME` under that sample/tool cache lifetime, the controlled public
`COREPACK_NPM_REGISTRY`, and `COREPACK_ENV_FILE=0`. Cold Corepack state is fresh
per sample; warm/hot state is shared only within that tool's isolated cache.
PATH, loader, and proxy variables remain available. The process environment is
not cleared and the operator's home config is never read. The fixture project
`.npmrc` remains authoritative for package downloads; the Corepack registry is
only for obtaining the package-manager binary. Availability and version
provenance are captured before timed samples with the same kind of temporary
private probe root, including a private project cwd, empty user/global npmrc
files, controlled public registry, and isolated caches.
Before timing each production pnpm sample, the harness resolves `pnpm
--version` again in the prepared fixture cwd with that sample environment and
requires an exact match with the pre-timing `versions.pnpm` provenance value.
Missing or mismatched resolution fails closed before setup, seed, or timed
install. This check applies to direct pnpm installations as well as Corepack
shims; it does not move version probing into the timed interval. Tools not
installed on the machine are skipped with a warning rather than failing a
permissive run.

## Comparable protocol

New result JSON contains a `protocol` object with protocol version `1`,
`per-sample-home-v1` isolation, an explicit `scripts-on` or `scripts-off`
lifecycle policy, `round-robin-rotated-v1` execution, `fixture-smoke-v1`
post-install validation, the order of every round, and whether parity
instrumentation was enabled. The harness runs one independently prepared sample
per tool per round and rotates the requested order deterministically:

```text
[npm, pnpm, bpm]
[pnpm, bpm, npm]
[bpm, npm, pnpm]
```

Warm/hot scenarios use a separate shared store per tool; cold scenarios use a
fresh store and home for every sample. After each successful timed install, a
fixture's `smoke` script runs untimed with the same isolated environment. Smoke
failure invalidates the sample and is never added to wall-clock statistics.

With `--profile-parity`, `ToolResults.network_samples` stores one proxy shape
per logical sample and `network` remains a raw-record aggregate for older
consumers. Each sample owns a fresh proxy. Finishing a sample closes acceptance,
joins or deterministically cancels its accepted connection tasks, waits for
response-body finalization, and returns that proxy's records only; an incomplete
drain fails closed rather than leaking or omitting a delayed request. Strict
true-cold parity runs require exactly `--runs` nonzero samples per tool and
reject any sample below 90% of that tool's median request count. The proxy is
network-shape instrumentation only: it uses HTTP/1.1 and must not be used for
production wall-clock superiority claims.

Old protocol-less files, including `baselines/reference.json`, remain readable
historical narrative. They are not cross-tool superiority evidence and are
rejected by the strict gate rather than silently upgraded.

## Baselines

There are two distinct kinds of baseline, with different purposes:

- **`baselines/reference.json`** — the curated, **checked-in** reference
  baseline (the `.gitignore` explicitly exempts it). It is a same-machine
  historical/product narrative: a fixed record of past measurements used to
  tell the performance story over time. It is **not** a portable CI gate,
  because strict comparison requires exact system and version equality and the
  reference was recorded on a different machine than CI runners.
- **Manual CI regression baseline** — a manually dispatched
  `benchmark-baseline` workflow job that dynamically benchmarks a selected
  `baseline_ref` (default `HEAD^`) and the current commit **on the same
  runner**, BPM only, then enforces the regression envelope. Comparing two
  builds on one host is what makes the gate meaningful.

`baselines/` also holds machine-stamped files produced by `--save-baseline`
(`.gitignore`d); regenerate the reference cells on a given machine with the
command above and copy the result into `reference.json` when the
materialization or lifecycle strategy changes.

### Strict vs informational comparison

The benchmark comparator accepts a BPM version difference between baseline
and current (comparing two BPM builds is the gate's purpose), but still
requires the same `machine`, `operating_system`, `kernel`, and identical
non-BPM runtime versions (`node`/`npm`/`pnpm`). Any other mismatch is a strict
error. **Informational** comparison (`--baseline-informational` or the
workflow's `informational_baseline` input) reports comparison rows without
gating on environment mismatch or ratio excess; strict comparison fails on
both.

### Choosing a baseline ref

The default `HEAD^` compares against the previous commit. For a longer-range
comparison choose a stable release tag or a known-good commit (for example
`v0.2.0` or a full SHA). The baseline ref must contain a compatible `bpm
bench` interface; if the harness CLI has changed, pick a more recent baseline
or the comparison step will report a clear error rather than silently
substituting the current binary or the historical reference.

## bpm metrics

For `bpm` runs the harness passes `--json-metrics` during the timed install and
folds the result into each tool entry's `bpm_metrics`: `requests_sent` (median /
p95 outbound registry requests per run) and `phase_ms` (median / p95 summed
duration per named phase — `dependency_resolution`, `artifact_download`,
`artifact_extract`, `integrity_verify`, …). Other tools omit `bpm_metrics`. This
makes cold-path request counts and resolver/download/extract phase breakdowns
reproducible from the JSON alone, without a separate profiling run.

## Scenarios

| Scenario | Store | Lockfile | Project view |
|---|---|---|---|
| `true_cold` | empty | absent | absent |
| `resolved_cold` | empty | present | absent |
| `warm_store` | populated | present | absent |
| `repeat_install` | populated | present | present |
| `second_project_same_graph` | populated | present | second project |
| `partial_dependency_change` | populated | present | one dependency changed |
| `monorepo_cold` | empty | present | workspace-style |
| `monorepo_incremental` | populated | present | workspace change |

Cold samples receive a fresh project, private home/XDG roots, package-manager
cache, and store; repeated samples therefore remain cold instead of silently
becoming warm. Warm/hot caches and stores are shared only within the same tool,
never across npm, pnpm, BPM, or another requested manager; their home/config
roots remain private per sample.

## Fixtures

`minimal`, `small`, and `medium` are small dependency graphs. The M7
comparison set uses `large-frontend`, `many-small-files`, `monorepo`, and
`native-addon` to expose frontend, filesystem, workspace, and native-addon
behavior. `lifecycle` remains a lifecycle-focused correctness fixture; list
all fixtures with `bpm bench --list`. A real `package-lock.json` is generated
per run where the selected tool needs one, so every tool installs from an
identical, integrity-bearing lockfile.