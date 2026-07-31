# Transaction Concurrency-Control Benchmark Design

Date: 2026-07-31

Status: Approved design

## Summary

Add a reproducible release-mode benchmark suite for comparing serializable
MVCC with every existing single-version transaction concurrency-control policy.
The benchmark covers the VMemory and file-backed tmemory backends, four
workloads, and worker counts from one through sixteen when the host has enough
parallelism.

Concurrency control remains selected at compile time, so the suite builds and
runs one exact feature configuration at a time. Storage remains a runtime
selection within each resulting executable. The benchmark exercises the real
transaction state, shared region authority, tmemory backend, durability path,
retry behavior, MVCC pruning, and GC compatibility barrier. It does not add a
public benchmark API or alter production transaction behavior.

The default profile is comprehensive but short enough for routine local use.
It emits authoritative JSONL, flattened CSV, and a generated Markdown summary.
Benchmark artifacts live under `target/` and are not committed.

## Goals

- Compare MVCC with all seven existing single-version concurrency-control
  policies.
- Compare VMemory and file-backed storage under identical logical workloads.
- Measure read overhead, uncontended write scaling, hot-key contention, and
  write-skew prevention.
- Distinguish attempted transaction throughput from committed logical-operation
  throughput.
- Count conflicts, aborts, retries, and retry-inclusive latency.
- Include normal durability, MVCC pruning, and GC-barrier costs in steady-state
  results.
- Fail on correctness violations instead of reporting fast but invalid results.
- Preserve compile-time concurrency-control selection, runtime backend
  selection, and independently pluggable GC behavior.
- Keep the default full run near five to ten minutes on a suitable development
  host while allowing longer and filtered runs.

## Non-goals

- A stable public benchmarking API.
- A new production feature or runtime configuration axis.
- DAX PMEM coverage in the default suite. DAX can be added later as an optional
  backend when suitable hardware is available.
- A performance threshold in normal CI.
- Cross-machine normalization or claims based on results from dissimilar hosts.
- Benchmarking non-serializable snapshot isolation. The MVCC configuration is
  the existing serializable MVCC plus optimistic-validation pairing.
- Changing any concurrency-control, storage, persistence, or GC algorithm to
  make its benchmark result more favorable.

## Configuration Matrix

The core matrix contains eight compile-time policies, two runtime backends,
four workloads, and five requested worker counts. On a host that supports all
worker counts, this produces 320 cells per repetition.

### Compile-time policies

| Report name | Feature selection |
| --- | --- |
| `lockbased` | `transaction-cc-lockbased` |
| `no-wait` | `transaction-cc-nowait-abort` |
| `optimistic` | `transaction-cc-optimistic-validation` |
| `strict-2pl` | `transaction-cc-strict-2pl` |
| `timestamp` | `transaction-cc-timestamp-ordering` |
| `wait-die` | `transaction-cc-wait-die` |
| `wound-wait` | `transaction-cc-wound-wait` |
| `mvcc-optimistic` | `transaction-mvcc` plus `transaction-cc-optimistic-validation` |

Every artifact must report its selected policy. The driver rejects an artifact
that does not match the policy requested by the orchestrator. The existing
compile-time rules continue to require exactly one `transaction-cc-*` feature
and permit `transaction-mvcc` only with optimistic validation.

### Runtime backends

- VMemory.
- File-backed tmemory using the existing copy-on-write durability contract.

Backend initialization and file cleanup are outside measurement. Transactional
data publication, the commit LP, and required durability are inside
measurement.

### Worker counts

The requested counts are `1, 2, 4, 8, 16`. The orchestrator obtains the host's
available parallelism and runs counts that do not exceed it. It always attempts
the one-worker cells. Counts that exceed the host limit are emitted as explicit
`skipped` records with the reason, rather than silently disappearing from the
result set.

## Architecture

### Internal benchmark engine

The benchmark engine is a test-only module within Wasmtime's transaction
runtime. It is compiled by release tests and excluded from normal library
artifacts with `cfg(test)`. This placement gives it narrow access to the real
crate-private transaction machinery without widening the supported public API.

