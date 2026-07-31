# Transaction Concurrency-Control Benchmark Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build and run a reproducible release-mode suite that compares serializable MVCC plus OCC with all seven single-version transaction concurrency-control builds over VMemory and file-backed tmemory.

**Architecture:** Add a `cfg(test)` benchmark package under the transaction runtime. Its core workers own real `TransactionState` values and share a real `TransactionRegionRuntime` and `TMemory` through a narrow storage adapter; a test-only tmemory commit helper composes the same concurrency, durability, MVCC certification, publication, and cleanup primitives used by the runtime. A Python standard-library orchestrator compiles one exact CC feature set at a time, invokes one ignored release-test driver per policy, preserves JSONL, and generates CSV and Markdown.

**Tech Stack:** Rust 2024, Wasmtime crate-private transaction and tmemory APIs, `serde`/`serde_json`, Rust standard threads and synchronization, Python 3 standard library (`argparse`, `csv`, `json`, `subprocess`, `unittest`).

## Global Constraints

- Keep the benchmark engine behind `cfg(test)`; do not add a public API or a production feature.
- Keep exactly one compile-time `transaction-cc-*` selection. MVCC is tested only as `transaction-mvcc,transaction-cc-optimistic-validation`.
- Benchmark all seven single-version policies plus MVCC+OCC.
- Benchmark VMemory and file-backed tmemory. Exclude DAX from the default suite.
- Request worker counts `1,2,4,8,16`; emit explicit skipped cells above `std::thread::available_parallelism()`.
- Implement read-only snapshot, disjoint writes, hot-key RMW, and forced write-skew workloads.
- Retry the same logical operation after a conflict. Sleep with deterministic jitter in an exponential interval from 1 microsecond through a 64-microsecond cap; reset after commit.
- The quick profile is 100 ms warmup, 500 ms measurement, and one repetition.
- Include durable commit, retry backoff, periodic GC barriers, final GC maintenance, and drain time in effective elapsed time.
- Invoke the existing pluggable persistent-GC interface; historical MVCC values remain sidecar data and are not object-table roots.
- JSONL is authoritative. CSV and Markdown must be derived from it and must agree with its configuration and counts.
- Store generated runs only below `target/transaction-bench/`; never commit benchmark result files.
- Performance values are not CI pass/fail thresholds. Correctness, schema, feature identity, and completion are pass/fail conditions.
- Preserve normal non-MVCC behavior and the existing runtime-selectable storage architecture.
- Do not open, comment on, or review a Wasmtime pull request or issue. Do not add an AI agent, model, or tool as a commit co-author.

## Complete Feature Closure

Every non-default CC release-test command uses this baseline with
`--no-default-features`:

```text
anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,
parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,
debug-builtins,runtime,component-model,component-model-async,threads,
stack-switching,std,debug,compile-time-builtins,wit-parser
```

Append exactly one CC feature for a single-version build. Append
`transaction-mvcc,transaction-cc-optimistic-validation` for MVCC.

## File Structure

- Modify `crates/wasmtime/Cargo.toml`: make `serde_json` available to test-only benchmark code.
- Modify `crates/wasmtime/src/runtime/transaction/mod.rs`: wire the test-only benchmark package.
- Create `crates/wasmtime/src/runtime/transaction/benchmark/mod.rs`: ignored driver, module exports, and end-to-end smoke tests.
- Create `crates/wasmtime/src/runtime/transaction/benchmark/config.rs`: policy identity, request parsing, validation, and matrix expansion.
- Create `crates/wasmtime/src/runtime/transaction/benchmark/record.rs`: schema-versioned JSONL record types and writer.
- Create `crates/wasmtime/src/runtime/transaction/benchmark/metrics.rs`: attempt counters, latency histogram, percentiles, fairness, and merges.
- Create `crates/wasmtime/src/runtime/transaction/benchmark/storage.rs`: shared real tmemory, transaction-attempt adapter, durability setup, conflict cleanup, and GC barriers.
- Create `crates/wasmtime/src/runtime/transaction/benchmark/workload.rs`: deterministic workload initialization, operations, and invariants.
- Create `crates/wasmtime/src/runtime/transaction/benchmark/runner.rs`: worker lifecycle, retry loop, maintenance gate, watchdog, and cell aggregation.
- Create `crates/wasmtime/src/runtime/transaction/benchmark/tfunc.rs`: one-worker Wasm call-boundary controls.
- Modify `crates/wasmtime/src/runtime/transaction/state.rs`: add a test-only, tmemory-only terminal commit helper.
- Modify `crates/wasmtime/src/runtime/transaction/region_runtime.rs`: expose the selected GC compatibility mode to tests.
- Modify `crates/wasmtime/src/runtime/transaction/mvcc/domains.rs`: expose total resident-version count to tests.
- Modify `crates/wasmtime/src/runtime/transaction/visibility.rs`: count opportunistically pruned versions in test builds.
- Create `scripts/transaction-cc-bench.py`: feature-matrix orchestration and raw-run preservation.
- Create `scripts/transaction_cc_bench_report.py`: JSONL validation plus CSV and Markdown generation.
- Create `scripts/tests/test_transaction_cc_bench.py`: Python orchestration tests.
- Create `scripts/tests/test_transaction_cc_bench_report.py`: Python reporter tests.
- Create `docs/shisoft/2026-07-31-transaction-cc-benchmark.md`: command, schema, workload, and interpretation guide.

---

### Task 1: Request, policy identity, matrix, and JSONL schema

**Files:**
- Modify: `crates/wasmtime/Cargo.toml:97-105`
- Modify: `crates/wasmtime/src/runtime/transaction/mod.rs:1-25,470-480`
- Create: `crates/wasmtime/src/runtime/transaction/benchmark/mod.rs`
- Create: `crates/wasmtime/src/runtime/transaction/benchmark/config.rs`
- Create: `crates/wasmtime/src/runtime/transaction/benchmark/metrics.rs`
- Create: `crates/wasmtime/src/runtime/transaction/benchmark/record.rs`

**Interfaces:**
- Produces: `BenchmarkRequest::read(&Path) -> Result<BenchmarkRequest>`
- Produces: `BenchmarkRequest::validate(&self) -> Result<()>`
- Produces: `BenchmarkRequest::cells(&self, usize) -> Vec<MatrixEntry>`
- Produces: `compiled_policy_name() -> &'static str`
- Produces: `compiled_transaction_features() -> &'static [&'static str]`
- Produces: `BenchmarkRecord` tagged with `record_type` and `schema_version = 1`
- Produces: `JsonlWriter::create(&Path) -> Result<JsonlWriter>` and `write(&mut self, &BenchmarkRecord) -> Result<()>`

- [ ] **Step 1: Add failing request-validation and matrix tests**

In `benchmark/config.rs`, define tests before the implementation:

```rust
#[test]
fn quick_request_expands_and_marks_unsupported_workers() {
    let request = BenchmarkRequest::quick(compiled_policy_name().to_string());
    let cells = request.cells(4);
    assert_eq!(cells.len(), 2 * 4 * 5);
    assert!(cells.iter().any(|entry| {
        entry.spec.workers == 16
            && matches!(&entry.disposition, MatrixDisposition::Skipped(reason)
                if reason == "worker count 16 exceeds available parallelism 4")
    }));
}

#[test]
fn request_rejects_a_policy_other_than_the_compiled_policy() {
    let mut request = BenchmarkRequest::quick("definitely-not-this-build".into());
    let error = request.validate().unwrap_err().to_string();
    assert!(error.contains("requested policy"), "{error}");
    request.expected_policy = compiled_policy_name().into();
    request.validate().unwrap();
}
```

