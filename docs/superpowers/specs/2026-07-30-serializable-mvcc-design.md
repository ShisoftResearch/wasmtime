# Serializable MVCC with Optimistic Concurrency Control Design

Date: 2026-07-30

Status: Approved design

## Summary

Add a compile-time-selected `transaction-mvcc` versioning and visibility feature
to the transaction extension. MVCC is orthogonal to concurrency control. The
first supported pairing is MVCC with the existing
`transaction-cc-optimistic-validation` policy, producing multiversion
optimistic concurrency control (MV-OCC).

The implementation uses multi-version reads, private copy-on-write workspaces,
optimistic commit-time certification, and parallel publication for disjoint
access sets. Commit timestamps determine visibility; short-lived commit latches
belong to OCC certification rather than to timestamp-ordering or lock-based
execution.

Although the original direction was snapshot isolation, the required guarantee
is stronger: serializability and opacity for STM applications. Writing
transactions validate their complete read and write sets while holding ordered
commit latches. This prevents write skew without a global commit mutex or an SSI
dependency graph. Read-only transactions remain lock-free.

MVCC history is volatile. Existing durable record formats, recovery decisions,
and storage-backend selection remain unchanged. Commit-time publication may use
narrow capacity-only tmemory helpers to durably prepare bytes in a staged growth
tail before publishing the logical size and commit LP. Those helpers are
feature-gated where they create or publish hidden writes. The underlying
capacity-bounded restore primitive remains available without MVCC so recovery
can undo a tail written by a different build. Ordinary logical APIs remain
bounded by the committed length, and non-MVCC workloads retain their existing
semantics. A recovered runtime treats the latest durable state as a new
timestamp-zero baseline.

Persistent object GC is independently pluggable. A current-state-only collector
uses a quiescent MVCC compatibility barrier; a future snapshot-aware collector
can consume a restricted MVCC view. MVCC version pruning remains separate from
persistent object reachability.

## Goals

- Provide serializable, opaque transactions over every existing `GranuleId`
  domain:
  - tmemory data and size;
  - tglobals;
  - ttable data and size;
  - transactional struct and array objects.
- Preserve private commit-time staging and read-your-own-writes.
- Allow disjoint writing transactions to validate and publish in parallel.
- Prevent write/write anomalies, lost updates, and read/write cycles such as
  write skew.
- Keep MVCC versioning independent from the exactly-one compile-time
  concurrency-control choice.
- Keep storage selection as an independent runtime choice among VMemory,
  file-backed memory, and DAX PMEM.
- Keep GC implementation selection independent from MVCC, concurrency control,
  and storage backend selection.
- Reuse existing durable publication, commit-LP, recovery formats, and backend
  selection, with only the capacity-only MVCC publication/recovery helpers
  described above.
- Confine MVCC-specific code to feature-gated modules and existing transaction
  helper boundaries.

## Non-goals

- Durable historical versions or time-travel queries.
- Transactions or snapshots that survive process restart.
- Serializable Snapshot Isolation dependency-graph tracking.
- MVCC pairings with lock-based, timestamp-ordering, or other concurrency
  policies in the first milestone. The interfaces must permit these future
  pairings without changing the version tables.
- A new concurrent, snapshot-aware persistent object collector in this
  milestone.
- Changes to Wasm parsing, validation, or Cranelift lowering.
- New durable log records or recovery formats.
- Backend-specific MVCC implementations.
- Forced abort of long-running transactions to make GC progress.

## Configuration Matrix

Exactly one existing `transaction-cc-*` feature remains required in every
transaction build. MVCC does not participate in that exactly-one group. Add the
independent `transaction-mvcc` feature.

The first milestone supports `transaction-mvcc` only when
`transaction-cc-optimistic-validation` is selected. Other MVCC/CC combinations
fail with a clear compile-time diagnostic until their policy adapters exist.
The default remains single-version `transaction-cc-lockbased`.

Each resulting artifact can select any supported storage backend through
`TransactionConfig`:

| Versioning feature | Compile-time concurrency control | Initial support | Runtime-selectable tmemory backend |
| --- | --- | --- | --- |
| Disabled | Any existing CC | Yes | VMemory, file-backed, or DAX PMEM |
| `transaction-mvcc` | Optimistic validation | Yes | VMemory, file-backed, or DAX PMEM |
| `transaction-mvcc` | Lock-based, timestamp ordering, or another CC | Reserved for a policy adapter | VMemory, file-backed, or DAX PMEM |

