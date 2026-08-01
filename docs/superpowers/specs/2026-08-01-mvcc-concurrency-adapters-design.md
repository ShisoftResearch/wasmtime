# MVCC Concurrency-Control Adapters Design

Date: 2026-08-01

Status: Approved design

## Summary

Extend the transaction runtime so the independently compiled
`transaction-mvcc` visibility feature can be paired with any one of the seven
existing compile-time concurrency-control policies. MVCC remains optional. A
transaction build always selects exactly one `transaction-cc-*` feature, while
`transaction-mvcc` independently selects multi-version rather than
single-version visibility.

Every MVCC pairing provides serializable, opaque transactions. Snapshot reads
remain lock-free. The selected concurrency-control policy governs writer
admission and writer/writer conflict behavior, while a common MVCC commit-time
certifier validates the complete read/write set and prevents lost updates,
write skew, and other read/write cycles. The certifier is held through the
existing durable publication and terminal cleanup sequence.

The implementation does not alter non-MVCC execution. Storage backend and
persistent-GC policy selection remain independent axes.

## Goals

- Support `transaction-mvcc` with each existing CC feature:
  - `transaction-cc-lockbased`;
  - `transaction-cc-nowait-abort`;
  - `transaction-cc-optimistic-validation`;
  - `transaction-cc-strict-2pl`;
  - `transaction-cc-timestamp-ordering`;
  - `transaction-cc-wait-die`;
  - `transaction-cc-wound-wait`.
- Preserve exactly-one compile-time CC selection in both MVCC and non-MVCC
  builds.
- Keep `transaction-mvcc` independently enableable and disableable.
- Preserve lock-free historical reads and private copy-on-write workspaces.
- Make the selected policy meaningfully control writer collisions without
  weakening serializability.
- Prevent write skew and lost updates for every supported pairing.
- Preserve parallel commits for disjoint access sets.
- Preserve the existing copy-on-write durability, commit-LP, recovery, storage,
  and MVCC GC behavior.
- Leave every existing non-MVCC policy behavior unchanged.

## Non-goals

- Combining more than one `transaction-cc-*` feature in one build.
- Runtime selection between compiled concurrency-control policies.
- Durable historical versions or snapshots that survive restart.
- Replacing the existing policy implementations with seven independent native
  multi-version algorithms.
- Making snapshot readers wait for current-version writers.
- Changing durable log formats, recovery decisions, Wasm syntax, validation,
  or compiler lowering.

## Configuration Contract

The two compile-time axes remain separate:

```text
SelectedTransactionVisibility        SelectedConcurrencyControl
--------------------------------     -------------------------------
transaction-mvcc off                 exactly one transaction-cc-*
  -> SingleVersionVisibility           -> existing policy

transaction-mvcc on                  exactly one transaction-cc-*
  -> MvccVisibility                    -> MVCC writer adapter
```

The build rules are:

1. `transaction` requires exactly one `transaction-cc-*` feature.
2. Selecting two or more CC features remains a compile error.
3. `transaction-mvcc` requires `transaction`, but does not imply or select a CC.
4. `transaction-mvcc` accepts any of the seven valid single-CC selections.
5. Disabling `transaction-mvcc` compiles the existing single-version behavior
   for the selected CC without MVCC sidecars, snapshots, certification, or GC
   barriers.

This creates fourteen primary verification builds: seven single-version and
seven MVCC pairings. Every build retains runtime selection among VMemory,
file-backed memory, and supported DAX PMEM configurations.

## Chosen Architecture

### Common MVCC certification overlay

All MVCC writers acquire a common ordered read/write-set certification permit
at commit. This authority is compiled whenever `transaction-mvcc` is enabled;
it is no longer owned only by the optimistic-validation configuration and is
not excluded by strict 2PL.

For every granule in the complete read/write set, certification requires:

```text
latest_committed_timestamp(granule) <= transaction_snapshot_timestamp
```

The ordered permit prevents a conflicting transaction from passing the same
check until the first transaction has either published or aborted. The
timestamp condition then rejects the later transaction if any value it read or
wrote changed after its snapshot. Writes remain first-writer-wins, and the
complete read-set check prevents write skew.

Read-only transactions use a stable registered snapshot and create no current
state, so they do not require commit certification.

### Selected-policy writer adapters

MVCC reads record access-set membership but do not call execution-time CC read
acquisition. A historical reader must not block a current writer or be treated
as a reader of the current single-version value.

The first staged write to a granule calls the selected CC's writer adapter:

| Selected CC | MVCC writer-admission behavior |
| --- | --- |
| Optimistic validation | Record the private write; defer conflicts to certification. |
| Lock-based | Acquire and retain the existing exclusive writer ownership. |
| No-wait abort | Acquire writer ownership or abort immediately on collision. |
| Strict 2PL | Acquire and retain an exclusive write lock through terminal completion; snapshot reads remain lock-free, yielding the multi-version 2PL form. |
| Wait-die | Apply the existing age ordering to writer collisions. |
| Wound-wait | Apply the existing age ordering and conflict-abort behavior to writer collisions. |
| Timestamp ordering | Apply the existing timestamp writer admission and validation state without registering a historical snapshot read as a current-value read. |

The policy adapters do not replace MVCC timestamp validation. Their role is to
shape writer contention and lifecycle behavior; the common certifier is the
serializability boundary shared by all pairings.

### Ownership

