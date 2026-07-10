# Cross-tool parity data

Machine- and session-specific benchmark artifacts that compare tools on a
controlled footing. Unlike `benchmarks/baselines/reference.json` (the
canonical headline baseline), these files isolate a single variable and are
regenerated on demand; treat the absolute numbers as reproducible on the
capturing host, not as portable targets.

## large-frontend / true_cold — network shape

`large-frontend-true-cold.json` captures each tool's network shape through a
counting HTTP/1.1 reverse proxy (`bpm bench --profile-parity`). Because all
three tools are routed through the same proxy, transport is normalized and the
**ratios** are the trustworthy signal; absolute wall-clock here is NOT
production timing (bpm loses its HTTP/2 advantage to the proxy). Production
timing remains in `reference.json`.

Captured 2026-08-04, **runs=1** (one cold install per tool — see the caveat
below on why multi-run undercounts pnpm).

| tool | requests | MB    | peak concurrent | p95 latency (ms) |
|------|---------:|------:|----------------:|-----------------:|
| npm  |      205 | 40.16 |              15 |              377 |
| pnpm |      116 | 13.50 |              16 |              393 |
| bpm  |      116 | 18.19 |              27 |              272 |

Ratios: **bpm/pnpm = 1.00× requests, 1.35× bytes.** **bpm/npm = 0.57×
requests, 0.45× bytes.**

### Interpretation

- **bpm does NOT over-fetch.** Its request count equals pnpm's (116 each), so
  the cold-path gap is **not** resolver over-fetching. The Step-1 branch
  is therefore **phase overlap / per-request efficiency**, not a resolver
  fetch-policy fix.
- **bpm fetches 1.35× the bytes of pnpm** with the same request count
  (≈157 KB/req vs ≈116 KB/req). The likely cause is bpm fetching fuller
  packuments where pnpm uses the abbreviated `application/vnd.npm.install-v1+json`
  form, or pnpm winning more conditional `304` responses. This is a real,
  secondary lever for 038 (per-request byte efficiency).
- **bpm peak = 27**, matching the baseline's `resolver_peak_http_concurrency: 27`
  exactly. The proxy independently observes the resolver hitting 27 concurrent
  requests, which **confirms the resolver is NOT serialized** and that the
  baseline's divergent `http_peak_concurrency: 7` measures a *different*
  subsystem (the HTTP-client/download layer). The 27-vs-7 gap is benign and
  cross-subsystem, vindicating the resolver characterization.
- npm is the wasteful one: 205 requests / 40 MB (full packuments + extra
  metadata endpoints), ~1.8× bpm's footprint.

### Methodology caveat — pnpm cross-run metadata caching

When the same cell is run with `--runs 3`, pnpm's per-install request count
drops from 116 (run 1) to ≈39/install (total 116 over 3 runs): pnpm keeps a
metadata cache that survives the per-run store isolation, so runs 2–3 are
partially warm. bpm (116/install) and npm (205/install) stay consistent across
runs. **Use `runs=1` for a clean cold-install network comparison.** This also
hints that pnpm's "cold" numbers in the 7-run `reference.json` baseline may be
partially warm — a baseline-methodology note, not a parity-data bug.

## large-frontend / true_cold — lifecycle parity sweep

Captured 2026-08-04 on the development host, 3 runs per cell, same session
(`cargo run --release -- bench --fixture large-frontend --scenario true_cold
--runs 3 --tools npm,pnpm,bpm`, with and without `--ignore-scripts`).

| tool | wall ON (ms) | wall OFF (ms) | Δ (on−off) | bpm `lifecycle` phase ON→OFF |
|------|-------------:|--------------:|-----------:|------------------------------|
| npm  |        15275 |          7569 |       7707 | — (no bpm metrics)           |
| pnpm |         1363 |          1357 |          6 | — (no bpm metrics)           |
| bpm  |         5667 |          4769 |        898 | 402 ms → 0 ms                |

Ratios: **bpm/pnpm = 4.16× (scripts ON) → 3.51× (scripts OFF)**.

### Interpretation

- **pnpm already skips lifecycle by default** (Δ = 6 ms): pnpm 10's
  `onlyBuiltDependencies`/allowBuilds policy runs almost no dependency build
  scripts unless explicitly allowlisted. So the fair bpm-vs-pnpm footing is the
  scripts-OFF column.
- **bpm's lifecycle tax is ~898 ms** (of which the `lifecycle` phase counter
  captures 402 ms; the remainder is related prepare/native-build work). This is
  real but **only ~21% of bpm's excess over pnpm** on this cell
  (898 / (5667 − 1357)).
- The remaining **~79% (~3.4 s) is resolve + materialize**, which is the target
  of Plans 037 (cross-tool network parity profiling) and 038 (cold-path phase
  overlap). Lifecycle parity is therefore a secondary lever, not the dominant
  one.
- npm is dramatically slower with scripts (15.3 s) because it runs every
  dependency's `preinstall`/`install`/`postinstall`.

### Source files

- `large-frontend-true-cold-scripts-on.json` — lifecycle ON (default behavior).
- `large-frontend-true-cold-scripts-off.json` — `--ignore-scripts` for all tools.

Regenerate with `make run ARGS="bench --fixture large-frontend --scenario
true_cold --runs 3 --tools npm,pnpm,bpm --ignore-scripts --json
benchmarks/parity/large-frontend-true-cold-scripts-off.json"` (drop
`--ignore-scripts` for the ON sweep).
