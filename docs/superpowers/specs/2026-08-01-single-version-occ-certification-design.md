# Single-Version OCC Commit Certification Design

Date: 2026-08-01

Status: Proposed correction after benchmark-discovered serializability failure

## Summary

Single-version `transaction-cc-optimistic-validation` currently validates its
read and write versions before terminal commit but holds no reservation from
validation through physical installation. Two transactions with overlapping
reads and disjoint writes can therefore both validate the same state and both
commit, producing classic write skew.

Fix the policy by reusing and generalizing the existing atomic per-granule
certification authority. An OCC transaction atomically reserves its complete
access set with shared modes for read-only granules and exclusive modes for
written granules, revalidates all recorded versions while holding the permit,
and retains the permit through terminal installation and cleanup. This makes
the validation-to-install interval serializable without a global commit lock
and without reducing concurrency for disjoint access sets.

## Evidence and Root Cause

The forced benchmark schedule starts two transactions from `(1,1)`. Each reads
both granules and stages a clear to a different granule. Under single-version
OCC, the schedule reproducibly completes both logical operations and observes
`(0,0)`.

The failure is not a benchmark rendezvous or retry defect:

1. Both transactions finish their reads before either stages a write.
2. `commit_single_tmemory_for_benchmark` captures current versions and calls
   `validate_active_reads_with` and `validate_active_writes_with`.
3. `OptimisticValidation::owner_for_granule` returns `None`, and terminal
   admission records only the transaction ID.
4. No reservation spans validation through physical installation, so both
   transactions can validate version zero and then install disjoint writes.

The same ordering exists in the real single-version `tfunc` commit path and in
`TransactionState::commit_with_read_validation`; fixing only the benchmark
helper would conceal the policy defect.

## Considered Approaches

### 1. Per-granule commit certification — selected

Atomically reserve the complete read/write set, revalidate while reserved, and
hold the RAII permit through terminal cleanup. Read-only granules use shared
reservations; written granules use exclusive reservations.

This prevents write skew while allowing transactions with disjoint access sets
to certify and commit concurrently. It also matches the mechanism already used
by serializable MVCC plus optimistic validation.

### 2. Global OCC commit mutex — rejected

Serializing validation and installation under one global mutex would prevent
write skew with a small implementation surface, but it would also serialize
disjoint commits and invalidate the benchmark's scaling comparison. It is
incompatible with the requirement to avoid harming non-conflicting non-MVCC
workloads.

### 3. Install, revalidate, and roll back — rejected

Installing first and validating afterward creates a period where invalid state
is physically visible. File-backed commits also have a durable linearization
point after which rollback is forbidden. A post-install scheme would require a
larger terminal protocol and still provide weaker isolation than certification
before installation.

## Architecture

### General certification authority

Generalize the existing MVCC optimistic certification types and diagnostics so
they describe optimistic commit certification rather than MVCC specifically.
Compile the authority whenever `transaction-cc-optimistic-validation` is
selected, whether or not `transaction-mvcc` is enabled.

The authority retains its existing behavior:

- sort reservations by granule;
- collapse a granule present in both sets to exclusive mode;
- check and install the complete reservation set atomically under one mutex;
- wait on a condition variable when any reservation conflicts;
- release every reservation through an RAII permit and notify waiters.

Atomic all-at-once reservation avoids latch-order deadlocks. Shared/read and
exclusive/write modes make the write-skew cycle conflict while preserving
parallel certification for disjoint transactions.

`OptimisticValidation` owns this authority in every OCC build. Shared-region
transactions acquire from the shared `TransactionRegionRuntime` authority;
standalone transaction states acquire from their local selected policy.

### Certification and validation order

For a single-version OCC commit:

1. Finish operations that can finalize the transaction's access sets,
   including persistent-reference promotion.
2. Obtain the complete active read and write sets.
3. Acquire the optimistic certification permit atomically.
4. Re-read and validate every recorded read and write version while the permit
   is held.
5. Enter terminal commit.
6. Publish durable undo, install current values, publish the durable commit LP
   when present, apply version bumps, and finish transaction cleanup.
7. Drop the certification permit only after terminal cleanup completes.

If validation fails, abort the active transaction using the existing cleanup
path and then release the permit. No physical write is allowed before all
version validation succeeds.

The permit must cover every transactional domain represented by `GranuleId`,
including tmemory, globals, tables, sizes, and objects. Certification is based
on the transaction's complete read/write sets, not only its staged records.

### Commit-path coverage

Use one `TransactionState` acquisition interface in all single-version OCC
commit entry points:

- the real `transaction_commit_single_version_impl` used by `tfunc`;
- `TransactionState::commit_with_read_validation` and its callers;
- the test-only `commit_single_tmemory_for_benchmark` adapter.

Other compile-time concurrency-control policies receive no new reservation or
global lock. Serializable MVCC retains its existing timestamp validation and
uses the same generalized certification authority without changing its
certification order.

### State and failure handling

Prefer an operation-local RAII permit where the commit function can retain it
through cleanup. If an existing retryable terminal state must outlive a stack
frame, store the permit in that terminal state rather than releasing it early.

Authority poisoning, validation failures, conflict cleanup failures, durable
publication failures, and terminal cleanup failures remain explicit errors.
Existing conflict diagnostics remain stable for retry classification. A
failed certification or validation attempt must leave no reservation, active
snapshot, terminal marker, or durable/current-state mutation behind.

## Testing

Use TDD and preserve the benchmark's failing schedule as the primary
regression:

- the exact single-version OCC two-worker, 20-schedule benchmark test fails
  with `(0,0)` before the fix and passes afterward with at least one conflict;
- a real single-version OCC `tfunc` forced-cycle test prevents `(0,0)`, proving
  the production commit path is fixed rather than only the benchmark helper;
- overlapping reservations block and the later transaction revalidates after
  the first releases its permit;
- disjoint read/write sets acquire permits concurrently and commit without an
  anomalous conflict or global serialization;
- validation and injected cleanup errors release permits and leave lifecycle
  counts at zero;
- existing serializable MVCC certification tests remain green;
- all seven single-version policy write-skew tests and the exact MVCC test pass;
- default, OCC-only, and MVCC transaction unit and persistence suites pass.

Performance remains observational. No throughput threshold is added; the
correctness requirements are the absence of write skew, exact conflict/cleanup
behavior, and preserved disjoint concurrency.

## Repository Surface

Expected production changes are limited to the optimistic concurrency module,
the shared region/runtime routing needed to acquire its permit, and the real
single-version commit entry points. Test-only benchmark code gains only the
calls and regressions needed to exercise the corrected policy.

No public API, runtime-selectable concurrency-control axis, new feature, global
commit mutex, storage-backend change, GC change, or unrelated refactoring is
part of this correction.

## Acceptance Criteria

- Single-version OCC prevents the forced `(0,0)` write-skew result.
- Certification spans complete version validation through terminal cleanup.
- Disjoint OCC transactions can certify and commit concurrently.
- Real `tfunc`, direct transaction-state, and benchmark commit paths share the
  corrected policy semantics.
- MVCC plus OCC retains first-writer-wins certification and snapshot timestamp
  validation.
- Every other compile-time CC build retains its existing behavior.
- VMemory and file-backed durability/rollback rules remain unchanged.
- All certification permits, snapshots, and terminal state are cleaned on
  success, conflict, and injected failure paths.