In `benchmark/record.rs`, add a JSONL round-trip test that writes a
`BenchmarkRecord::SkippedCell`, reads the line with `serde_json`, and asserts
`record_type == "skipped_cell"`, `schema_version == 1`, and the original cell
identity.

- [ ] **Step 2: Run the focused tests and verify the module is missing**

Run:

```bash
cargo test -p wasmtime transaction::benchmark::config --lib -- --nocapture
```

Expected: FAIL because `transaction::benchmark` and its types are not defined.

- [ ] **Step 3: Add the test-only module and test JSON dependency**

Add this dev dependency to `crates/wasmtime/Cargo.toml`:

```toml
serde_json = { workspace = true }
```

Add this module declaration near the other transaction submodules:

```rust
#[cfg(test)]
mod benchmark;
```

Create `benchmark/mod.rs`:

```rust
mod config;
mod metrics;
mod record;

use crate::prelude::*;
```

Create `metrics.rs` with the serialized `AttemptCounts`, `GcMetrics`,
`VersionMetrics`, `FairnessMetrics`, and `CellMetrics` field definitions used
by `record.rs`. Task 2 adds their calculation and validation methods; the types
are complete data contracts in this task rather than empty scaffolding.

Use this stable payload shape:

```rust
#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct AttemptCounts {
    pub attempts: u64,
    pub successful_attempts: u64,
    pub conflict_aborts: u64,
    pub committed_operations: u64,
    pub retries: u64,
    pub unexpected_errors: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct GcMetrics {
    pub barriers: u64,
    pub total_ns: u64,
    pub max_pause_ns: u64,
    pub elapsed_share: f64,
    pub policy_mode: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct VersionMetrics {
    pub created: u64,
    pub opportunistically_pruned: u64,
    pub barrier_pruned: u64,
    pub resident_after_final_barrier: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct FairnessMetrics {
    pub min_max_ratio: Option<f64>,
    pub coefficient_of_variation: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct RestoreMetrics {
    pub counts: AttemptCounts,
    pub p50_ns: Option<u64>,
    pub p95_ns: Option<u64>,
    pub p99_ns: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct CellMetrics {
    pub requested_elapsed_ns: u64,
    pub effective_elapsed_ns: u64,
    pub attempts_per_second: f64,
    pub committed_operations_per_second: f64,
    pub counts: AttemptCounts,
    pub p50_ns: Option<u64>,
    pub p95_ns: Option<u64>,
    pub p99_ns: Option<u64>,
    pub fairness: FairnessMetrics,
    pub gc: GcMetrics,
    pub versions: VersionMetrics,
    pub schedules_per_second: Option<f64>,
    pub restore: Option<RestoreMetrics>,
    pub anomalous_conflict: bool,
}
```

- [ ] **Step 4: Implement exact request and matrix types**

Use these type names and serialized values in `config.rs`:

```rust
use super::super::ConcurrencyControl;
use crate::prelude::*;
use serde_derive::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum BackendKind { Vmemory, FileBacked }

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum WorkloadKind { ReadOnly, DisjointWrites, HotKey, WriteSkew }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct BenchmarkRequest {
    pub schema_version: u32,
    pub expected_policy: String,
    pub warmup_ms: u64,
    pub measure_ms: u64,
    pub repetitions: u32,
    pub backends: Vec<BackendKind>,
    pub workloads: Vec<WorkloadKind>,
    pub workers: Vec<usize>,
    pub seed: u64,
    pub gc_commit_interval: u64,
    pub include_tfunc_control: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct CellSpec {
    pub policy: String,
    pub compiled_features: Vec<String>,
    pub backend: BackendKind,
    pub workload: WorkloadKind,
    pub workers: usize,
    pub repetition: u32,
    pub seed: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum MatrixDisposition { Run, Skipped(String) }

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MatrixEntry {
    pub spec: CellSpec,
    pub disposition: MatrixDisposition,
}
```

`quick()` sets schema version `1`, durations `100` and `500`, repetition `1`,
both backends, all four workloads,
`[1,2,4,8,16]`, seed `0x5eed_5eed_d15c_a11e`, GC interval `4096`, and enables
the `tfunc` control. `validate()` rejects any schema version other than `1`,
zero durations, repetitions, workers, or GC interval; duplicates; empty axes;
unknown policy identity; and MVCC without the optimistic compiled policy.

Implement `compiled_policy_name()` by returning `"mvcc-optimistic"` when
`cfg!(feature = "transaction-mvcc")`, otherwise map
`ConcurrencyControl::default_for_build()` to the seven names in the design.
Do not infer the policy from the request. `compiled_transaction_features()`
returns only the transaction-selection identity: the one compiled
`transaction-cc-*` feature for a single-version build, or
`["transaction-mvcc", "transaction-cc-optimistic-validation"]` in that exact
order for MVCC. Copy this value into every generated `CellSpec`; the full
baseline feature closure remains captured by orchestrator invocation metadata.

- [ ] **Step 5: Implement the schema and flushing JSONL writer**

In `record.rs`, define:

```rust
pub(super) const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "record_type", rename_all = "snake_case")]
pub(super) enum BenchmarkRecord {
    Driver(DriverRecord),
    Cell(CellRecord),
    TfuncControl(TfuncControlRecord),
    SkippedCell(SkippedCellRecord),
    Failure(FailureRecord),
    Complete(CompleteRecord),
}
```

Use these exact record payloads:

```rust
#[derive(Clone, Debug, Serialize)]
pub(super) struct DriverRecord {
    pub schema_version: u32,
    pub compiled_policy: String,
    pub compiled_features: Vec<String>,
    pub available_parallelism: usize,
    pub gc_policy_mode: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct CellRecord {
    pub schema_version: u32,
    pub spec: CellSpec,
    pub metrics: CellMetrics,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct TfuncControlRecord {
    pub schema_version: u32,
    pub policy: String,
    pub compiled_features: Vec<String>,
    pub backend: BackendKind,
    pub control: String,
    pub repetition: u32,
    pub seed: u64,
    pub metrics: CellMetrics,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct SkippedCellRecord {
    pub schema_version: u32,
    pub spec: CellSpec,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct FailureRecord {
    pub schema_version: u32,
    pub policy: String,
    pub cell: Option<CellSpec>,
    pub phase: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct CompleteRecord {
    pub schema_version: u32,
    pub policy: String,
    pub completed_cells: usize,
    pub skipped_cells: usize,
    pub tfunc_controls: usize,
}
```

`JsonlWriter::write` calls `serde_json::to_writer`, writes one newline, and
flushes after every record so partial runs survive a later failure.

- [ ] **Step 6: Run tests and verify the schema**

Run:

```bash
cargo test -p wasmtime transaction::benchmark::config --lib -- --nocapture
cargo test -p wasmtime transaction::benchmark::record --lib -- --nocapture
```

Expected: PASS, including 40 entries from two backends, four workloads, and
five requested worker counts for one repetition.

- [ ] **Step 7: Commit**

```bash
git add crates/wasmtime/Cargo.toml \
  crates/wasmtime/src/runtime/transaction/mod.rs \
  crates/wasmtime/src/runtime/transaction/benchmark
git commit -m "Add transaction benchmark request schema"
```

---