The GC implementation is a third, independent policy axis. It is selected
through the persistent-GC policy interface or factory, not inferred from
`transaction-mvcc`, a `transaction-cc-*` feature, or `TMemoryBackend`.

## Architecture

Transaction helpers use two compile-selected facades:

```text
transaction helpers
   |                          |
   v                          v
SelectedTransactionVisibility    SelectedConcurrencyControl
   |-- SingleVersionVisibility      |-- LockBased
   `-- MvccVisibility               |-- OptimisticValidation
                                     `-- other existing policies
```

`SingleVersionVisibility` is a zero-allocation pass-through to current backend
values. `MvccVisibility` is selected only by `transaction-mvcc`. Builds without
that feature do not compile version-chain payloads, snapshot registries, or
MVCC GC adapters.

The concurrency facade remains selected by exactly one `transaction-cc-*`
feature. Under the initial supported pairing, `OptimisticValidation` supplies
the ordered shared/exclusive commit latches and full access-set certification.
Timestamp assignment remains an MVCC visibility concern; it is not
timestamp-ordering concurrency control.

`MvccVisibility` uses an `MvccRuntime`:

```rust
struct MvccRuntime {
    coordinator: MvccCoordinator,
    memories: MemoryVersionTable,
    memory_sizes: MemorySizeVersionTable,
    globals: GlobalVersionTable,
    tables: TableVersionTable,
    table_sizes: TableSizeVersionTable,
    objects: ObjectVersionTable,
}
```

A shared `TransactionRegionRuntime` owns this state for multi-store
transactions. A store-local transaction runtime owns the same abstraction when
no shared region is attached.

Commit latches and validation state live in the selected OCC authority rather
than in `MvccRuntime`. `MvccCoordinator` owns only visibility clocks, snapshot
registrations, atomic commit records, and version-collection coordination.

The typed sidecar tables are logically domain-owned but may initially be
co-located behind one runtime lock. Their interfaces permit later independent
sharding. They do not use a cross-domain value enum.

The MVCC layer sits above storage backends. Tmemory sidecars contain logical
granule snapshots, and current values are installed through backend-neutral
operations. A growth-tail publication can reserve backing capacity and
read/write/restore within that capacity while the logical length remains
unchanged; only the MVCC publication path can create such hidden writes.
Recovery may restore their existing undo records across MVCC and non-MVCC
builds. Ordinary reads, writes, metadata access, and non-MVCC commits remain
logical-length bounded. No durable record or recovery format is added, and
nothing in the MVCC algorithm branches on VMemory versus file-backed memory
versus DAX PMEM.

### Chosen concurrency-control policy

The first MVCC implementation uses optimistic concurrency control:

- transaction execution takes no concurrency locks;
- reads record their granules and writes remain private;
- OCC certifies the complete access set at commit;
- OCC uses short-lived ordered commit latches to keep certification valid
  through publication.

This is MV-OCC. It is not timestamp-ordering concurrency control: timestamps
select visible versions but do not apply timestamp-ordering conflict rules. It
is not lock-based execution: locks are acquired only for OCC certification and
released after commit or abort.

The MVCC visibility facade exposes snapshot and version metadata through a
policy-neutral certification interface. Future 2PL or timestamp-ordering
adapters can implement that interface without replacing the typed sidecar
tables.

## Core Types

### Snapshot and commit timestamps

Use a monotonic runtime-local `u64` timestamp. `TransactionId` remains an
identity and is not overloaded as a visibility timestamp.

At transaction begin, the coordinator briefly enters the publication-clock
critical section, captures the current visible timestamp, and registers it as
an active snapshot. Capturing and registering occur atomically with respect to
version-GC horizon calculation and persistent-GC barrier entry.

The transaction workspace owns the snapshot registration. Suspending and
restoring a workspace preserves the same snapshot. Commit, abort, or
conflict-driven cleanup unregisters it exactly once.

### Commit record

All versions prepared by one transaction reference one shared runtime-only
commit record:

```rust
enum CommitState {
    Pending,
    Committed(CommitTimestamp),
    Aborted,
}

struct Version<T> {
    commit: Arc<CommitRecord>,
    value: T,
}
```

The state transition is atomic with release/acquire visibility. A single
`Pending -> Committed(timestamp)` transition exposes all of the transaction's
domain versions. Readers skip pending and aborted entries.

Commit timestamps are assigned only during final visibility publication. A
short publication-clock critical section allocates the next timestamp, marks
the record committed, and advances the visible timestamp. Durable data
publication is outside this critical section, so disjoint commits can finish in
any order without timestamp holes.

