# Transaction concurrency-control benchmark

This suite compares the transaction runtime's compile-time concurrency-control
policies while keeping storage selection a runtime choice. It runs the real
transaction and persistent-GC paths in release-mode tests; it does not add a
production API.

## Running the suite

Run the default quick profile from the repository root:

```console
python3 scripts/transaction-cc-bench.py
```

The quick profile uses a 100 ms warmup, a 500 ms measurement, one repetition,
both backends, all workloads, and worker counts 1, 2, 4, 8, and 16. Requested
worker counts above the host's available parallelism become explicit skipped
cells.

A small two-policy smoke run is useful before a longer comparison:

```console
python3 scripts/transaction-cc-bench.py \
  --policies optimistic,mvcc-optimistic \
  --backends vmemory,file-backed \
  --workloads read-only,hot-key \
  --workers 1,2 --warmup-ms 10 --measure-ms 50
```

For a less noisy run, lengthen both timed phases and repeat every cell:

```console
python3 scripts/transaction-cc-bench.py \
  --warmup-ms 1000 --measure-ms 5000 --repetitions 3
```

`--output-dir`, when supplied, must resolve to a new directory below
`target/transaction-bench/`. Paths containing `..` and symlinks cannot escape
that generated-results boundary.

An already completed run can be revalidated and its derived reports refreshed
with:

```console
python3 scripts/transaction_cc_bench_report.py \
  target/transaction-bench/<run-id>
```

The standalone reporter enforces the same output boundary. Its Python API can
accept other paths for tests and explicitly managed archived data.

## Matrix

The exact policy names are:

- `lockbased`
- `no-wait`
- `optimistic`
- `strict-2pl`
- `timestamp`
- `wait-die`
- `wound-wait`
- `mvcc-optimistic`

The first seven are single-version builds. `mvcc-optimistic` is the only MVCC
build and combines `transaction-mvcc` with optimistic validation. Each Cargo
invocation selects exactly one transaction concurrency-control feature; the
orchestrator validates the mapping before invoking Cargo, and the reporter
validates it again from the Rust driver's records.

The runtime-selectable backend names are `vmemory` and `file-backed`. DAX is
deliberately excluded from the default suite because its setup and host
requirements would make the standard matrix less reproducible.

The four core workloads are:

- `read-only`: deterministic reads from a fixed address plan, validating that
  the observed checksum remains unchanged.
- `disjoint-writes`: workers update non-overlapping granules, measuring scale
  without intentional logical contention.
- `hot-key`: all workers perform read-modify-write operations on shared hot
  state, measuring conflicts and retry cost.
- `write-skew`: paired transactions read two granules initially holding
  `(1,1)`, then each clears a different granule. The serializable invariant is
  that committed state must never be `(0,0)`. Each schedule validates and
  restores `(1,1)` through ordinary transactions.

On a conflict, a worker retries the same logical operation. Retry sleep uses
deterministic jitter in an exponentially increasing interval, starting at 1
microsecond and capped at 64 microseconds; the interval resets after commit.
The configured GC commit interval triggers coalesced maintenance barriers.
Every run also performs warmup and final GC maintenance, and effective elapsed
time includes durable commit, retry backoff, GC barriers, final maintenance,
and drain time. The benchmark calls the selected pluggable persistent-GC
interface: an MVCC-aware collector may prune historical sidecar versions,
while a current-state-only collector need not treat those versions as
object-table roots.

## Core cells and `tfunc` controls

Core cells drive the transaction runtime directly with parallel
`TransactionState` workers. The separately reported one-worker `tfunc`
controls call exported transactional Wasm functions through the normal typed
function boundary. `read-only` controls deterministic loads and `rmw` controls
a transactional increment. They help distinguish core concurrency-control
cost from Wasm call-boundary overhead; they are not additional core matrix
cells and should not be pooled with them.

## Output and validation

Each run is written below `target/transaction-bench/<run-id>/`:

```text
invocation.json            host, revision, toolchain, arguments, elapsed time
orchestrator.jsonl         policy-build lifecycle and failure metadata
requests/<policy>.json     exact request passed to each compiled driver
raw/<policy>.jsonl         authoritative streaming output for one policy
results.jsonl              raw streams merged in requested policy order
results.csv                stable cell, skip, and control rows
summary.md                 comparisons derived from validated JSONL
```

The current schema version is 1. Raw JSONL is authoritative. Reporting rejects
unknown or mixed schemas, failed or incomplete streams, policy/feature
mismatches, invalid attempt identities, duplicate identities, missing matrix
cells, implicit unsupported-worker omissions, and footer count mismatches.
It also requires every requested `tfunc` control and the exact closed Rust
domains for backend, workload, GC mode, integer widths, and phase duration.
Unavailable latency percentiles are empty in CSV and shown as `n/a` in
Markdown; they are never changed to zero.

During report construction, `invocation.json` remains nonterminal with status
`reporting`. A run becomes consumable only after all requested policy streams
and reports finish, the orchestrator writes its single matching completion
footer, and terminal invocation metadata is published as the final lifecycle
write. Consequently the recorded elapsed time includes reporting and the
orchestrator footer. That terminal elapsed value is frozen once and rendered
into Markdown once; the final metadata render is not recursively added to the
duration it displays.

The Markdown report includes core scaling, MVCC-versus-OCC throughput,
file-backed-versus-vmemory throughput, abort and retry behavior, GC and version
statistics, `tfunc` controls, explicit skips, aggregate counts, and actual
elapsed time. Ratios only appear when both matching measurements exist and the
denominator is nonzero.

Only results from the same host, revision, benchmark configuration, and
comparable system load are directly comparable. A dirty run is labeled with
its changed paths and should not be presented as a clean-revision result.

Throughput is never a correctness oracle. A faster cell that violates an
invariant, reports an anomalous conflict, fails validation, or does not finish
is a failed run—not a performance result. Generated result files remain under
`target/transaction-bench/` and must not be committed.