### Task 2: Metrics, percentiles, accounting, and fairness

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/benchmark/metrics.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/benchmark/record.rs`

**Interfaces:**
- Consumes: `CellRecord` and `TfuncControlRecord` from Task 1
- Produces: `LatencyHistogram::record(Duration)` and `percentile(f64) -> Option<u64>`
- Produces: `WorkerMetrics::record_commit`, `record_conflict`, and `merge`
- Produces: `CellMetrics::from_workers(&[WorkerMetrics], CellAggregateInput) -> Result<CellMetrics>`
- Produces: `AttemptCounts::validate() -> Result<()>`

`CellAggregateInput` contains `requested_elapsed: Duration`,
`effective_elapsed: Duration`, `gc: GcMetrics`, `versions: VersionMetrics`,
`schedules: u64`, `restore: Option<RestoreMetrics>`, and
`anomalous_conflict: bool`.

- [ ] **Step 1: Write failing histogram and accounting tests**

Add these cases to `metrics.rs`:

```rust
#[test]
fn percentile_requires_enough_samples_and_returns_nanoseconds() {
    let mut histogram = LatencyHistogram::default();
    for micros in 1..=100 {
        histogram.record(Duration::from_micros(micros));
    }
    assert!(histogram.percentile(0.50).unwrap() >= 50_000);
    assert!(histogram.percentile(0.95).unwrap() >= 95_000);
    assert!(histogram.percentile(0.99).unwrap() >= 99_000);

    let mut sparse = LatencyHistogram::default();
    sparse.record(Duration::from_nanos(7));
    assert_eq!(sparse.percentile(0.50), None);
}

#[test]
fn attempt_identity_rejects_inconsistent_counts() {
    let valid = AttemptCounts { attempts: 9, successful_attempts: 4,
        conflict_aborts: 5, committed_operations: 4, retries: 5,
        unexpected_errors: 0 };
    valid.validate().unwrap();
    let invalid = AttemptCounts { retries: 4, ..valid };
    assert!(invalid.validate().is_err());
}
```

Add a fairness test with worker commit counts `[100, 100, 50]`; require a
minimum/maximum ratio of `0.5` and a positive coefficient of variation.

- [ ] **Step 2: Run the tests and verify missing types fail**

Run:

```bash
cargo test -p wasmtime transaction::benchmark::metrics --lib -- --nocapture
```

Expected: FAIL because histogram, validation, merge, and fairness methods are
not implemented.

- [ ] **Step 3: Implement the bounded per-worker latency histogram**

Use 64 power-of-two nanosecond buckets and no shared hot-path lock:

```rust
#[derive(Clone, Debug)]
pub(super) struct LatencyHistogram {
    buckets: [u64; 64],
    samples: u64,
}

impl LatencyHistogram {
    pub(super) fn record(&mut self, elapsed: Duration) {
        let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX).max(1);
        let bucket = (63 - nanos.leading_zeros()) as usize;
        self.buckets[bucket] += 1;
        self.samples += 1;
    }
}
```

Return the inclusive bucket upper bound. Require 2 samples for p50, 20 for
p95, and 100 for p99; otherwise return `None`. Implement `Default` manually
because the fixed array is internal and is not serialized.

- [ ] **Step 4: Implement exact counts and aggregate payload calculations**

Implement `WorkerMetrics` and the calculation methods for the payload types
created in Task 1. `AttemptCounts::validate()` enforces:

```text
attempts = successful_attempts + conflict_aborts
committed_operations = successful_attempts
retries = attempts - committed_operations
```

Keep warmup, write-skew restore, and measured metrics in distinct structures.
Compute throughput with the effective elapsed duration. Compute fairness only
when at least one worker committed; a zero maximum produces unavailable
fairness rather than division by zero.

- [ ] **Step 5: Connect metric payloads to serialized records**

Replace the provisional fields in `CellRecord` and `TfuncControlRecord` with
the exact `CellMetrics` types. Derive `Serialize` only for report payloads;
keep `LatencyHistogram` internal.

- [ ] **Step 6: Run focused tests**

Run:

```bash
cargo test -p wasmtime transaction::benchmark::metrics --lib -- --nocapture
cargo test -p wasmtime transaction::benchmark::record --lib -- --nocapture
```

Expected: PASS with the attempt identities and sparse-percentile behavior
covered.

- [ ] **Step 7: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/benchmark/metrics.rs \
  crates/wasmtime/src/runtime/transaction/benchmark/record.rs
git commit -m "Add transaction benchmark metrics"
```

---

### Task 3: Shared tmemory attempt adapter and real GC barrier

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/benchmark/mod.rs`
- Create: `crates/wasmtime/src/runtime/transaction/benchmark/storage.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs:4516-4695`
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs:1280-1320`
- Modify: `crates/wasmtime/src/runtime/transaction/mvcc/domains.rs:80-125,350-380`
- Modify: `crates/wasmtime/src/runtime/transaction/visibility.rs:90-115`

**Interfaces:**
- Produces: `BenchmarkStorage::new(BackendKind) -> Result<Arc<BenchmarkStorage>>`
- Produces: `new_transaction_state(&self) -> Result<TransactionState>`
- Produces: `begin(&self, &mut TransactionState) -> Result<()>`
- Produces: `read_u64(&self, &mut TransactionState, u64) -> Result<u64>`
- Produces: `stage_u64(&self, &mut TransactionState, u64, u64) -> Result<()>`
- Produces: `commit(&self, &mut TransactionState) -> Result<CommitObservation>`
- Produces: `finish_attempt<T>(&self, &mut TransactionState, Result<T>) -> Result<AttemptOutcome<T>>`
- Produces: `abort_if_active(&self, &mut TransactionState) -> Result<()>`
- Produces: `run_gc_barrier(&self) -> Result<GcBarrierObservation>`
- Produces: `TransactionState::commit_single_tmemory_for_benchmark(&mut self, &Mutex<TMemory>) -> Result<bool>` under `cfg(test)`

- [ ] **Step 1: Write failing VMemory, file-backed, conflict, and GC tests**

Add tests in `storage.rs` that:

1. Begin a state, increment address zero, commit, and read `1` from VMemory.
2. Repeat with file-backed storage, flush the mapped backend through the normal
   commit path, and while the adapter still owns its temporary directory assert
   that the tmemory file's first eight bytes contain `1u64.to_le_bytes()`.
   Close the mapped handles, drop the adapter, and separately assert its unique
   temporary directory was removed.
3. Begin two states against one runtime, make both read and stage address zero,
   commit concurrently, and require at least one conflict while the final value
   equals one committed increment.
4. In an MVCC build, create several versions, call `run_gc_barrier()`, require a
   positive removed-version count, and require zero active snapshots afterward.

Use `clear_current_thread_transaction_on_drop_for_test()` in every worker
closure so a panic cannot leak TLS transaction identity into another test.

- [ ] **Step 2: Run the VMemory test and verify it fails**

Run:

```bash
cargo test -p wasmtime transaction::benchmark::storage::tests::vmemory_rmw_commits --lib -- --nocapture
```

Expected: FAIL because `BenchmarkStorage` is not implemented.

- [ ] **Step 3: Implement backend construction and narrow snapshots**

Add `mod storage;` to `benchmark/mod.rs`.

Define:

```rust
pub(super) struct BenchmarkStorage {
    backend: BackendKind,
    runtime: TransactionRegionRuntime,
    tmemory: Mutex<TMemory>,
    file_paths: Option<BenchmarkFilePaths>,
    object_table: Mutex<ObjectTable>,
    mvcc_versions_created: AtomicU64,
}

pub(super) struct CommitObservation {
    pub wrote: bool,
    pub mvcc_versions_created: u64,
}
```