Each worker owns a real `TransactionState`. Workers share one
`TransactionRegionRuntime`, logical region identity, and granule namespace.
They therefore contend through the same selected concurrency authority and, in
MVCC builds, the same version and snapshot authorities used by normal shared
transactions.

The physical `TMemory` instance is shared behind a benchmark-owned storage
adapter. That adapter serializes only operations that require exclusive access
to a backend snapshot or physical installation. It never holds its storage
guard while a transaction performs concurrency-control acquisition,
certification, publication bookkeeping, retry backoff, or metric aggregation.
The timed result includes actual storage access, but a broad harness mutex must
not replace the concurrency behavior being measured.

The engine exposes one ignored release-test entry point. One invocation runs
all requested backends, workloads, and worker counts for its compiled policy,
so the orchestrator pays the compile cost once per policy rather than once per
cell.

### Orchestrator and reporter

A repository script performs these steps:

1. Resolve the requested profile, filters, worker counts, output directory, and
   deterministic seed.
2. Record host, toolchain, git revision, and dirty-state metadata.
3. Build the ignored release-test driver once for each requested exact feature
   configuration.
4. Invoke each driver and validate its policy and schema identity.
5. Preserve each JSONL stream even if a later configuration fails.
6. Merge successful streams into one run-level JSONL file.
7. Generate flattened CSV and Markdown from that authoritative stream.

Each run writes to
`target/transaction-bench/<UTC-timestamp>-<short-revision>/`. The directory
contains the raw per-policy streams, merged JSONL, CSV, Markdown, and the exact
invocation metadata. The existing ignored `target/` tree keeps results out of
version control.

### Wasm call-boundary control

The core matrix directly exercises transaction internals so it can share an
actual VMemory instance across workers without adding a public sharing API. A
small one-worker control separately invokes equivalent read-only and
single-key read/modify/write operations through `tfunc`. It runs for every
policy and both backends.

This control estimates Wasm/host call-boundary overhead by comparison with the
corresponding one-worker core cells. It does not include hot-key contention or
write-skew schedules because those require multiple concurrent workers. Its
measurements are reported separately and never mixed into core scaling tables.

## Workloads

Every worker executes logical operations until the cell deadline. A logical
operation may require several transaction attempts. A concurrency conflict
aborts the current attempt and retries the same logical operation. Successful
attempts advance the worker's operation sequence; aborted attempts do not.

The retry delay starts at one microsecond and doubles after each consecutive
abort to a cap of 64 microseconds. A deterministic per-worker pseudo-random
jitter chooses a delay in the current backoff interval, and the worker sleeps
for that duration. Backoff resets after a successful logical operation. There
is no arbitrary retry-count cutoff; the cell watchdog turns a transaction that
cannot make progress into a benchmark failure.

### Read-only snapshot

Each transaction reads a deterministic set of granules from a stable dataset.
The granule selection sequence is derived from the run seed and worker index.
The transaction performs no writes. A checksum over returned values is checked
against the initialized data.

This workload measures transaction start, access tracking, visibility lookup,
and commit/finish overhead without write publication. Read-only operations are
counted as committed logical operations even when a selected policy implements
their successful finish without a write commit.

### Disjoint writes

Each worker owns a separate granule partition and repeatedly reads and
increments its own counter. No other worker accesses that partition. The
counter delta after measurement must equal that worker's committed logical
operations.

This workload measures uncontended write preparation, certification,
publication, durability, and scaling. Any conflict abort is retained in the
result but also marked anomalous because the logical access sets are disjoint.

### Hot-key read/modify/write

All workers read and increment the same counter. Each logical operation retries
until one increment commits. The final counter delta must equal the aggregate
number of committed logical operations.

This workload measures contention handling, abort cost, retry behavior,
fairness, and lost-update prevention. Backoff is part of the measured logical
operation latency and wall-clock throughput.

### Write-skew schedule

Workers are paired over independent two-granule states. Each pair starts a
schedule with Boolean values `(1, 1)` and the invariant that at least one value
must remain set. One worker owns each value. Both transactions read both values
and propose clearing their own value when the other remains set. A pair-local
barrier ensures both initial attempts finish their reads before either attempts
to commit, creating the classic disjoint-write/read-write cycle.

