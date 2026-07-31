# Task 12 Report

## Task 12A: MVCC-Owned Pruning and Rebase Primitives

Implemented the MVCC-owned half of Task 12 without extracting persistent-GC
policies and without editing `state.rs`, `region_runtime.rs`, or `object_gc.rs`.

The coordinator now captures a conservative pruning horizon in one lock
acquisition as `oldest_active_snapshot.unwrap_or(visible_timestamp)`, reports
MVCC GC metadata, rejects pending-commit registration while a GC barrier is
active, validates that rebase operations use the exact live barrier permit, and
advances the baseline while still holding that permit and the coordinator lock.
Full rebase always locks coordinator before domains.

`mvcc/gc.rs` owns a pruning budget distinct from persistent-GC budgets. It
scans the six typed sidecar tables with per-table cursors and a table-kind
round-robin cursor. Opportunistic pruning retains pending and aborted entries,
all commits newer than the captured horizon, and exactly one committed
predecessor at or before it. Successful snapshot completion invokes a
best-effort eight-chain pruning step; snapshot-finish and pruning failures
remain separate so pruning cannot change a completed transaction result.

Permit-bound full rebase validates that retained chains have no pending
version, clones newest expected object payloads into a plan without retaining
the domains lock, represents aborted-only new objects as expected absent
slots, revalidates before mutation, clears all six maps and cursors, and
advances the baseline to the visible timestamp. The future Task 12B adapter
will validate the plan against `ObjectTable` and invoke the selected
persistent-GC policy.

### TDD evidence

The first focused RED failed only on the missing pruning API:

```text
unresolved MvccPruneBudget
missing MvccRuntime::prune_horizon_for_test
missing MvccRuntime::prune_versions_at_horizon_for_test
missing MvccRuntime::prune_versions_for_test
```

The first GREEN covered the barrier, conservative no-active V/V+1 cut point,
active horizons, aborted-version retention, budget bounds, six-table fairness,
and cursor wrap:

```text
mvcc_gc_ filter
5 passed; 0 failed
```

The second RED failed on the missing permit-bound rebase plan and completion
APIs. Its first compile after implementation exposed two local type issues:
borrowing the coordinator guard for metadata and deriving `Eq` for
`ObjectPayload`. Both were corrected, after which:

```text
mvcc_gc_ filter
8 passed; 0 failed
```

The final proof-test RED failed only because the deterministic
coordinator-before-domains hook did not yet exist. Adding the test-only hook
made the complete focused slice GREEN:

```text
mvcc_gc_ filter
12 passed; 0 failed
```

The final slice additionally proves metadata transitions, rejection of an
unregistered pending chain during barrier validation, pruning only after
successful snapshot finish, and an event-driven interleaving in which rebase
holds the coordinator while waiting for an already-held domains lock.

### Verification

All commands below used the complete Task 10 MVCC unit-test feature closure
where shown:

```text
anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,
parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,
debug-builtins,runtime,component-model,component-model-async,threads,
stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-mvcc,
transaction-cc-optimistic-validation
```

Fresh results:

```text
cargo test ... --lib runtime::transaction::tests::mvcc_gc_
12 passed; 0 failed

cargo test ... --lib runtime::transaction::mvcc::
31 passed; 0 failed

cargo test ... --lib runtime::transaction::tests::mvcc_serializable_
8 passed; 0 failed

cargo test ... --lib runtime::transaction::tests::mvcc_commit_
19 passed; 0 failed

cargo check ... (complete MVCC feature closure)
exit 0

cargo check -p wasmtime --no-default-features \
  --features runtime,std,gc,cranelift,wat,transaction-cc-optimistic-validation
exit 0
```

Only existing workspace warnings were emitted. No backend, durable-format,
commit-LP, copy-on-write, persistent-GC policy, or object reachability code was
changed in Task 12A.