VMemory uses `TransactionRegionRuntime::new_for_test()` and
`TMemory::new_vmemory(1)`. File-backed storage creates a `tempfile::TempDir`,
uses `TransactionRegionRuntime::create_file_backed_for_test(tmemory, log, 4096)`,
and creates `TMemory` from the runtime's shared transaction configuration.
Keep the temp directory alive in `BenchmarkFilePaths`. For every file-backed
worker, `new_transaction_state` starts with `TransactionState::default()`,
clones the runtime's `SharedFileBackedStorageConfig`, calls
`open_shared_file_backed_durable_log`, and attaches the shared runtime before
the first `begin_with_region_runtime`. VMemory workers use a default state and
attach the runtime at begin. This gives each worker a distinct durable-log
stream while retaining the shared allocator and tmemory commit locks.

For reads and stages, lock only long enough to call
`collect_tmemory_access_snapshot`, release the mutex, then call
`read_tmemory_owned_from_snapshot` or
`stage_tmemory_write_owned_from_snapshot`. Use owner `None` and memory index
zero for every worker so all states address the same granules.

- [ ] **Step 4: Add the test-only single-tmemory terminal helper**

Add `commit_single_tmemory_for_benchmark` inside the existing `#[cfg(test)]`
`impl TransactionState`. Pass the tmemory mutex itself; the caller must not
lock it around this method. It must not duplicate a simplified conflict
oracle. Compose the existing methods in this order:

```text
single-version:
  capture current granule versions under a narrow storage lock, then release it
  validate the complete active read and write sets
  enter terminal commit
  reacquire storage only to publish durable undo and install staged tmemory
  complete_commit

MVCC:
  obtain active_mvcc_commit_context
  enter the shared user-transaction region
  acquire full read/write-set MVCC certification
  register one pending commit record
  prepare every staged memory granule from predecessors captured under and
    released from a narrow storage lock
  enter terminal commit
  reacquire storage only to publish durable undo and install the staged value
  publish the pending commit record
  prepare the normal version-bump completion
  finish_mvcc_commit_cleanup
```

On an MVCC certification error, no physical write is allowed. Abort the pending
record when it exists, remove its prepared sidecar entries, abort an active
state, and return the original conflict. For an error after physical install,
use the prepared predecessor and existing durable rollback primitives before
returning the unexpected error. The method returns `true` exactly when at
least one tmemory granule was installed.

Keep this helper `pub(crate)` only inside `cfg(test)`. Add an
equivalence test that commits the same one-granule RMW through this helper and
through a real `tfunc`, then compares the final value for both backends.
Add an MVCC hook test that pauses one transaction inside certification while a
second thread successfully acquires the tmemory mutex. This prevents a storage
guard from accidentally spanning CC acquisition or certification.

For `CommitObservation`, `mvcc_versions_created` is the number of staged
granules whose commit-time sidecar versions were successfully published by
this transaction. Count zero for every single-version build. Do not count a
lazy baseline predecessor materialized for an object's first MVCC read as a
commit-created version, and do not retain counts for prepared entries that are
rolled back after an error.

- [ ] **Step 5: Classify conflicts and guarantee cleanup**

In `storage.rs`, define an internal `AttemptOutcome<T> { Committed(T),
Conflict }`. Classify only the established transaction diagnostics:

```rust
const CONFLICT_MARKERS: &[&str] = &[
    "transaction read conflict",
    "transaction write conflict",
    "transaction conflict would wait",
    "transaction was conflict-aborted",
    "transaction MVCC certification conflict",
];
```

Every classified conflict calls `abort_if_active`; a cleanup failure is
combined with the original error and becomes an unexpected cell failure.
Errors without one of these markers are never counted as conflicts.

- [ ] **Step 6: Add test observations without production behavior changes**

Add `TransactionRegionRuntime::persistent_gc_mode_for_test() -> Result<GcMvccMode>`.
Add `MvccRuntime::total_version_count_for_test() -> Result<usize>` by summing
`VersionChain::version_count()` across all six tables. Add a test-only atomic
counter for versions removed by the opportunistic `prune_versions(chains(8))`
call in `MvccVisibility::finish_snapshot`, plus a snapshot accessor. All new
fields and branches in these runtime modules remain under `cfg(test)`.

Serialize the selected GC mode in benchmark records with an exhaustive match:
`GcMvccMode::CurrentStateOnly => "current-state-only"` and
`GcMvccMode::MvccCompliant => "mvcc-compliant"`. Do not derive the external
value from `Debug` output.

- [ ] **Step 7: Invoke the real pluggable GC entry point**

`run_gc_barrier()` creates an inactive `TransactionState` attached to the
shared runtime and calls:

```rust
state.persistent_gc_maintenance_step(
    &mut *self.object_table.lock().unwrap(),
    PersistentGcBudget::objects(1),
)?;
```

This deliberately uses the existing current-state-only/MVCC-compliant policy
adapter. In MVCC builds it performs the compatibility barrier and full rebase;
in non-MVCC builds it runs the same policy without MVCC history. Measure wall
time, selected mode, resident versions before/after, barrier-removed versions,
and the delta in opportunistic pruning.

- [ ] **Step 8: Run both feature-focused storage suites**

Run:

```bash
cargo test -p wasmtime transaction::benchmark::storage --lib -- --nocapture
cargo test -p wasmtime --no-default-features \
  --features "anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-mvcc,transaction-cc-optimistic-validation" \
  transaction::benchmark::storage --lib -- --nocapture
```

Expected: PASS for VMemory, file-backed persistence, conflict cleanup, real GC
admission, and MVCC version removal.

- [ ] **Step 9: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/benchmark/storage.rs \
  crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/transaction/region_runtime.rs \
  crates/wasmtime/src/runtime/transaction/mvcc/domains.rs \
  crates/wasmtime/src/runtime/transaction/visibility.rs
git commit -m "Add shared tmemory benchmark adapter"
```

---

### Task 4: Deterministic read, disjoint-write, and hot-key workloads

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/benchmark/mod.rs`
- Create: `crates/wasmtime/src/runtime/transaction/benchmark/workload.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/benchmark/storage.rs`

**Interfaces:**
- Consumes: `BackendKind`, `WorkloadKind`, and `BenchmarkStorage`
- Produces: `WorkloadState::initialize(&Arc<BenchmarkStorage>, &CellSpec) -> Result<WorkloadState>`
- Produces: `execute_attempt(&self, &mut WorkerContext, u64) -> Result<AttemptOutcome<OperationEffect>>`
- Produces: `validate(&self, &BenchmarkStorage, &MeasurementBaseline, &AttemptCounts) -> Result<()>`
- Produces: `DeterministicRng::next_u64()` used for addresses and retry jitter

`WorkerContext` is defined here with fields `index: usize`,
`state: TransactionState`, `rng: DeterministicRng`, and `sequence: u64`.
`OperationEffect` contains `checksum: u64`, `writes: u64`, and
`counter_delta: u64`. `MeasurementBaseline` contains the committed counter
values captured after warmup. Task 5 adds timing and metrics around these
workload-level contracts without renaming them.

- [ ] **Step 1: Write failing deterministic and invariant tests**

Add tests in `workload.rs`:

```rust
#[test]
fn deterministic_rng_repeats_for_the_same_worker_seed() {
    let first = (0..16).scan(DeterministicRng::new(7, 3), |rng, _| Some(rng.next_u64()))
        .collect::<Vec<_>>();
    let second = (0..16).scan(DeterministicRng::new(7, 3), |rng, _| Some(rng.next_u64()))
        .collect::<Vec<_>>();
    assert_eq!(first, second);
}

#[test]
fn disjoint_write_validation_rejects_a_missing_increment() {
    let expected = vec![11, 9, 7];
    let actual = vec![11, 8, 7];
    assert!(validate_disjoint_counters(&expected, &actual).is_err());
}

#[test]
fn hot_key_validation_rejects_a_lost_update() {
    assert!(validate_counter_delta(100, 113, 14).is_err());
}
```