A serializable implementation must prevent both proposed writes from
committing against the same state. The rejected side retries the same logical
operation against the newly visible state; it may then commit without clearing
its value. A coordinator verifies the invariant and restores `(1, 1)` through
the normal transaction path before the next schedule. Restore attempts and
latency are reported separately from the two measured logical operations.

Worker counts `2, 4, 8, 16` form one, two, four, or eight independent pairs.
The one-worker cell is a sequential control: it executes the two roles in
sequence, must preserve the invariant, and should have no serialization abort.
The benchmark records schedules per second in addition to the common metrics.

## Cell Lifecycle and Timing

Each cell uses a newly created backend and transaction authority. Its lifecycle
is:

1. Initialize deterministic data outside measurement.
2. Release workers together for warmup.
3. Stop warmup, validate state, run a GC barrier, and capture a measurement
   baseline. Warmed backend pages and valid steady-state logical data remain in
   place.
4. Release workers together for the measurement interval.
5. At the common deadline, stop starting new logical operations. Each worker
   finishes its current logical operation.
6. Run the final GC barrier. The effective elapsed time ends when this barrier
   completes. The throughput denominator uses this actual effective elapsed
   time, so neither a long conflict-resolution tail nor final maintenance is
   hidden.
7. Validate state and accounting and emit the cell record.

The default quick profile uses a 100 ms warmup and a 500 ms measurement
interval, with one repetition. If all 320 cells are eligible, this contributes
about 3.2 minutes of scheduled warmup and measurement before compilation,
process, validation, and reporting overhead. Command-line or environment
options can change warmup, measurement duration, repetitions, policy filters,
backend filters, workloads, worker counts, seed, and output path.

Write workloads request a coordinated GC compatibility barrier after every
4,096 successful write commits across the cell and once at cell completion.
Read-only workloads run it after warmup and at cell completion. A worker that
crosses the threshold requests the barrier; the coordinator prevents new
attempts, waits for active attempts to finish, invokes the existing configured
GC interface, and releases the workers.

The selected persistent object collector remains independent of MVCC. In an
MVCC build, a current-state-only collector enters through the existing MVCC
compatibility barrier; historical object versions remain sidecar entries and
are pruned by MVCC maintenance, not traced as first-class objects. In a
non-MVCC build, the same barrier protocol may perform no MVCC work. Barrier
wall time is included in effective cell time and also reported separately.

## Metrics

Each completed cell reports:

- requested and effective elapsed time;
- attempted transactions per second;
- committed logical operations per second;
- successful-attempt, conflict-abort, retry, and unexpected-error counts;
- abort rate and retries per committed logical operation;
- retry-inclusive logical-operation p50, p95, and p99 latency;
- per-worker throughput, minimum/maximum ratio, and coefficient of variation;
- scaling relative to the same policy, backend, and workload at one worker;
- GC-barrier count, aggregate time, maximum pause, and percentage of effective
  elapsed time;
- MVCC versions created, opportunistically pruned, and barrier-pruned when the
  selected build exposes those counters;
- write-skew schedules per second and restore statistics for that workload.

Attempt accounting obeys:

```text
attempts = successful attempts + conflict aborts
committed logical operations = successful measured operations
retries = attempts - committed logical operations
```

Warmup, restore, and measured counters occupy separate fields and never enter
one another's identities. Unexpected errors fail the cell and are not folded
into conflict aborts.

Latency histograms are maintained per worker and merged after the timed work.
The recorder must not place a shared lock on the per-operation hot path. A cell
with too few observations to support a percentile retains its count and marks
that percentile unavailable instead of inventing a value.

## Output and Comparison Report

JSONL is the authoritative schema-versioned output. It contains run metadata,
one record for every completed or skipped cell, failure records when a policy
run terminates early, and a final completeness record. Each cell includes the
compiled feature identity, runtime backend, workload parameters, worker count,
seed, repetition, selected persistent-GC policy and MVCC compatibility mode,
and metrics.

CSV flattens completed and skipped cells for external analysis. Markdown
contains:

- committed throughput by policy and worker count;
- p50/p95/p99 logical-operation latency;
- abort and retry behavior under hot-key and write-skew contention;
- one-worker overhead and multi-worker scaling;
- VMemory versus file-backed deltas;
- MVCC plus OCC versus single-version OCC deltas, isolating the cost and benefit
  of versioning as closely as this matrix permits;
- GC and version-pruning overhead;
- the separate `tfunc` control comparison;
- skipped cells, failures, host identity, revision, dirty state, and invocation.

The reporter checks that JSONL aggregates reproduce CSV and Markdown counts.
It does not compare absolute numbers from different host identities unless the
reader explicitly chooses to do so.

## Error Handling and Watchdogs

- A driver rejects invalid durations, repetitions, worker counts, workload
  names, backends, or schema versions before starting timed work.
- A requested policy mismatch is a hard error.
- Worker panics and unexpected transaction errors are captured with cell and
  worker context, all peers are asked to stop, and the driver exits nonzero.
- Each cell has a watchdog derived from its warmup and measurement durations,
  with a fixed grace period for final in-flight work. Exceeding it reports the
  last known phase and worker progress and fails the run.
- An invariant violation immediately fails the cell. In particular, a
  write-skew state of `(0, 0)` is never summarized as performance data.
- File-backed temporary directories are unique per cell and are cleaned after
  handles close. Cleanup failures are reported without overwriting the raw
  result stream.
- The orchestrator preserves partial per-policy JSONL if compilation or a later
  policy run fails. It does not generate a success footer for an incomplete
  matrix.

## Testing Strategy

Normal deterministic tests cover:

- configuration parsing and matrix expansion;
- host-capacity filtering and explicit skipped records;
- deterministic key and jitter sequences;
- retry and attempt accounting;
- histogram merging and unavailable-percentile handling;
- JSONL schema serialization and parsing;
- CSV and Markdown aggregation from fixture data;
- read checksum, disjoint counter, hot-key counter, and write-skew invariants;
- timeout, worker-panic, and policy-mismatch propagation;
- the write-skew checker rejecting a synthetic `(0, 0)` state.

Short smoke tests run the internal engine against VMemory and file-backed
storage. Feature-matrix verification builds and invokes the ignored driver for
all seven single-version policies and MVCC plus OCC with tiny durations. These
tests check execution and correctness, not relative speed.

The full quick benchmark is explicit and is not part of ordinary `cargo test`.
No throughput or latency threshold enters CI because scheduler noise, storage,
CPU frequency, and host topology make such thresholds unreliable.

Before committing the implementation, verification must also rerun the existing
transaction unit and persistence suites for the affected feature combinations,
including normal non-MVCC builds.

## Acceptance Criteria

- Normal production builds do not compile the benchmark engine and expose no
  new public benchmark API.
- Existing non-MVCC workloads and feature selection retain their behavior.
- All seven single-version policies and MVCC plus OCC build and execute as
  separate configurations.
- VMemory and file-backed cells produce internally consistent output. DAX is
  not required.
- The full core matrix covers all eligible combinations of eight policies, two
  backends, four workloads, and five requested worker counts.
- Conflict aborts retry the same logical operation with the specified bounded
  exponential backoff.
- Hot-key results cannot contain lost updates.
- Write-skew results never violate the at-least-one-set invariant.
- The configured GC barrier runs during the suite, MVCC history remains behind
  sidecar version tables, and available version-pruning statistics are
  reported.
- JSONL, CSV, and Markdown agree on configuration identity and aggregate counts.
- The default profile is designed for a five-to-ten-minute local run and records
  actual total elapsed time rather than treating that target as a correctness
  assertion.
- Benchmark outputs remain untracked under `target/`.

## Expected Repository Surface

Implementation should remain limited to:

- focused test-only benchmark modules under the transaction runtime;
- an ignored release-test entry point;
- a repository orchestration/reporting script;
- deterministic unit and smoke tests;
- documentation for running and interpreting the suite.

Production transaction algorithms may gain `cfg(test)` observation hooks for
otherwise unavailable benchmark counters. They must not gain benchmark-only
branches in normal execution. Unrelated refactoring is outside this work.
