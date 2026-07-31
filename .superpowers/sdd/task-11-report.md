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
  barrier releases both commits;
- two lost-update increments reading and staging from the same snapshot;
- classic two-record write skew with `A = 1`, `B = 1`, where both transactions
  read both values and stage disjoint zero writes before release;
- a transaction that reads `A` and stages a disjoint write to `B`, held until
  another transaction commits a change to `A`;
- a read-only transaction held between two reads while a writer commits;
- two disjoint writers held together inside terminal installation;
- conflicting and disjoint schedules across multiple Stores sharing one
  `TransactionRegionRuntime`;
- a memory/global/table commit observed by one old snapshot on both sides of
  the common pending-record transition, proving it cannot observe a mixed
  old/new commit.

Every concurrent worker reports through a bounded channel before its handle is
joined. Host barriers and `MvccCommitTestHook` gates provide deterministic
ordering without sleeps. Tests assert zero active certification permits and
snapshots, balanced commit/snapshot lifecycle counts, and admission through
both MVCC and persistent-GC barriers.

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
transactions.

The real side executes the same operations through the production snapshot
facade, shared certification authority, MVCC reads, one pending commit record,
one prepare guard for every write, and `PendingCommitRegistration::publish`.
It compares every read, commit/conflict outcome, assigned timestamp, final
visible value, and a replay in committed timestamp order. Failures print the
seed, case, and complete operation list.

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
