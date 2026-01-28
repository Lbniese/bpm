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
map. Tools not installed on the machine are skipped with a warning rather than
failing the run.

## Baselines

`baselines/` holds machine-stamped baseline files produced by `--save-baseline`.
They are local measurement artifacts — `.gitignore`d by default — and are not
checked in. Regenerate on a given machine with the command above.

## Scenarios

| Scenario | Store | Lockfile | Project view |
|---|---|---|---|
| `true_cold` | empty | absent | absent |
| `resolved_cold` | empty | present | absent |
| `warm_store` | populated | present | absent |
| `repeat_install` | populated | present | present |

## Fixtures

`minimal`, `small`, `medium` — each is a small `package.json` with pinned
direct dependencies; a real `package-lock.json` is generated per run with
`npm install --package-lock-only` so every tool installs from an identical,
integrity-bearing lockfile.