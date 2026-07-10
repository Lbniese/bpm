# Profiling

The benchmark harness records toolchain and machine metadata in every result.
For a CPU profile, build the release binary and run:

```sh
cargo build --release
sample target/release/bpm bench --fixture minimal --scenario repeat_install --tools bpm --runs 3 --json /tmp/bpm-bench.json
```

On Linux, replace `sample` with `perf record --call-graph dwarf` and produce
a flamegraph with the locally installed `inferno` tools.

`reference-folded.txt` is a compact checked-in folded-stack sample for the hot
`minimal/repeat_install/bpm` path. Keep full raw profiles machine-local; the
reproducible timing contract lives in `benchmarks/baselines/reference.json`.
