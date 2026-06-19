# Five Concurrency-Control Implementation Roadmap

Date: 2026-06-19

## Goal

Implement five additional single-version transaction concurrency-control
schemes behind the existing compile-time policy switch:

1. Wound-Wait
2. Wait-Die
3. Strict Two-Phase Locking
4. Optimistic Validation Only
5. Pure Timestamp Ordering

The roadmap keeps the current transaction storage, recovery, and garbage
collection model intact. It deliberately leaves room for MVCC as the eventual
general-purpose concurrency-control architecture, but MVCC is not implemented
in this roadmap.

## Current Baseline

The runtime currently has:

- `LockBased`, the default owner-based optimistic-read/pessimistic-write policy
- `NoWaitAbort`, an owner-based policy that rejects conflicts without
  preemption
- `ConcurrencyControl`, the compile-time selected policy enum
- `ConcurrencyControlState`, the selected-policy runtime wrapper
- `TransactionConcurrencyControl`, the policy contract for single-version
  policies

The first implementation step should finish and commit the policy-contract
refactor before any new policy work lands. After that point, each new policy
should require:

1. a policy type implementing `TransactionConcurrencyControl`
2. one `ConcurrencyControl` enum case
3. one `ConcurrencyControlState` enum case
4. one Cargo feature
5. direct policy tests
6. selected-policy runtime tests

## Non-Goal: MVCC For This Roadmap

MVCC remains the ultimate target because it can provide true timestamp
visibility, snapshot reads, and better read/write concurrency. It should not be
implemented as part of these five policies.

MVCC needs a separate storage/GC/recovery roadmap because it requires:

- multiple committed versions per `GranuleId`
- read timestamp visibility rules
- commit timestamp assignment independent of transaction start order
- old-version garbage collection
- recovery-time version reconstruction
- integration with `tmemory`, object records, roots, globals, and tables

This roadmap should leave extension points for MVCC by keeping timestamp helper
naming explicit and avoiding public API decisions that assume one committed
version is the permanent model.

## Shared Groundwork

### Milestone 0: Finish The Single-Version Policy Contract

Complete the `TransactionConcurrencyControl` contract refactor before adding
new policies.

Required contract surface:

- acquire granule read
- acquire granule write
- validate read
- refresh read version
- release transaction
- release transaction with `Result`
- query current owner
- list read granules for a transaction

Acceptance criteria:

- `LockBased` and `NoWaitAbort` implement the full trait.
- `ConcurrencyControlState` delegates ordinary operations through the trait.
- test-only helpers remain outside the trait.
- a generic trait-contract test covers every trait method for both existing
  policies.

### Milestone 1: Timestamp Helpers

Add timestamp terminology without adding runtime timestamp state.

Recommended shape:

```rust
type TransactionTimestamp = TransactionId;

fn timestamp_for_transaction(transaction: TransactionId) -> TransactionTimestamp {
    transaction
}

fn is_older(requester: TransactionId, owner: TransactionId) -> bool {
    timestamp_for_transaction(requester) < timestamp_for_transaction(owner)
}

fn is_younger(requester: TransactionId, owner: TransactionId) -> bool {
    timestamp_for_transaction(requester) > timestamp_for_transaction(owner)
}
```

Use `TransactionId` as the timestamp for all timestamp-based policies in this
roadmap. A lower transaction id means an older transaction.

Keep these helpers private to the transaction concurrency module unless another
runtime layer needs them.

### Milestone 2: Feature Selection Scaffolding

Extend the one-policy-at-a-time feature guard.

Existing features:

- `transaction-cc-lockbased`
- `transaction-cc-nowait-abort`

New features:

- `transaction-cc-wound-wait`
- `transaction-cc-wait-die`
- `transaction-cc-strict-2pl`
- `transaction-cc-optimistic-validation`
- `transaction-cc-timestamp-ordering`

Acceptance criteria:

- enabling more than one transaction concurrency-control feature fails at
  compile time
- enabling none while `transaction` is enabled fails at compile time
- default feature selection remains `transaction-cc-lockbased`
- `TransactionConfig::default()` selects the configured policy

## Policy 1: Wound-Wait

### Semantics

Wound-Wait is timestamp based:

- the transaction timestamp is `TransactionId`
- older requester vs younger owner: requester wounds the owner
- younger requester vs older owner: requester receives a conflict
- reads remain optimistic unless a writer owns the granule
- writes remain pessimistic and take exclusive ownership
- commit validates optimistic reads against current versions
- commit and abort release ownership and read tracking

This policy is closest to the current `LockBased` id-priority behavior and
should be implemented first.

### Implementation Shape

Add:

- `WoundWait`
- `WoundWaitConflictKind`
- `ConcurrencyControl::WoundWait`
- `ConcurrencyControlState::WoundWait`

The policy state can match the current owner/read-version shape:

```rust
owners: BTreeMap<GranuleId, TransactionId>
read_versions: BTreeMap<(TransactionId, GranuleId), u64>
```

When an older requester wounds a younger owner, return the wounded transaction
id through the existing `Result<Option<TransactionId>>` path so the runtime can
use the existing cleanup path.

### Tests

Direct policy tests:

- older writer wounds younger owner
- older reader wounds younger writer before recording read version
- younger writer conflicts with older owner
- younger reader conflicts with older writer
- read version mismatch is still detected
- commit/abort release ownership and read tracking

Selected-policy tests:

- `transaction-cc-wound-wait` selects `ConcurrencyControl::WoundWait`
- shared runtime records wounded transactions consistently
- wounded transaction cleanup releases owned granules and allocated objects

## Policy 2: Wait-Die

### Semantics

Wait-Die is timestamp based, but the first version must not block runtime
threads.

- older requester vs younger owner: return `WouldWait`
- younger requester vs older owner: requester dies with an ordinary conflict
- no transaction preempts another transaction
- owner is unchanged after `WouldWait`
- commit and abort release ownership and read tracking

The runtime does not yet have wait queues, wakeups, or retry scheduling. In this
roadmap, "wait" is represented as a structured conflict result, not a blocking
operation.

### Implementation Shape

Add:

- `WaitDie`
- `WaitDieConflictKind`
- `ConcurrencyControl::WaitDie`
- `ConcurrencyControlState::WaitDie`

The current trait returns `Result<Option<TransactionId>>`, which can represent
"success" and "wounded owner" but not "would wait" cleanly. Add a narrow
structured conflict path before implementing Wait-Die.

Recommended shape:

```rust
enum TransactionConflictAction {
    AbortRequester,
    AbortOther(TransactionId),
    WouldWait(TransactionId),
}
```

Use this internally first. Public/runtime behavior can still surface an error
until there is a real wait/retry scheduler.

### Tests

Direct policy tests:

- older reader receives `WouldWait` against younger writer
- older writer receives `WouldWait` against younger writer
- younger reader dies against older writer
- younger writer dies against older writer
- owner is unchanged after `WouldWait`
- commit/abort release ownership and read tracking

Selected-policy tests:

- `transaction-cc-wait-die` selects `ConcurrencyControl::WaitDie`
- shared runtime leaves both transactions open after `WouldWait`
- no wounded transaction is recorded for `WouldWait`

## Policy 3: Strict Two-Phase Locking

### Semantics

Strict 2PL uses locks for both reads and writes:

- reads acquire shared read locks
- writes acquire exclusive write locks
- all locks are held until commit or abort
- multiple readers may hold the same granule
- writer conflicts with active readers
- reader conflicts with active writer
- read-to-write upgrade succeeds only when the requester is the sole reader
- no timestamp preemption

This policy gives a non-timestamp baseline and exercises a different lock-state
shape.

### Implementation Shape

Add:

- `StrictTwoPhaseLocking`
- `StrictTwoPhaseLockingConflictKind`
- `ConcurrencyControl::StrictTwoPhaseLocking`
- `ConcurrencyControlState::StrictTwoPhaseLocking`

The state should not reuse the single-owner-only map directly.

Recommended state:

```rust
readers: BTreeMap<GranuleId, BTreeSet<TransactionId>>
writers: BTreeMap<GranuleId, TransactionId>
read_versions: BTreeMap<(TransactionId, GranuleId), u64>
```

`owner_for_granule` should return the writer only. Runtime code that uses owner
queries for terminal-owner checks should not treat shared readers as exclusive
owners.

### Tests

Direct policy tests:

- multiple readers can hold the same granule
- writer conflicts with active readers
- reader conflicts with active writer
- writer can reacquire a granule it already owns
- read-to-write upgrade succeeds for sole reader
- read-to-write upgrade fails when other readers exist
- commit/abort release read and write locks

Selected-policy tests:

- `transaction-cc-strict-2pl` selects the policy
- shared runtime respects reader/writer exclusion
- generic transaction tests assert conflict occurrence without assuming
  timestamp winner behavior

## Policy 4: Optimistic Validation Only

### Semantics

This policy delays conflict detection as much as possible:

- reads record observed versions
- writes record observed versions but do not acquire ownership
- writes remain private in the transaction workspace until commit
- commit validates read granules and write granules
- commit fails if any written granule changed since it was first observed
- no snapshots and no old-version retention

This is still single-version concurrency control. It is not MVCC.

### Implementation Shape

Add:

- `OptimisticValidation`
- `OptimisticValidationConflictKind`
- `ConcurrencyControl::OptimisticValidation`
- `ConcurrencyControlState::OptimisticValidation`

The current trait can record read versions, but commit-time write validation
needs a small contract extension.

Recommended additions:

```rust
fn validate_write(
    &self,
    transaction: TransactionId,
    granule: GranuleId,
    current_version: u64,
) -> Result<()>;

fn write_granules_for_transaction(&self, transaction: TransactionId) -> Vec<GranuleId>;
```

Recommended state:

```rust
read_versions: BTreeMap<(TransactionId, GranuleId), u64>
write_versions: BTreeMap<(TransactionId, GranuleId), u64>
```

`acquire_granule_write` should record the observed version and return success
without taking ownership. The commit path must validate write granules before
publishing staged writes.

### Tests

Direct policy tests:

- reads record versions
- writes record versions without ownership
- read validation fails when version changes
- write validation fails when version changes
- commit validation checks both reads and writes
- commit/abort release read and write tracking

Runtime tests:

- two transactions can stage writes to the same granule
- first committer succeeds
- second committer fails if the granule version changed
- no terminal-owner assumption is required for optimistic writes

## Policy 5: Pure Timestamp Ordering

### Semantics

Pure Timestamp Ordering is timestamp based and single-version in this roadmap.

Use `TransactionId` as the timestamp:

- lower id means older transaction
- each granule tracks a read timestamp and write timestamp
- read succeeds only if `timestamp(transaction) >= write_timestamp(granule)`
- write succeeds only if `timestamp(transaction) >= read_timestamp(granule)` and
  `timestamp(transaction) >= write_timestamp(granule)`
- reads update the granule read timestamp
- committed writes update the granule write timestamp
- aborted writes do not update the committed write timestamp
- no Thomas write rule in the first version
- no snapshots and no old-version retention

This policy is intentionally last because it is less aligned with the current
owner-based runtime and needs durable reasoning about timestamp metadata.

### Implementation Shape

Add:

- `TimestampOrdering`
- `TimestampOrderingConflictKind`
- `ConcurrencyControl::TimestampOrdering`
- `ConcurrencyControlState::TimestampOrdering`

Recommended state:

```rust
read_timestamps: BTreeMap<GranuleId, TransactionTimestamp>
write_timestamps: BTreeMap<GranuleId, TransactionTimestamp>
read_versions: BTreeMap<(TransactionId, GranuleId), u64>
write_versions: BTreeMap<(TransactionId, GranuleId), u64>
```

The policy still needs version validation because storage remains
single-version. Timestamp checks decide ordering conflicts; version checks
catch real committed changes in the current runtime.

Commit needs a post-publication hook or an explicit commit-success path so the
policy can advance write timestamps only after writes are actually published.

Recommended addition:

```rust
fn commit_transaction_result(&mut self, transaction: TransactionId) -> Result<()>;
```

Keep this addition private to the runtime policy contract. It is not a public
embedding API.

### Tests

Direct policy tests:

- read from older transaction fails after newer committed write timestamp
- read updates read timestamp
- write from older transaction fails after newer read timestamp
- write from older transaction fails after newer write timestamp
- committed write advances write timestamp
- aborted write does not advance write timestamp
- version mismatch remains a conflict

Runtime tests:

- timestamp conflicts are deterministic by transaction id
- staged writes do not advance write timestamp until commit succeeds
- abort releases transaction-local tracking without corrupting timestamp maps

## Cross-Policy Runtime Work

### Conflict Result Cleanup

Wound-Wait already fits the existing "abort another transaction" path. Wait-Die
needs an explicit `WouldWait` outcome. Optimistic Validation and Timestamp
Ordering need commit-time validation of write granules.

Do this in small steps:

1. introduce an internal structured conflict action
2. keep existing error messages stable where possible
3. route runtime cleanup based on the action
4. add policy-specific selected-runtime tests

### Commit-Time Validation

Before Optimistic Validation and Timestamp Ordering, add generic commit-time
write validation support:

- enumerate written granules for the transaction
- retrieve current version for each granule
- validate through the selected policy before publishing
- release policy state only after commit or abort reaches the existing cleanup
  boundary

This work should not change `LockBased`, `NoWaitAbort`, Wound-Wait, Wait-Die,
or Strict 2PL semantics. Those policies can implement write validation as a
no-op or as the same owner/version checks they already enforce.

## Verification Strategy

Each policy should have three layers of coverage:

1. direct policy unit tests over `GranuleId`, `TransactionId`, and versions
2. `ConcurrencyControlState` selected-policy tests under the matching Cargo
   feature
3. runtime tests for store-local and shared-region transactions

Run the default policy suite after each milestone:

```bash
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo fmt --all -- --check
git diff --check
```

Run each alternate policy with the same support-feature shape:

```bash
cargo check -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,transaction-cc-wound-wait"

cargo test -p wasmtime --no-default-features \
  --features "anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-cc-wound-wait" \
  --lib transaction:: -- --format terse
```

Repeat the alternate-policy command for:

- `transaction-cc-wait-die`
- `transaction-cc-strict-2pl`
- `transaction-cc-optimistic-validation`
- `transaction-cc-timestamp-ordering`

## Recommended Implementation Order

1. finish and commit the single-version policy trait contract
2. add timestamp helper terminology
3. add Wound-Wait
4. add structured conflict action for `WouldWait`
5. add Wait-Die without blocking
6. add Strict 2PL shared-reader/exclusive-writer state
7. add commit-time write validation hooks
8. add Optimistic Validation Only
9. add timestamp metadata hooks
10. add Pure Timestamp Ordering

MVCC should start only after these policies are in place and the storage layer
has a separate version-retention and garbage-collection plan.

## Implementation Status

- Wound-Wait: implemented
- Wait-Die: implemented without blocking; `WouldWait` is a structured conflict action
- Strict Two-Phase Locking: implemented
- Optimistic Validation Only: implemented with commit-time write validation
- Pure Timestamp Ordering: implemented as single-version timestamp ordering
- MVCC: still deferred to a separate storage, recovery, and garbage-collection roadmap