Also add a read-only checksum test covering eight deterministic granules.

- [ ] **Step 2: Run the workload tests and verify they fail**

Run:

```bash
cargo test -p wasmtime transaction::benchmark::workload --lib -- --nocapture
```

Expected: FAIL because the workload module is not wired or implemented.

- [ ] **Step 3: Wire the module and implement deterministic layout**

Add `mod workload;` to `benchmark/mod.rs`. Use one 64 KiB tmemory page and
64-byte granules. Reserve:

```text
granules   0..255   read-only dataset
granule       256   hot-key counter
granules 257..272   disjoint worker counters
granules 288..303   write-skew pair values
```

Read and write only the first eight bytes of each granule. Initialize value
`seed.rotate_left(granule_index % 64) ^ granule_index` for the read-only set,
zero for counters, and one for skew values. Initialization uses committed
tmemory writes before workers start and is outside timing.

Implement `DeterministicRng` as xorshift64* with a nonzero seed derived from
`run_seed ^ worker_index.wrapping_mul(0x9e37_79b9_7f4a_7c15)`. Use this exact
generator for both read selection and retry jitter so no `rand` hot-path
dependency is added.

- [ ] **Step 4: Implement transaction attempts for the first three workloads**

Each attempt follows this shape:

```rust
storage.begin(&mut worker.state)?;
let result = match kind {
    WorkloadKind::ReadOnly => read_eight_and_checksum(storage, worker, sequence),
    WorkloadKind::DisjointWrites => increment(storage, worker, disjoint_addr(worker.index)),
    WorkloadKind::HotKey => increment(storage, worker, hot_key_addr()),
    WorkloadKind::WriteSkew => unreachable!("implemented by the paired schedule"),
};
storage.finish_attempt(&mut worker.state, result)
```

`finish_attempt` commits successful work, classifies only known conflicts, and
aborts active state on every error. `OperationEffect` contains checksum delta,
write count, and committed counter delta; it contains no timing fields.

- [ ] **Step 5: Implement post-warmup baselines and validation**

After warmup, validate current state and capture counter values. Do not reset
the backend or MVCC history. Measurement validation uses deltas from that
baseline:

- read-only checksum equals deterministic initialized values;
- every disjoint counter delta equals that worker's measured commits;
- hot-key counter delta equals aggregate measured commits;
- a disjoint conflict remains in output but sets `anomalous_conflict = true`.

- [ ] **Step 6: Run workload tests under default and MVCC builds**

Run:

```bash
cargo test -p wasmtime transaction::benchmark::workload --lib -- --nocapture
cargo test -p wasmtime --no-default-features \
  --features "anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-mvcc,transaction-cc-optimistic-validation" \
  transaction::benchmark::workload --lib -- --nocapture
```

Expected: PASS with identical logical results for both visibility modes.

- [ ] **Step 7: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/benchmark/mod.rs \
  crates/wasmtime/src/runtime/transaction/benchmark/workload.rs \
  crates/wasmtime/src/runtime/transaction/benchmark/storage.rs
git commit -m "Add transaction benchmark workloads"
```

---

### Task 5: Parallel runner, retry loop, GC cadence, watchdog, and write skew

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/benchmark/mod.rs`
- Create: `crates/wasmtime/src/runtime/transaction/benchmark/runner.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/benchmark/workload.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/benchmark/metrics.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/benchmark/record.rs`

**Interfaces:**
- Consumes: `MatrixEntry`, `BenchmarkStorage`, `WorkloadState`, and `WorkerMetrics`
- Produces: `run_cell(&BenchmarkRequest, &CellSpec) -> Result<CellRecord>`
- Produces: `run_write_skew_attempt(&WriteSkewPair, &mut WorkerContext) -> Result<AttemptOutcome<OperationEffect>>`
- Produces: `MaintenanceGate::after_write_commit()` and `final_barrier()`
- Produces: `retry_delay(&mut DeterministicRng, u32) -> Duration`
- Produces: `CancellableRendezvous::wait(Instant) -> Result<()>` and `break_all()`

- [ ] **Step 1: Write failing retry, lifecycle, and write-skew tests**

Add these deterministic tests:

```rust
#[test]
fn retry_backoff_is_bounded_and_resets() {
    let mut rng = DeterministicRng::new(5, 0);
    for aborts in 0..64 {
        assert!(retry_delay(&mut rng, aborts) <= Duration::from_micros(64));
    }
    assert!(retry_delay(&mut rng, 0) <= Duration::from_micros(1));
}

#[test]
fn write_skew_invariant_rejects_both_cleared() {
    assert!(validate_write_skew_pair(0, 0).is_err());
    validate_write_skew_pair(0, 1).unwrap();
    validate_write_skew_pair(1, 0).unwrap();
}
```

Add `write_skew_two_workers_preserves_invariant`, a 2-worker, 20-schedule test
that requires zero `(0,0)` states, at least one conflict, two committed logical
operations per schedule, and successful restore to `(1,1)`. Add
`write_skew_one_worker_is_sequential_control`, requiring zero serialization
aborts.

Add a watchdog test using an injected worker closure that parks past its
deadline; require the error to name the cell, phase, and last per-worker
progress count. Add a paired-worker panic test and require its peer to leave a
rendezvous with a cancellation error instead of remaining blocked.

- [ ] **Step 2: Run the focused tests and verify missing runner failure**

Run:

```bash
cargo test -p wasmtime --no-default-features \
  --features "anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-mvcc,transaction-cc-optimistic-validation" \
  transaction::benchmark::runner --lib -- --nocapture
```

Expected: FAIL because the runner is not wired.

- [ ] **Step 3: Implement worker start, deadline, drain, and panic propagation**

Add `mod runner;` to `benchmark/mod.rs`. `run_cell` creates fresh storage and
workload state, spawns named workers, and uses a start `Barrier` for warmup and
measurement. Each worker owns `TransactionState`, `WorkerMetrics`, RNG, and an
atomic progress counter.

At the common deadline, stop launching new logical operations. Finish the
current operation and retries, then join. Run the final GC barrier before
capturing effective elapsed time. A cell watchdog budget is:

```rust
let requested = warmup + measurement;
let grace = Duration::from_secs(5).max(requested.saturating_mul(2));
let watchdog = requested + grace;
```

Use a supervisor channel with `recv_timeout`; do not block forever in `join`.
Convert worker panic payloads into contextual `FailureRecord` values. On panic
or timeout, set the shared cancellation flag and call `break_all()` on every
write-skew rendezvous. Poll `JoinHandle::is_finished()` until the remaining
grace period expires, join only handles known to be finished, and drop any
still-running handles before returning the failure. The cell error reports the
unfinished worker names and last progress counters; no error path performs an
unbounded join.

- [ ] **Step 4: Implement retry accounting and deterministic sleep**

Record attempt counts separately from retry-inclusive logical-operation
latency. On `AttemptOutcome::Conflict`, increment attempts, aborts, and retries;
call `retry_delay(rng, consecutive_aborts)` before saturating the counter
upward. The helper chooses a uniform deterministic value in
`1..=min(1 << consecutive_aborts.min(6), 64)` microseconds. Thus counter zero
means the first retry waits 1 microsecond and later ceilings are 2, 4, 8, 16,
32, and 64 microseconds. On commit, increment attempts, successful attempts,
and committed operations; record end-to-end latency; and reset
`consecutive_aborts` to zero.