The common certification authority belongs to the same local or shared region
runtime that owns MVCC visibility. This avoids coupling it to the selected CC
implementation and ensures all stores participating in one region certify
against one authority.

The transaction workspace owns:

- its registered MVCC snapshot;
- its complete read and write sets;
- selected-policy writer reservations;
- at most one MVCC certification permit;
- the existing pending/terminal commit state.

All resources use RAII-style terminal cleanup and are released exactly once on
commit, abort, conflict-driven abort, injected failure, or dropped workspace.

## Transaction Flow

### Begin and execution

1. Register an MVCC snapshot atomically with respect to visibility publication
   and GC horizon calculation.
2. Resolve reads from the typed version sidecars at that snapshot and record
   read granules without taking CC read locks.
3. Keep writes private in the existing workspace.
4. On the first write to each granule, call the selected writer adapter and
   record the granule in both the read and write sets because partial writes
   consume snapshot state.

### Commit

For a read-only transaction, unregister the snapshot and finish the selected
policy lifecycle without entering the writer pipeline.

For a writing transaction:

1. Confirm that no selected-policy conflict has already aborted it.
2. Complete any selected-policy writer validation required before publication.
3. Acquire the ordered common MVCC certification permit for the complete access
   set.
4. While holding the permit, validate every accessed granule against the
   snapshot timestamp.
5. Prepare the existing copy-on-write durable/current-state publication.
6. Cross the existing durable commit linearization point.
7. Install current values and atomically publish all MVCC versions under one
   commit record and visibility timestamp.
8. Complete the selected CC's commit lifecycle.
9. Unregister the snapshot and release writer reservations and certification.

Disjoint writers acquire disjoint certification reservations and may proceed in
parallel. Conflicting writers retain the selected policy's admission behavior,
then the MVCC timestamp check resolves any remaining snapshot conflict.

### Failure handling

Before the durable commit LP, any error aborts prepared MVCC versions, restores
prepared current-state changes, unregisters the snapshot, and releases both
policy and certification resources.

After the durable commit LP, the existing force-completion rules remain in
effect: the transaction must finish current-state and MVCC publication rather
than report an abort. Certification and writer ownership remain held until that
terminal path completes or hands recoverable cleanup to the existing deferred
mechanism.

Selected-policy cleanup errors continue to compose with the primary operation
error. A failed adapter must not leak locks, conflict-aborted identities,
timestamp metadata, certification reservations, pending MVCC commits, or active
snapshots.

## Durability and Recovery

The adapter layer adds no durable record. It reuses the existing staged
copy-on-write publication, durable transaction log, commit-LP ordering, storage
incarnation checks, and recovery procedures.

MVCC history remains volatile. Restart reconstructs the latest durable current
state as timestamp zero regardless of which CC policy compiled the recovered
runtime. Durable images therefore remain compatible between MVCC and non-MVCC
builds and among all seven concurrency-control builds.

## GC

Historical values remain behind typed sidecar version tables and never become
first-class object-table entries or reachability roots. MVCC continues to prune
versions opportunistically and at the GC compatibility barrier.

The persistent collector remains independently pluggable:

- a current-state-only collector receives no historical-version view and runs
  only at a quiescent MVCC barrier;
- an MVCC-compliant collector receives the existing restricted timestamp and
  horizon view;
- a non-MVCC build invokes the selected collector without compiling MVCC
  sidecars or compatibility barriers.

The selected concurrency-control policy does not choose or alter the collector.

## Alternatives Rejected

### Delegate all MVCC reads to the selected single-version CC

This would be mechanically simple but would make snapshot readers acquire locks
against current state and would associate historical values with current-value
version counters. Lock-based pairings would lose the primary MVCC read benefit,
and existing validation APIs would describe the wrong version.

### Implement seven independent native multi-version algorithms

Dedicated MVTO, MV2PL, and age-ordered implementations could remove some
redundant certification, but would duplicate visibility, failure, durability,
and lifecycle logic. The larger correctness and testing surface is not justified
until benchmark evidence identifies a common-certifier bottleneck.

### Ignore the selected CC in MVCC builds

Using the common certifier for every build while bypassing the selected policy
would compile, but the resulting combinations would be aliases rather than real
policy choices. Writer admission is therefore delegated to explicit adapters.

## Testing and Benchmarking

The implementation is accepted only if:

- all seven non-MVCC library and persistence configurations still pass;
- all seven MVCC pairings compile and pass library and persistence tests;
- every pairing prevents forced two-record write skew and lost updates;
- read-only snapshots remain non-blocking during a concurrent writer;
- writer collisions exhibit the selected policy's expected abort, ownership,
  age-order, or timestamp behavior;
- disjoint writers can enter certification and publication concurrently;
- pre-LP rollback and post-LP force-completion tests pass for every adapter;
- VMemory and file-backed tests pass for all pairings, with DAX exercised by the
  existing supported tests;
- GC barriers prune sidecar versions to the safe horizon without exposing them
  to object reachability;
- compile-fail tests reject zero or multiple CC features but accept
  `transaction-mvcc` with each single CC;
- a no-MVCC build does not compile MVCC runtime state;
- the benchmark policy matrix expands from eight configurations to fourteen:
  seven single-version policies and their seven MVCC pairings.

The benchmark retains read-only, disjoint-write, hot-key, and forced write-skew
workloads over both VMemory and file-backed storage. Results must report the
visibility mode and selected CC separately so MVCC remains a feature axis rather
than a synthetic concurrency-control policy name.