### Version chains

Each typed table stores newest-first `VersionChain<T>` values. The payload types
are:

- tmemory granule: the existing 64-byte granule snapshot;
- tmemory size: `u64`;
- global: `GlobalSnapshot`;
- table granule: up to 16 `TableElementSnapshot` values;
- table size: `u64`;
- object: `ObjectPayload`.

The logical `ObjectId` is the only object-table identity. `ObjectVersionTable`
is indexed by `ObjectId` and stores historical payloads behind that identity.
Versions are not object-table slots, do not receive `ObjectId`s, and are not
first-class persistent-GC objects.

At runtime startup, recovered current state is the timestamp-zero baseline. A
first access can read that baseline without eagerly populating a chain. Before
the first commit that modifies an unversioned granule, MVCC captures the current
value as the predecessor to the new pending version.

The check for an existing chain and the timestamp-baseline read form one
operation under a short domain-sidecar metadata guard. Commit preparation uses
the corresponding write guard when installing the predecessor and pending
entry. A reader therefore either finishes reading the old baseline before
preparation begins or observes the prepared chain and selects its committed
predecessor. There is no interval in which it can mistake a pending physical
installation for the baseline.

## Transaction Semantics

### Begin

Beginning a transaction:

1. Fails if a persistent-GC barrier is active.
2. Captures the current visible timestamp.
3. Registers that timestamp in the active-snapshot multiset.
4. Stores the registration in the transaction workspace.

If a commit completed before begin, the new snapshot includes it. A commit
whose atomic visibility transition occurs after begin is excluded.

### Read

Reads are lock-free at the transaction level:

1. Return the transaction's staged value when present.
2. Otherwise select the newest sidecar version whose commit record is
   `Committed(ts)` with `ts <= snapshot_ts`.
3. When no version chain exists, read the current timestamp-baseline value.

The short sidecar metadata guard needed to make baseline fallback atomic is not
retained after the value is copied and is not part of the commit-latch set.

All granules involved in one operation use the same transaction snapshot.
Memory and table bounds checks use snapshot-visible sizes plus private staged
growth. Partial writes start from the snapshot-visible granule before modifying
the private staged copy.

Every read records its `GranuleId` in the workspace read set. Reads performed
through a staged write still preserve the access information needed for commit
validation.

### Write

Writes remain private until commit and use the existing staged maps and
transaction-local object machinery. Execution-time reads and writes take no
transaction locks under MVCC.

Blind writes record the written `GranuleId` even if it is absent from the read
set. Transaction-local object promotion prepares initial object versions that
share the transaction's commit record.

### Serializable commit

Read-only transactions need no commit record or commit latches. They serialize
at their snapshot timestamp.

A writing transaction:

1. Flushes pending transaction helper scratch into its private workspace.
2. Collects the complete read and write sets.
3. Sorts the union by `GranuleId`.
4. Asks the selected optimistic-validation policy to acquire a shared commit
   latch for each read-only granule and an exclusive commit latch for each
   written granule. Acquisition always follows the sorted order.
5. Validates every accessed granule:
   - the newest committed version of each read granule must be no newer than the
     transaction snapshot;
   - the newest committed version of each blind-write granule must be no newer
     than the transaction snapshot.
6. Creates one pending commit record and appends pending versions to all
   affected typed chains.
7. Executes existing current-state and durable publication, including the
   existing single durable commit LP.
8. Completes required runtime installation.
9. Enters the short publication-clock critical section and atomically commits
   the shared record with the next timestamp.
10. Releases commit latches, unregisters the snapshot, and runs budgeted
    version pruning.

Commit latches remain held from validation through the visibility transition.
Disjoint access sets therefore publish in parallel. Overlapping read-only sets
share latches. A read/write or write/write overlap orders the commits and causes
a stale validator to abort.

### Why this prevents write skew

Consider transactions that both read `A` and `B`, where one writes `A` and the
other writes `B`. Their commit latch sets overlap as shared/exclusive pairs.
Because both acquire the entire set in the same order, one obtains a valid
commit order. After it publishes, the other observes a read version newer than
its snapshot and aborts.

More generally, a writer serializes at its atomic commit-record transition.
Holding latches for every read/write dependency while validating prevents a
conflicting writer from changing the validated state before that point.
Read-only transactions serialize at their snapshot. This supplies
serializability, and stable committed snapshots ensure that even transactions
that later abort never observe a partially committed state, supplying opacity.

### Conflict granularity

Use the existing conflict units:

- one 64-byte tmemory granule;
- one tmemory-size granule per memory;
- one whole tglobal;
- one 16-element ttable granule;
- one ttable-size granule per table;
- one whole object payload per logical `ObjectId`.

Range access records every touched data granule and the size granule used for
bounds. This covers the address-space predicates exposed by the transaction
extension.

## Failure and Durability Rules

MVCC history is a volatile visibility layer around the existing publication
protocol.

Before modifying current state, a prepared commit retains predecessor values in
its version chains and tracks installation progress. If publication fails
before the durable LP:

- mark the commit record aborted;
- restore any already-installed current values from the committed
  predecessors;
- remove or opportunistically prune aborted pending entries;
- release latches and unregister the snapshot;
- run existing transaction-local object cleanup.

Existing durable undo or loose-end recovery remains responsible for a process
crash before the LP. Live-process rollback is necessary as well so that a later
MVCC rebase does not adopt an aborted physical value.

Once the durable LP succeeds, the transaction cannot become aborted. Any later
installation or cleanup error follows the existing "committed durably but
cleanup failed" path. The runtime must complete remaining installation from
the prepared values and atomically mark the commit record committed before
returning that error.

For transactions with no durable participants, successful current-state
installation followed by the commit-record transition is the commit decision.
A pre-transition failure restores predecessor values.

After restart, recovery reconstructs only the latest committed current state.
All version chains, commit records, latches, timestamps, and active snapshots
start fresh.

## Version Collection

Version collection is owned by MVCC, not by persistent object GC.

The coordinator tracks the oldest active snapshot and a budgeted cursor across
the typed sidecar tables. Transaction completion runs an opportunistic pruning
step.

With active snapshots, each chain retains:

- every pending entry;
- every committed entry newer than the oldest active snapshot;
- the newest committed entry at or before the oldest active snapshot.

Aborted entries are removable immediately after no installation rollback
depends on them.

With no active snapshots or pending commits, pruning may collapse histories
toward the installed current state. A complete rebase:

1. Ensures each current domain value equals its newest committed version.
2. Drops all historical chains and obsolete commit records.
3. Sets the runtime baseline timestamp to the current visible timestamp.
4. Removes unused version metadata and asks the OCC authority to retire unused
   granule-latch entries.

A long-running or suspended snapshot may retain history indefinitely. This
milestone reports GC as busy rather than aborting such a transaction.

## Pluggable Persistent GC

Persistent GC advertises an MVCC capability independently of concurrency and
storage selection:

```rust
enum GcMvccMode {
    CurrentStateOnly,
    SnapshotAware,
}
```

Persistent-GC implementations are supplied through an internal
`TransactionPersistentGc` policy interface covering mark, incremental step,
sweep, and compaction entry points. The interface reports `GcMvccMode`; the
current mark/sweep implementation is the default policy. Its factory or
configuration is independent from the MVCC feature, selected concurrency
feature, and tmemory backend.

### Current-state-only collector

The current persistent mark/sweep implementation declares
`CurrentStateOnly`. In an MVCC build, a compatibility adapter:

1. Acquires the GC lifecycle lock.
2. Requires zero active snapshots and zero pending terminal commits.
3. Prevents new transactions from beginning.
4. Performs a complete MVCC version rebase.
5. Invokes the current collector over current roots and current object
   payloads only.

The collector never sees historical versions or sidecar edges.

In a build without `transaction-mvcc`, the same collector is called directly
with no MVCC registry, view, or barrier-adapter overhead.

### Snapshot-aware collector

A future collector may declare `SnapshotAware` and receive a restricted
`MvccGcView`. The view exposes the active horizon and safe traversal of retained
sidecar payloads without representing versions as `ObjectTable` identities.

The first milestone defines this contract and a conformance probe but does not
implement a new concurrent collector.

### Incremental collection

The existing persistent-GC epoch rules remain. Each incremental maintenance
step enters the appropriate compatibility adapter. A commit between steps
invalidates saved mark state, as it does today. A full mark/sweep holds its
permit for the complete operation.

## Integration Surface

Add feature-specific code in focused modules:

- `concurrency/mvcc_optimistic.rs`: the MVCC certification adapter for the
  selected optimistic-validation policy;
- `mvcc/version_chain.rs`: commit records and generic chains;
- `mvcc/coordinator.rs`: clocks, snapshots, atomic records, and publication;
- `mvcc/domains.rs`: six typed sidecar tables;
- `mvcc/gc.rs`: version collector and persistent-GC adapter;
- `visibility.rs`: compile-selected visibility facade.

