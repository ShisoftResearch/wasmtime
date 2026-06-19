# Timestamp Concurrency-Control Roadmap

Date: 2026-06-19

## Goal

Extend the transaction concurrency-control switch beyond `LockBased` and
`NoWaitAbort` with timestamp-oriented lock policies while keeping the current
single-version storage and recovery model intact.

Use `TransactionId` as the timestamp for this wave. It already provides a total
order, is monotonic in the local and shared transaction runtimes, and matches
the existing convention that a lower id is older. Add semantic helper names
around the comparison so timestamp policies read as timestamp policies instead
of raw id arithmetic.

## Non-Goal: MVCC

Do not add MVCC in this roadmap.

MVCC needs cooperation with version storage and garbage collection:

- multiple committed versions per `GranuleId`
- read timestamp visibility rules
- commit timestamp assignment
- old-version retirement for `tmemory`, object records, roots, globals, and
  tables
- coordination with persistent object GC and recovery-time object filtering

Those requirements make MVCC a storage/GC/recovery project, not just another
concurrency-control policy.

## Milestone 1: Policy Trait Contract

Make `TransactionConcurrencyControl` the full policy contract for single-version
concurrency policies.

Required trait surface:

- acquire granule read
- acquire granule write
- validate optimistic read
- refresh read version
- release transaction
- query current owner for runtime terminal-owner checks
- list read granules for active-read validation

`ConcurrencyControlState` should remain the compile-time-selected enum wrapper,
but it should mostly delegate through this trait rather than manually mirroring
every policy method. After this milestone, adding a policy should mean:

1. implement the trait
2. add a `ConcurrencyControl` enum case
3. add a `ConcurrencyControlState` enum case
4. add one Cargo feature
5. add policy-specific tests

No behavior change is expected in this milestone.

## Milestone 2: Timestamp Helpers

Add lightweight timestamp terminology without adding new runtime state.

Recommended shape:

```rust
type TransactionTimestamp = TransactionId;

fn timestamp_for_transaction(transaction: TransactionId) -> TransactionTimestamp {
    transaction
}

fn is_older(requester: TransactionId, owner: TransactionId) -> bool {
    timestamp_for_transaction(requester) < timestamp_for_transaction(owner)
}
```

Keep these helpers private to the concurrency module unless another runtime
layer needs them. The purpose is readability and future replacement, not a new
public or embedding-facing API.

## Milestone 3: Wound-Wait

Add `transaction-cc-wound-wait`.

Semantics:

- A transaction's timestamp is its `TransactionId`.
- Older requester vs younger owner: requester wounds the owner.
- Younger requester vs older owner: requester receives a conflict.
- Reads remain optimistic unless a writer owns the granule.
- Writes remain pessimistic.
- Commit still validates optimistic reads against current versions.
- Commit/abort releases ownership and read-version tracking.

Why first:

- It is closest to the current `LockBased` id-priority behavior.
- It exercises the existing conflict-aborted transaction cleanup path.
- It proves the timestamp helper layer without requiring wait queues.

Expected tests:

- older writer wounds younger owner
- older reader wounds younger writer before recording read version
- younger writer conflicts with older owner
- wounded transaction cleanup releases owned granules and allocated objects
- shared runtime records wounded transactions consistently

## Milestone 4: Wait-Die Without Blocking

Add `transaction-cc-wait-die`.

Initial semantics should not block runtime threads. The runtime does not yet
have wait queues, wakeups, or retry scheduling, so "wait" must be represented as
a structured conflict result for now.

Semantics:

- Older requester vs younger owner: return a `WouldWait` conflict.
- Younger requester vs older owner: requester dies with an ordinary conflict.
- No transaction preempts another transaction in the first version.
- Commit/abort behavior stays the same as other single-version policies.

Why after Wound-Wait:

- It exposes the missing wait/retry abstraction explicitly.
- It avoids faking blocking semantics inside host-thread execution.
- Later, if the runtime gets wait queues, `WouldWait` can become the boundary
  where blocking or retry integration attaches.

Expected tests:

- older requester gets `WouldWait` against younger owner
- younger requester dies against older owner
- owner is unchanged after `WouldWait`
- shared runtime leaves both transactions open after `WouldWait`
- commit/abort release state normally

## Milestone 5: Strict Two-Phase Locking Baseline

Add `transaction-cc-strict-2pl`.

Semantics:

- Reads acquire shared read ownership.
- Writes acquire exclusive ownership.
- Read and write locks are held until commit or abort.
- Upgrading from read to write succeeds only when no other transaction holds a
  read lock for the same granule.
- No timestamp preemption.

Why include it:

- It gives a non-timestamp baseline.
- It helps compare optimistic reads vs shared read locks.
- It exercises a different state shape before any future wait-queue work.

Expected tests:

- multiple readers can hold the same granule
- writer conflicts with active readers
- reader conflicts with active writer
- owner/readers release on commit and abort
- read-to-write upgrade succeeds only for sole reader

## Milestone 6: Optimistic Validation Only

Add `transaction-cc-optimistic-validation`.

Semantics:

- Reads record versions.
- Writes stage privately without pessimistic ownership.
- Commit validates read and write granule versions before publishing.
- Commit fails if any staged write granule changed since it was first observed.

This remains single-version. It does not create snapshots and does not keep old
versions alive.

Why last in this roadmap:

- It changes when write conflicts are detected.
- It needs careful commit-path validation coverage.
- It is the closest single-version stepping stone toward future MVCC, but it
  should not pull in MVCC storage.

## Feature Flag Shape

Keep exactly one transaction concurrency-control feature selected at a time.

Current:

- `transaction-cc-lockbased`
- `transaction-cc-nowait-abort`

Planned:

- `transaction-cc-wound-wait`
- `transaction-cc-wait-die`
- `transaction-cc-strict-2pl`
- `transaction-cc-optimistic-validation`

The default branch policy should remain `transaction-cc-lockbased` until a
later experiment deliberately changes the default.

## Verification Strategy

Each policy should have three layers of tests:

1. Direct policy unit tests over `GranuleId`, `TransactionId`, and versions.
2. `ConcurrencyControlState` selected-policy tests under the matching Cargo
   feature.
3. Runtime tests for both store-local and shared-region transactions.

Policy-specific winner/preemption assertions must be gated under that policy's
feature. Generic transaction persistence tests should assert only that a
transaction conflict occurred unless the policy explicitly defines the winner.

Use the existing alternate-policy command shape, adding default support features
for lib unit tests when needed:

```bash
cargo check -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,transaction-cc-wound-wait"

cargo test -p wasmtime --no-default-features \
  --features "anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-cc-wound-wait" \
  --lib transaction:: -- --format terse
```

