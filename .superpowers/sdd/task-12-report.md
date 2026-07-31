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

## Task 12A Review Fix: Validated Rebase Type State

Review found that `complete_full_rebase` could clear histories without proving
that the prepared object expectations had been checked against `ObjectTable`.
The corrected API enforces this sequence:

```text
prepare_full_rebase(permit) -> MvccRebasePlan
MvccRebasePlan::validate_current_objects(self, &mut ObjectTable)
    -> ValidatedMvccRebasePlan
complete_full_rebase(permit, ValidatedMvccRebasePlan)
```

`ValidatedMvccRebasePlan` has no public or crate-visible field constructor and
is not cloneable. Validation refreshes each current object snapshot. A present
expectation requires the exact `ObjectId` to name a live persistent slot and
requires exact payload equality; an absent expectation requires no live current
slot. Historical sidecars are not traversed through `ObjectTable`, exposed as
roots, or made collector-visible.

Every admitted barrier now receives a checked monotonic generation. The plan
and validated token retain an opaque coordinator identity plus that generation.
Completion rechecks coordinator identity, barrier generation, quiescence,
complete coordinator metadata, and the internal expected object states while
holding coordinator before domains. All fallible checks precede the six
infallible map/cursor clears and baseline assignment. Stale, cross-barrier,
cross-coordinator, metadata-changed, and sidecar-changed tokens fail without
mutation.

The first focused RED failed exactly on the missing type-state interface:

```text
E0599: no method named MvccRebasePlan::validate_current_objects
E0061: complete_full_rebase accepted only the permit
```

After implementing the opaque token and updating real `ObjectTable` fixtures:

```text
mvcc_gc_ filter
19 passed; 0 failed

runtime::transaction::mvcc:: filter
33 passed; 0 failed
```

The focused tests distinguish a payload-equal volatile slot from a persistent
slot, reject payload mismatch, enforce absent slots for aborted-only new-object
chains, reject prior-epoch and foreign-coordinator tokens, revalidate sidecars
and metadata at completion, and prove failed validation preserves histories,
baseline, and later snapshot admission. Two private `gc.rs` unit tests exercise
actual per-table key selection across successor movement, insertion above and
below the cursor, removal of the cursor key, and wraparound without adding a
broad runtime test API.

Fresh adjacent and compile verification:

```text
mvcc_serializable_ filter: 8 passed; 0 failed
mvcc_commit_ filter: 19 passed; 0 failed
complete MVCC feature-closure cargo check: exit 0
non-MVCC OCC cargo check: exit 0
```

The plan still clones newest current object payloads for the bounded,
quiescent barrier phase. This can temporarily increase peak memory, but keeping
that stable validation snapshot is the current brief-required tradeoff.
Chunked object validation would change adapter sequencing and is deferred
outside this review fix.

## Task 12B: Pluggable Persistent-GC Policy and Compatibility Adapter

Task 12B adds an object-safe `TransactionPersistentGc` policy with four
operations: full mark/sweep, bounded maintenance, finish/sweep, and persistent
object compaction. `CurrentStatePersistentGc` is the default in every build.
`GcMvccMode::{CurrentStateOnly, MvccCompliant}` controls only whether the
callback receives a metadata-only `TransactionMvccGcView`; it never changes
barrier or rebase safety.

Local transaction state and shared region runtimes own independent
`Arc<dyn TransactionPersistentGc>` handles. Shared selection pairs the shared
policy with that same runtime's visibility authority. Local replacement clears
incremental state. Shared replacement installs a cycle-stable `Arc`, then bumps
the existing persistent-GC epoch so every Store restarts stale incremental
work.

In MVCC builds, all four state entry points now use the same sequence:

```text
active/suspended check
select policy + visibility authority
MVCC barrier
shared-region GC permit (when shared)
prepare full rebase
validate current persistent object identity/payload or absence
complete rebase
optional metadata-only view
policy callback over the current ObjectTable
```

Declaration order guarantees that the shared-region permit drops before the
MVCC permit on every return path. The permits remain live through the callback.
Both policy modes are quiescent and fully rebased. Historical payloads,
sidecars, synthetic object IDs, and historical roots are not exposed.

Non-MVCC builds use a separate compile-time path. It clones the same selected
policy, takes only the shared-region permit when needed, passes `None`, and
does not query `mvcc_mode` or touch any MVCC type. The incremental wrapper
discards state after every error, including errors after a collector has
consumed grey work.

The original collector implementations are now transaction-module-only
`*_uncoordinated` methods used by the default policy. Destructive ObjectTable
sweep helpers are likewise narrowed and explicitly named
`*_uncoordinated`; low-level tests opt into those names.

### TDD evidence

The first focused RED was the missing policy contract:

```text
E0405: cannot find trait TransactionPersistentGc
E0433: cannot find CurrentStatePersistentGc
E0433: cannot find GcMvccMode
```

After adding only the object-safe interface and default mode:

```text
mvcc_gc_policy_default_is_current_state_only_and_object_safe
1 passed; 0 failed
```

The next RED was the policy ownership boundary:

```text
E0599: no set_persistent_gc_policy_for_test on TransactionState
E0599: no set_persistent_gc_policy_for_test on TransactionRegionRuntime
```

After adding local/shared handles and dispatch:

```text
mvcc_gc_policy_ filter
3 passed; 0 failed
```

The completed focused suites prove all-four local/shared dispatch, shared
snapshot and pending-commit rejection, active/suspended rejection before the
callback, both permits live during the callback, release after all four
callback-error paths, metadata mode behavior, full rebase before callback for
both modes, current ObjectTable-only visibility, maintenance rollback, and
local/shared replacement invalidation:

```text
MVCC mvcc_gc_policy_ filter
13 passed; 0 failed

non-MVCC OCC mvcc_gc_policy_ filter
8 passed; 0 failed
```

The non-MVCC policy's `mvcc_mode` deliberately panics if queried; all four
direct callbacks still pass with no view.

### Task 12B verification

All MVCC commands used the complete Task 10 feature closure recorded above.
Fresh results:

```text
MVCC persistent_gc filter
48 passed; 0 failed

MVCC runtime::transaction::tests::mvcc_gc_ filter
32 passed; 0 failed

MVCC runtime::transaction::mvcc:: filter
33 passed; 0 failed

MVCC runtime::transaction::tests::mvcc_serializable_ filter
8 passed; 0 failed

MVCC runtime::transaction::tests::mvcc_commit_ filter
19 passed; 0 failed

complete MVCC feature-closure cargo check
exit 0

default non-MVCC cargo check
exit 0

non-MVCC OCC cargo check
exit 0

non-MVCC OCC persistent_gc filter, using the complete non-MVCC test closure
48 passed; 0 failed
```

A smaller non-MVCC unit-test feature list (`runtime,std,gc,cranelift,wat` plus
OCC) is sufficient for `cargo check` but not for this crate's complete `--lib`
test harness, whose unrelated serialization tests require cache, threads, and
other test features. Re-running the persistent-GC filter with the complete
non-MVCC closure passed as shown above.

No backend, durable format, commit linearization point, copy-on-write, or
transaction configuration behavior changed in Task 12B.
