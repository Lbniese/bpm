# benchmarks/

The benchmark harness lives in `src/bench.rs` and runs via `bpm bench`. It
measures bpm's install performance across repeatable scenarios. It is a
measurement tool, not a ranking: it will run any tool manager present on PATH
(`npm`, `pnpm`, `bpm`) and record timings, but it does not compare or market
tools against one another.

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
```

Results record the exact toolchain versions (`node`, `npm`, `pnpm`, `bpm`) under
`versions`, so a result is only comparable to another with the same versions
map. Each tool receives an isolated temporary cache root; warm scenarios reuse
that root, while cold scenarios cannot accidentally benefit from the
operator's global cache. Tools not installed on the machine are skipped with a
warning rather than failing the run.

## Baselines

`baselines/` holds machine-stamped baseline files produced by `--save-baseline`.
Machine-stamped files are local measurement artifacts — `.gitignore`d — while
`baselines/reference.json` is the curated, **checked-in** reference baseline
(the `.gitignore` explicitly exempts it). Regenerate the reference cells on a
given machine with the command above and copy the result into
`reference.json` when the materialization or lifecycle strategy changes.

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

Cold samples receive a fresh project, package-manager cache, and BPM store;
repeated samples therefore remain cold instead of silently becoming warm.

## Fixtures

`minimal`, `small`, and `medium` are small dependency graphs. The M7
comparison set uses `large-frontend`, `many-small-files`, `monorepo`, and
`native-addon` to expose frontend, filesystem, workspace, and native-addon
behavior. `lifecycle` remains a lifecycle-focused correctness fixture; list
all fixtures with `bpm bench --list`. A real `package-lock.json` is generated
per run where the selected tool needs one, so every tool installs from an
identical, integrity-bearing lockfile.