- [ ] **Step 5: Implement the read/write maintenance gate**

Use `RwLock<()>` as a harness gate:

- every transaction attempt holds a read guard from begin through terminal
  commit or abort;
- retry sleep occurs after releasing the read guard;
- a worker that crosses a multiple of `gc_commit_interval` coalesces the
  request under a maintenance mutex, takes the write guard, and calls the real
  `BenchmarkStorage::run_gc_barrier()`;
- warmup and final maintenance always take the write guard;
- normal attempts never take an exclusive harness lock.

Accumulate barrier count, total duration, maximum pause, mode, versions removed
at barriers, and opportunistic pruning deltas. Count restore write commits in
the GC threshold even though their logical-operation metrics are separate.

- [ ] **Step 6: Implement the forced write-skew pair schedule**

For each pair, allocate two granules initialized to `(1,1)`. Both initial
attempts begin, read both values, and rendezvous at a reusable pair barrier
before either stages its own clear. Only the initial attempt waits there; a
loser retries the same role without returning to that barrier.

Implement that reusable barrier as `CancellableRendezvous`, backed by
`Mutex<{ generation, arrivals, broken }>`, a `Condvar`, and the cell
cancellation flag. `wait(deadline)` advances the generation when both workers
arrive, returns a contextual timeout when its deadline expires, and returns a
cancellation error after `break_all()`. Do not use `std::sync::Barrier` for
pair-local scheduling.

Define the pair contract as:

```rust
struct WriteSkewPair {
    addresses: [u64; 2],
    rendezvous: Arc<CancellableRendezvous>,
}
```

After both logical operations commit, rendezvous again. The lower-index worker
reads and validates the pair, records one schedule, restores `(1,1)` through
the ordinary retrying transaction path, and records restore metrics. A final
rendezvous releases the pair into its next schedule. The one-worker cell runs
role A and then role B sequentially before the same validation and restore.

Any observed `(0,0)` returns an invariant error immediately. Do not serialize
the two initial commits with a pair mutex.

- [ ] **Step 7: Merge metrics and validate every cell**

Merge worker-local histograms after join. Validate attempt identities, workload
state, no leaked active snapshots, no active certification permits, GC policy
mode, and output finiteness. For write skew, include schedules per second and
separate restore attempt/abort/latency metrics.

- [ ] **Step 8: Run default and all-policy tiny parallel cells**

First run Rust tests:

```bash
cargo test -p wasmtime transaction::benchmark::runner --lib -- --nocapture
```

Then loop over the seven single-version features with a 2-worker, 20-schedule
test filter using the complete feature closure. Expected: every policy
preserves the invariant and completes without a watchdog timeout. Finally run
the MVCC command from Step 2; expected: PASS with certification aborts visible.

Use this exact loop:

```bash
BENCH_BASE_FEATURES=anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser
for BENCH_CC in \
  transaction-cc-lockbased \
  transaction-cc-nowait-abort \
  transaction-cc-wound-wait \
  transaction-cc-wait-die \
  transaction-cc-strict-2pl \
  transaction-cc-optimistic-validation \
  transaction-cc-timestamp-ordering
do
  cargo test -p wasmtime --no-default-features \
    --features "$BENCH_BASE_FEATURES,$BENCH_CC" \
    transaction::benchmark::runner::tests::write_skew_two_workers_preserves_invariant \
    --lib -- --nocapture
done
```

- [ ] **Step 9: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/benchmark
git commit -m "Add parallel transaction benchmark runner"
```

---

### Task 6: Ignored release driver and one-worker `tfunc` controls

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/benchmark/mod.rs`
- Create: `crates/wasmtime/src/runtime/transaction/benchmark/tfunc.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/benchmark/config.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/benchmark/record.rs`

**Interfaces:**
- Consumes: `WASMTIME_TRANSACTION_BENCH_REQUEST` and `WASMTIME_TRANSACTION_BENCH_OUTPUT`
- Produces: ignored test `transaction_cc_benchmark_driver() -> Result<()>`
- Produces: `run_driver(&Path, &Path) -> Result<()>`
- Produces: `run_tfunc_controls(&BenchmarkRequest, BackendKind) -> Result<Vec<TfuncControlRecord>>`
- Produces: complete or failure footer after streaming every cell record

- [ ] **Step 1: Write failing driver and tfunc smoke tests**

Add a driver test that writes a request selecting VMemory, read-only, one
worker, 1 ms warmup, and 5 ms measurement; call `run_driver` directly with the
request and output paths; parse the output; and require `Driver`, `Cell`,
`TfuncControl`, and `Complete` records in that order.

Add `tfunc` tests for both backends requiring a read-only checksum and an RMW
counter delta equal to committed operations.

- [ ] **Step 2: Run the tests and verify the controls are missing**

Run:

```bash
cargo test -p wasmtime transaction::benchmark::tfunc --lib -- --nocapture
```

Expected: FAIL because the `tfunc` controls are not wired.

- [ ] **Step 3: Implement the two Wasm controls**

Add `mod tfunc;` and compile one transaction module containing:

```wat
(module
  (tmemory 1)
  (tfunc (export "read") (param i32) (result i64)
    (i64.tload (local.get 0)))
  (tfunc (export "rmw") (result i64)
    (local $next i64)
    (local.set $next
      (i64.add (i64.tload (i32.const 0)) (i64.const 1)))
    (i64.tstore (i32.const 0) (local.get $next))
    (local.get $next)))
```

VMemory uses a normal store. File-backed control calls
`transaction_create_file_backed_storage_for_test` with unique tmemory and log
paths before instantiation. Time only typed function calls, use the same
warmup/deadline/drain convention, and emit these results separately from core
cells. The read control visits the same eight deterministic addresses; the RMW
control validates its final counter.

- [ ] **Step 4: Implement the ignored driver**

Add this single ignored entry point in `benchmark/mod.rs`:

```rust
#[test]
#[ignore = "explicit release-mode transaction benchmark"]
fn transaction_cc_benchmark_driver() -> Result<()> {
    let request_path = std::env::var_os("WASMTIME_TRANSACTION_BENCH_REQUEST")
        .context("WASMTIME_TRANSACTION_BENCH_REQUEST is not set")?;
    let output_path = std::env::var_os("WASMTIME_TRANSACTION_BENCH_OUTPUT")
        .context("WASMTIME_TRANSACTION_BENCH_OUTPUT is not set")?;
    run_driver(Path::new(&request_path), Path::new(&output_path))
}
```

`run_driver` validates the requested policy before writing timed data, streams
one record per completed or skipped matrix entry, streams the requested
`tfunc` controls, and writes `Complete` only if all runnable cells succeed. On
error it first writes `Failure` with the active phase and cell, then returns
the error so `cargo test` exits nonzero.

- [ ] **Step 5: Run the release-mode driver smoke test**

The nonignored `release_driver_smoke` test calls `run_driver` with a tempfile
request and output, while the explicit production-sized entry point remains
ignored. Run:

```bash
cargo test -p wasmtime --release \
  transaction::benchmark::tests::release_driver_smoke --lib -- --nocapture
```

Expected: PASS; every JSONL line parses; the driver policy is `lockbased`; the
file ends in a `complete` record; and tempfile cleanup succeeds.

- [ ] **Step 6: Run MVCC release smoke**

Run:

```bash
cargo test -p wasmtime --release --no-default-features \
  --features "anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-mvcc,transaction-cc-optimistic-validation" \
  transaction::benchmark::tests::release_driver_smoke --lib -- --nocapture
```

Expected: PASS with core and `tfunc` records identifying MVCC+OCC.

- [ ] **Step 7: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/benchmark
git commit -m "Add transaction benchmark release driver"
```

---

### Task 7: Compile-time policy orchestrator and partial-run preservation

**Files:**
- Create: `scripts/transaction-cc-bench.py`
- Create: `scripts/tests/test_transaction_cc_bench.py`

**Interfaces:**
- Consumes: command-line filters and Rust driver JSON request schema version 1
- Produces: `build_command(policy: str, request: Path, output: Path) -> list[str]`
- Produces: `run_policy(policy: str, run_dir: Path, args: argparse.Namespace) -> PolicyResult`
- Produces: `target/transaction-bench/<timestamp>-<revision>/raw/<policy>.jsonl`
- Produces: run-level `invocation.json` and `orchestrator.jsonl`

- [ ] **Step 1: Write failing Python command and failure-preservation tests**

Create `scripts/tests/test_transaction_cc_bench.py` using `unittest`. Load the
hyphenated script with `importlib.util.spec_from_file_location`. Cover:

```python
def test_mvcc_command_has_exactly_optimistic_and_mvcc(self):
    cmd = bench.build_command("mvcc-optimistic", Path("request.json"), Path("out.jsonl"))
    features = cmd[cmd.index("--features") + 1].split(",")
    self.assertIn("transaction-mvcc", features)
    self.assertIn("transaction-cc-optimistic-validation", features)
    self.assertEqual(1, len([f for f in features if f.startswith("transaction-cc-")]))

def test_single_version_commands_have_one_cc_and_no_mvcc(self):
    for policy in bench.POLICIES:
        if policy == "mvcc-optimistic":
            continue
        features = bench.features_for(policy)
        self.assertNotIn("transaction-mvcc", features)
        self.assertEqual(1, len([f for f in features if f.startswith("transaction-cc-")]))
```

Mock `subprocess.run` so the third policy exits nonzero after writing one raw
line. Require the first three raw files and failure metadata to remain, require
the orchestrator to return nonzero, and require no completeness marker.

- [ ] **Step 2: Run Python tests and verify the script is absent**

Run:

```bash
python3 -m unittest scripts.tests.test_transaction_cc_bench -v
```

Expected: FAIL because `scripts/transaction-cc-bench.py` does not exist.

- [ ] **Step 3: Implement policy and complete-feature constants**

Use these exact policy entries:

```python
POLICIES = {
    "lockbased": ["transaction-cc-lockbased"],
    "no-wait": ["transaction-cc-nowait-abort"],
    "optimistic": ["transaction-cc-optimistic-validation"],
    "strict-2pl": ["transaction-cc-strict-2pl"],
    "timestamp": ["transaction-cc-timestamp-ordering"],
    "wait-die": ["transaction-cc-wait-die"],
    "wound-wait": ["transaction-cc-wound-wait"],
    "mvcc-optimistic": [
        "transaction-mvcc",
        "transaction-cc-optimistic-validation",
    ],
}
```

Define `BASE_FEATURES` from the complete closure at the top of this plan. Build
commands always use `cargo test -q -p wasmtime --release
--no-default-features --features <joined features>
transaction_cc_benchmark_driver --lib -- --ignored --nocapture
--test-threads=1`.

- [ ] **Step 4: Implement CLI and request generation**

Use `argparse` with:

```text
--profile quick
--policies all|comma-list
--backends vmemory,file-backed
--workloads read-only,disjoint-writes,hot-key,write-skew
--workers 1,2,4,8,16
--warmup-ms 100
--measure-ms 500
--repetitions 1
--seed 0x5eed5eedd15ca11e
--gc-commit-interval 4096
--output-dir PATH
--skip-tfunc-control
```

Explicit duration flags override the profile. Reject names not present in the
Rust schema before invoking Cargo. Write one request JSON file per policy with
that policy as `expected_policy`.

- [ ] **Step 5: Implement run identity and failure-safe execution**

The default run directory is
`target/transaction-bench/<UTC timestamp>-<12-character revision>`. Record:

- full and short git revision;
- dirty-state Boolean and changed-path list;
- exact argv;
- `rustc -Vv`;
- Python version;
- `platform.uname()`;
- CPU model from `platform.processor()` with `/proc/cpuinfo` fallback;
- `os.cpu_count()`;
- start/end UTC timestamps and actual elapsed time.

Run policies sequentially to avoid compilation and measurement interference.
Pass request/output paths through the two required environment variables.
Never truncate a raw policy file after the Rust driver has started. Stop after
the first failed build or driver, write an orchestrator failure record, and
exit nonzero.

- [ ] **Step 6: Run Python tests**

Run:

```bash
python3 -m unittest scripts.tests.test_transaction_cc_bench -v
```

Expected: PASS for exact CC selection, MVCC pairing, argument validation,
stable request serialization, output paths, and partial-run preservation.

- [ ] **Step 7: Run two-policy smoke orchestration**

Run:

```bash
python3 scripts/transaction-cc-bench.py \
  --policies lockbased,mvcc-optimistic \
  --backends vmemory \
  --workloads read-only,hot-key \
  --workers 1,2 \
  --warmup-ms 1 \
  --measure-ms 5 \
  --skip-tfunc-control