Focused integration changes are expected in:

- root and Wasmtime `Cargo.toml` feature declarations;
- transaction `config.rs`, `concurrency/mod.rs`, and the optimistic-validation
  policy's feature-gated certification hooks;
- `TransactionState` and `TransactionWorkspace`;
- `TransactionRegionRuntime`;
- transaction libcalls at existing read and commit helper boundaries;
- `ObjectTable` current-payload snapshot and installation hooks;
- transaction tests and concurrency feature-matrix commands.

Do not change:

- parser, validator, or Cranelift lowering;
- durable log formats;
- recovery formats or decision rules;
- non-OCC concrete concurrency-control implementations;
- existing single-version optimistic-validation behavior when
  `transaction-mvcc` is disabled.

Backend code may expose the narrow capacity-only primitives required for
commit-time MVCC growth-tail publication and cross-build undo application.
Creation/publication entry points remain MVCC-feature-gated; the
capacity-bounded restore primitive must compile without MVCC so recovery can
apply existing undo bytes behind the logical length. This narrow recovery
execution change must not alter recovery decisions, relax ordinary logical
bounds, or change non-MVCC publication behavior.

## Verification

### Unit tests

- Stable repeated reads while newer commits publish.
- Read-your-own-staged-writes.
- Pending and aborted versions are invisible.
- A shared commit record atomically exposes mixed-domain values.
- Timestamp-zero baseline creation and runtime rebase.
- Horizon pruning retains exactly the required predecessor and newer versions.
- Snapshot registration survives workspace suspend/restore.
- Ordered mixed shared/exclusive latch acquisition has no deadlock cycle.

### Serializable concurrency schedules

- Blind write/write conflict aborts one writer.
- Lost-update schedule aborts one writer.
- Classic two-record write skew aborts at least one writer.
- A writer whose read dependency changes aborts even when its write set is
  disjoint from the changing transaction.
- Read-only transactions remain lock-free and consistent.
- Disjoint writing transactions overlap terminal publication.
- A mixed tmemory/global/table/object transaction has no partially visible
  state.
- Multi-store transactions share timestamps, histories, and latches.

### Domain coverage

- Partial and cross-granule tmemory stores.
- Snapshot-visible memory sizes, staged growth, and bounds failures.
- Ttable element granules, size changes, and range operations.
- Scalar and reference tglobals.
- Struct and array payload history.
- Transaction-local object promotion and abort cleanup.
- Persistent roots published through globals and tables.

### GC coverage

- Opportunistic pruning with and without active snapshots.
- A current-state-only collector cannot enter while a snapshot or prepared
  commit is active.
- The compatibility adapter fully rebases before invoking the collector.
- A mock snapshot-aware collector receives only the restricted MVCC view.
- Builds without `transaction-mvcc` invoke persistent GC directly.
- Incremental mark state restarts after an intervening commit epoch.

### Durability and recovery

- Failure before LP leaves versions invisible and restores live current state.
- Crash before LP uses existing loose-end recovery and restores old durable
  state.
- Failure after LP still finalizes the commit record before reporting cleanup
  failure.
- Crash after LP recovers the new state.
- Restart exposes the latest state as a fresh timestamp-zero baseline.
- Run applicable scenarios with VMemory, file-backed memory, and research-mode
  DAX PMEM.

### Build matrix

- Every existing `transaction-cc-*` feature compiles independently without
  MVCC.
- `transaction-mvcc` plus `transaction-cc-optimistic-validation` compiles.
- `transaction-mvcc` without a selected CC fails.
- Initially unsupported MVCC/CC pairings fail with a clear diagnostic.
- Selecting zero or multiple concurrency features fails.
- Every supported versioning/concurrency-control build can instantiate every
  supported storage backend.
- The default build remains lock-based.

Add a small serializable-MVCC reference model for generated schedules, then use
thread barriers and fault injection against the real implementation for atomic,
latch, and LP cut-point behavior.

## Acceptance Criteria

- The write-skew regression aborts at least one participant and preserves the
  application invariant.
- No transaction observes a pending or partially published mixed-domain
  commit.
- Disjoint writers can be inside terminal publication concurrently.
- Current-state-only persistent GC never runs while an MVCC snapshot can
  observe history.
- MVCC version pruning and persistent object reachability remain separate.
- MVCC and concurrency-control selection remain separate policy axes, with
  MVCC+OCC as the first supported pairing.
- All supported backends remain selectable under every supported build.
- Existing behavior without `transaction-mvcc`, durable formats, and recovery
  tests remain unchanged.
