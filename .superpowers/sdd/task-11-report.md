# Task 11 Report

## Status

Complete. Deterministic real-runtime schedules show that ordered commit-time
MVCC certification prevents write skew, blind write/write conflicts, lost
updates, and stale read-dependency commits while preserving stable read-only
snapshots and overlap for disjoint writers.

No production behavior changed. The only non-test module change exposes the
new reference model under `cfg(test)`.

## Deterministic schedule coverage

The `mvcc_serializable_` suite covers:

- two blind writers staging the same shared file-backed granule before a
  timeout-aware parent-controlled gate releases both commits;
- two lost-update increments reading and staging from the same snapshot;
- classic two-record write skew with `A = 1`, `B = 1`, where both transactions
  read both values and stage disjoint zero writes before release;
- a transaction that reads `A` and stages a disjoint write to `B`, held until
  another transaction commits a change to `A`;
- a read-only transaction held between two reads while a writer commits;
- two disjoint granule writers in the same shared file-backed memory held
  together inside terminal installation, with two active certification permits
  and both final values checked from a fresh Store;
- conflicting and disjoint schedules across multiple Stores sharing one
  `TransactionRegionRuntime`;
- a memory/global/table commit observed by one old snapshot on both sides of
  the common pending-record transition, proving it cannot observe a mixed
  old/new commit.

Every concurrent worker reports through a bounded channel before its handle is
joined. Parent-released `MvccCommitTestHook` gates have bounded arrival waits
and release on timeout; writes are staged before those gates. Tests assert that
target raw `InstanceId`s deliberately differ across Stores, so shared
file-backed conflict/disjointness results cannot pass through accidentally
equal store-local identities. They also assert zero active certification
permits and snapshots, balanced commit/snapshot lifecycle counts, and admission
through both MVCC and persistent-GC barriers.

## Reference model

`mvcc/model.rs` contains a test-only serializable-MVCC model with:

- one visible commit timestamp;
- per-granule committed version lists;
- transaction snapshot, read set, and write map;
- read-your-own-write behavior;
- read-only completion without advancing the timestamp;
- commit rejection when any read or write granule has a newer committed
  timestamp than the transaction snapshot;
- atomic timestamp assignment and append of every accepted write.

Two fixed seeds generate 24 bounded schedules apiece. Each transaction owns a
queue containing Begin, reads, optional writes, and Commit; a fixed xorshift
generator interleaves eligible queues while preserving transaction-local
order. Cases use 2–4 transactions and 2–4 granules and include read-only
transactions. Every generated write is followed by a read of the just-written
granule, deliberately exercising read-your-own-write.

The real side executes the same operations through the production snapshot
facade, shared certification authority, MVCC reads, one pending commit record,
one prepare guard for every write, and `PendingCommitRegistration::publish`.
It records each transaction's snapshot, ordered observed reads and writes,
read-your-own-write classification, commit position, and outcome. A separate
fresh model then replays only accepted transactions at valid serialization
points: writers in assigned-timestamp order and read-only transactions at
their original snapshot. The replay checks every external and
read-your-own-write observation, exact commit outcome/timestamp, and final
visible state. Aggregate assertions require nonzero conflicts, overlapping
disjoint committed writers, read-only transactions, and read-your-own-write
reads. Failures print the seed, case, and complete operation list.

## TDD evidence

- The named write-skew test was added and run before model work. Existing
  production certification already preserved the invariant. Its first
  assertion run failed because `Error::to_string()` omitted the chained
  certification cause; the test was corrected to inspect `{error:#}` and then
  passed without a production change.
- The reference-model test was added before its API. The RED build failed on
  unresolved `ModelCommitOutcome`, `ModelOperation`, `ModelTransaction`,
  `SerializableMvccModel`, and `generate_model_schedules` symbols. Implementing
  only that test-only API made the exact model test pass.
- The review-strengthening test for deliberate read-your-own-write coverage was
  run before changing the generator and failed with
  `assertion failed: read_your_own_write > 0`. Adding a read of each
  just-written granule made it pass.
- The multi-Store write-skew test first asserted unequal target raw
  `InstanceId`s without dummy instances and failed because both were `1`.
  Deliberately instantiating a dummy only in the second Store made the target
  IDs differ while the shared file-backed granules continued to conflict.

## Verification

The complete Task 10 feature closure was used:

```text
anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,
parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,
debug-builtins,runtime,component-model,component-model-async,threads,
stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-mvcc,
transaction-cc-optimistic-validation
```

Fresh verification:

```text
exact fully-qualified write-skew test
1 passed; 0 failed

exact disjoint-terminal-publication test
1 passed; 0 failed

exact fixed-seed transcript/replay test
1 passed; 0 failed

exact deliberate read-your-own-write generator test
1 passed; 0 failed

mvcc_serializable_ filter, repeated five times
8 passed; 0 failed on every run

mvcc_certification_ filter
7 passed; 0 failed

mvcc_commit_ filter
22 passed; 0 failed

cargo check with complete MVCC feature closure
exit 0
```

Only existing workspace warnings were emitted.