```

Expected: both exact feature builds pass, both raw JSONL streams end in
`complete`, and all generated files are below `target/transaction-bench/`.

- [ ] **Step 8: Commit**

```bash
git add scripts/transaction-cc-bench.py scripts/tests/test_transaction_cc_bench.py
git commit -m "Add transaction benchmark orchestrator"
```

---

### Task 8: JSONL validator, CSV/Markdown report, and user documentation

**Files:**
- Create: `scripts/transaction_cc_bench_report.py`
- Create: `scripts/tests/test_transaction_cc_bench_report.py`
- Modify: `scripts/transaction-cc-bench.py`
- Create: `docs/shisoft/2026-07-31-transaction-cc-benchmark.md`

**Interfaces:**
- Consumes: all raw policy JSONL streams plus `invocation.json`
- Produces: `load_complete_run(run_dir: Path) -> RunData`
- Produces: `write_csv(data: RunData, path: Path) -> None`
- Produces: `write_markdown(data: RunData, path: Path) -> None`
- Produces: merged `results.jsonl`, `results.csv`, and `summary.md`

- [ ] **Step 1: Write failing reporter fixture tests**

Create a temporary run with two policies, two backends, one workload, worker
counts one and two, one skipped worker count, one `tfunc` record, and complete
footers. Tests require:

- duplicate cell identity is rejected;
- a missing completeness footer is rejected;
- mixed schema versions are rejected;
- CSV rows equal completed plus skipped cells plus controls;
- Markdown contains both policies, MVCC-versus-OCC delta, backend delta,
  scaling, abort/retry, GC/version, `tfunc`, skipped-cell, host, and dirty-state
  sections;
- aggregate attempt, commit, abort, and retry counts exactly equal JSONL.

- [ ] **Step 2: Run reporter tests and verify missing module failure**

Run:

```bash
python3 -m unittest scripts.tests.test_transaction_cc_bench_report -v
```

Expected: FAIL because the reporter does not exist.

- [ ] **Step 3: Implement strict JSONL loading and completeness validation**

Use immutable tuple keys:

```python
CellKey = tuple[str, str, str, int, int, int]
# policy, backend, workload, workers, repetition, seed
```

Require schema version 1, one driver identity and one complete footer per
requested policy, matching expected and compiled policy names, unique cell
keys, explicit skipped cells for unsupported requested workers, and no failure
records. Validate each driver's, cell's, skipped cell's, and control's compiled
transaction feature list against the exact policy mapping before validating
the Rust attempt identities again in Python. Merge raw files in requested
policy order without modifying them.

- [ ] **Step 4: Implement stable CSV**

Write one header and one row per cell/control. Use fixed column order beginning
with identity and status, followed by elapsed time, throughput, latency,
attempt counts, fairness, GC, version statistics, write-skew schedule/restore
statistics, and skip/failure reason. Represent unavailable percentiles as an
empty field, not zero.

- [ ] **Step 5: Implement Markdown comparisons**

Generate compact tables grouped by backend and workload. Include absolute
committed operations/s, scaling versus each policy's one-worker cell, abort
rate, retries/commit, p99, fairness, and GC share. Add focused comparison
tables for:

```text
mvcc-optimistic / optimistic
file-backed / vmemory
tfunc / matching one-worker core cell
```

Show ratios and signed percentages only when both denominators exist. List
skips and low-sample unavailable percentiles. Lead with host/revision/profile
metadata and end with actual total elapsed time.

- [ ] **Step 6: Connect reporting to successful orchestration**

After every requested policy completes, call the reporter to create
`results.jsonl`, `results.csv`, and `summary.md`. If validation or report
generation fails, preserve all raw files, write orchestrator failure metadata,
omit the run success footer, and exit nonzero.

- [ ] **Step 7: Document the command and interpretation rules**

In `docs/shisoft/2026-07-31-transaction-cc-benchmark.md`, include:

- the default quick command;
- filtered smoke and longer-run examples;
- the exact eight policy names and two backend names;
- all four workload definitions, including the `(1,1)` write-skew invariant;
- retry and GC cadence semantics;
- core versus `tfunc` distinction;
- output layout and schema version;
- the warning that only same-host/same-revision results are directly
  comparable;
- the warning that throughput is never a correctness oracle;
- DAX exclusion and result-files-not-committed policy.

- [ ] **Step 8: Run all Python tests and inspect fixture output**

Run:

```bash
python3 -m unittest discover -s scripts/tests -p 'test_transaction_cc_bench*.py' -v
```

Expected: PASS. Open the generated fixture CSV and Markdown and verify the
counts and ratios stated in the tests are visible without reading JSONL.

- [ ] **Step 9: Commit**

```bash
git add scripts/transaction-cc-bench.py \
  scripts/transaction_cc_bench_report.py \
  scripts/tests/test_transaction_cc_bench.py \
  scripts/tests/test_transaction_cc_bench_report.py \
  docs/shisoft/2026-07-31-transaction-cc-benchmark.md
git commit -m "Add transaction benchmark reports and guide"
```

---

### Task 9: Full verification and comprehensive benchmark run

**Files:**
- Verify only: all files changed in Tasks 1-8
- Generate, do not commit: `target/transaction-bench/<run-id>/`

**Interfaces:**
- Consumes: committed release driver and orchestration/reporting scripts
- Produces: one complete local JSONL/CSV/Markdown comparison run
- Produces: final verification evidence in the handoff, not in a committed result file

- [ ] **Step 1: Format and run static checks**

Run:

```bash
cargo fmt --all
cargo fmt --all -- --check
git diff --check
python3 -m py_compile scripts/transaction-cc-bench.py \
  scripts/transaction_cc_bench_report.py \
  scripts/tests/test_transaction_cc_bench.py \
  scripts/tests/test_transaction_cc_bench_report.py
```

Expected: all commands exit zero.

- [ ] **Step 2: Run Rust and Python benchmark tests**

Run:

```bash
cargo test -p wasmtime transaction::benchmark --lib -- --nocapture
python3 -m unittest discover -s scripts/tests -p 'test_transaction_cc_bench*.py' -v
```

Expected: all benchmark logic, schema, workload, watchdog, GC, driver, and
reporter tests pass; the ignored release driver remains ignored in ordinary
Rust tests.

- [ ] **Step 3: Run exact-feature tiny smoke across all eight policies**

Run:

```bash
python3 scripts/transaction-cc-bench.py \
  --policies all \
  --backends vmemory,file-backed \
  --workloads read-only,disjoint-writes,hot-key,write-skew \
  --workers 1,2 \
  --warmup-ms 1 \
  --measure-ms 10 \
  --repetitions 1
```

Expected: eight independently compiled policy drivers complete; both backends
and all workloads appear; write-skew invariants hold; JSONL, CSV, and Markdown
validation passes.

- [ ] **Step 4: Rerun affected default and MVCC transaction suites**

Run:

```bash
cargo test -q -p wasmtime transaction:: --lib -- --format terse
cargo test -q -p wasmtime --test transaction_persistence -- --format terse
cargo test -q -p wasmtime --no-default-features \
  --features "anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-mvcc,transaction-cc-optimistic-validation" \
  transaction:: --lib -- --format terse
cargo test -q -p wasmtime --no-default-features \
  --features "anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-mvcc,transaction-cc-optimistic-validation" \
  --test transaction_persistence -- --format terse
```

Expected: the established default and MVCC unit/persistence totals remain
green, apart from intentional count increases from new benchmark tests.

- [ ] **Step 5: Verify every single-version unit configuration**

Loop over:

```text
transaction-cc-lockbased
transaction-cc-nowait-abort
transaction-cc-wound-wait
transaction-cc-wait-die
transaction-cc-strict-2pl
transaction-cc-optimistic-validation
transaction-cc-timestamp-ordering
```

For each, run `cargo test -q -p wasmtime --no-default-features` with the complete
feature closure plus that one feature, followed by `transaction:: --lib --
--format terse`.

Use:

```bash
BENCH_BASE_FEATURES=anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser
for BENCH_CC in \
  transaction-cc-lockbased \
  transaction-cc-nowait-abort \
  transaction-cc-wound-wait \
  transaction-cc-wait-die \
  transaction-cc-strict-2pl \
  transaction-cc-optimistic-validation \
  transaction-cc-timestamp-ordering
do
  cargo test -q -p wasmtime --no-default-features \
    --features "$BENCH_BASE_FEATURES,$BENCH_CC" \
    transaction:: --lib -- --format terse
done
```

Expected: all seven suites pass with exactly one CC compiled.

- [ ] **Step 6: Run the full default quick benchmark**

Run:

```bash
python3 scripts/transaction-cc-bench.py --profile quick
```

Expected: every host-eligible cell across eight policies, two backends, four
workloads, and worker counts `1,2,4,8,16` completes; ineligible counts are
explicit skips; `tfunc` controls complete; no write-skew invariant fails; the
run has authoritative JSONL, consistent CSV, and Markdown summary. Record the
actual elapsed time even if host compilation or storage makes it exceed the
five-to-ten-minute target.

- [ ] **Step 7: Audit generated and tracked state**

Run:

```bash
git status --short
git ls-files 'target/transaction-bench/**'
```

Expected: no benchmark result is tracked. `git status` is clean after the
implementation commits, while the ignored `target/transaction-bench/` run
remains available locally.

- [ ] **Step 8: Report results without publishing upstream**

The final handoff names all implementation commits, the generated local run
directory, total elapsed time, eligible/skipped cells, test totals, and the
headline MVCC-versus-OCC and backend comparisons from `summary.md`. Do not open
or interact with a Wasmtime pull request or issue.
