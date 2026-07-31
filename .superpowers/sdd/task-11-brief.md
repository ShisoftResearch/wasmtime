### Task 11: Prevent Write Skew and Prove Serializable Schedules

Base commit: `ee4314a8b3bba4b6eead4286a79d7ad58b899073`

Plan: `docs/superpowers/plans/2026-07-30-serializable-mvcc-implementation.md`,
Task 11.

## Goal

Prove that MVCC plus ordered commit-time optimistic certification is
serializable and opaque across deterministic concurrent schedules, especially
classic write skew. This task is primarily tests and a test-only reference
model; production behavior should only change if a test exposes a real defect.

## Required coverage

- Blind write/write conflict.
- Lost update.
- Classic two-record write skew: both transactions read `A = 1, B = 1`; one
  stages `A = 0`, the other `B = 0`; release commits concurrently; exactly one
  commits, the other reports certification conflict, and `A + B >= 1`.
- Changed read dependency plus disjoint write.
- Read-only transaction concurrent with a writer.
- Disjoint writers overlapping terminal publication.
- Conflicting and disjoint schedules across multiple stores sharing a runtime.
- No reader observes an in-flight pending commit as a mixture of old/new
  domains.

Use barriers/hooks, not sleeps or probabilistic timing. Assert thread joins and
permit/snapshot cleanup so a deadlock cannot be hidden.

## Reference model

Create `crates/wasmtime/src/runtime/transaction/mvcc/model.rs` as a test-only
small serializable-MVCC model and expose it only under `cfg(test)`.

The model tracks visible timestamp, per-granule committed versions, transaction
snapshot/read set/write map, and applies the implementation rule:

- reject when the newest committed timestamp of any read or write granule is
  newer than the transaction snapshot;
- otherwise assign the next timestamp and atomically append every write.

Generate bounded schedules from fixed deterministic seed(s), print seed and
operation list on failure, execute equivalent operations through real
visibility/certification helpers, and compare commit/abort outcomes plus final
visible values. Use 2–4 granules and 2–4 transactions without creating an
unbounded test.

## TDD and verification

1. Add the named write-skew regression first and run it RED if any missing
   behavior is exposed.
2. Implement only what is needed for GREEN.
3. Add the remaining deterministic and model comparisons.
4. Run the exact write-skew test, then the entire `mvcc_serializable_` filter
   five times.
5. Run adjacent MVCC certification/publication tests, `cargo check` for the
   complete MVCC feature closure recorded in
   `.superpowers/sdd/task-10-report.md`, rustfmt check, and `git diff --check`.

The plan's minimal feature list does not compile unrelated unconditional
Wasmtime tests; use the complete unit-test feature closure from the Task 10
report.

## Deliverables

- `crates/wasmtime/src/runtime/transaction/mvcc/model.rs`
- `crates/wasmtime/src/runtime/transaction/mvcc/mod.rs`
- `crates/wasmtime/src/runtime/transaction/tests.rs`
- `.superpowers/sdd/task-11-report.md`
- Commit: `Test serializable MVCC schedules`

Do not edit `.superpowers/sdd/progress.md`. Do not open/comment/review any
upstream Wasmtime PR or issue, and do not add co-author annotations.
