# Persistent Object GC Wave 1 Design

Date: 2026-06-14

## Implementation Status

Implemented on 2026-06-14 in
`crates/wasmtime/src/runtime/transaction.rs` as `PersistentObjectMarker`.

The implementation follows the mark-only scope below:

- explicit `ObjectId` roots only
- `ObjectTable` plus layout-guided tracing as the graph source
- reachable, unreachable, dangling-ref, and invalid-root report sets
- active transaction rejection through the transaction thread-local state
- no mutation of the object table or durable storage

Focused implementation tests use:

```bash
cargo test -p wasmtime --lib persistent_object_marker -- --format terse
```

## Goal

Add the first persistent-object GC proof point without touching durable
reclamation. The first collector is a logical `ObjectId` graph marker: it proves
that recovered roots, volatile object-table rebuild, persistent type-layout
metadata, and layout-guided object tracing work together.

This is intentionally not a full storage collector. It does not compact, move,
delete, tombstone, recycle blocks, update line marks, or persist free-list
metadata.

## Context

The branch now has the foundation a persistent collector needs:

- `ObjectId` is stable persistent object identity.
- Persistent object records carry `type_layout_id`.
- Type layout metadata is stored in region metadata space.
- Recovery rebuilds the volatile object table from committed object winners.
- Recovery can reconstruct root `ObjectId`s from committed `TGlobal`/`TTable`
  root-bearing records.
- Persistent object refs are traceable through recovered type layouts.

Wasmtime's ordinary GC remains separate. Its `GcHeap`, `VMGcRef`, roots, and
barriers are designed for volatile store-local objects. The persistent object
collector should borrow its discipline, such as explicit roots, no-GC scopes,
and collector/runtime boundaries, but it must not use Wasmtime `GcHeap` as
durable object storage or `VMGcRef` as persistent identity.

## Approaches Considered

### Approach A: Logical mark-only collector

The collector takes explicit root `ObjectId`s, walks committed persistent object
edges, and returns a report containing reachable objects, unreachable persistent
objects, and dangling references.

This is the recommended first step. It tests the hard infrastructure without
risking persistent corruption from premature deletion or block reuse.

### Approach B: Immediate mark/sweep with volatile index cleanup

The collector marks reachable objects and removes unreachable objects from the
volatile object table, leaving durable records untouched.

This is useful later, but it is still a behavioral mutation. It can hide bugs in
the marker by making objects disappear from the runtime before we have durable
tombstone and recovery semantics.

### Approach C: Durable mark/sweep with tombstones

The collector appends GC-owned tombstone metadata for unreachable object-data
versions and allows recovery to omit covered objects from the rebuilt volatile
object table.

This is the right durable direction, but it requires a deletion design:
tombstone version ordering, root consistency, GC block scanning, and chunk/block
reclamation rules. It is too much for the first proof point.

## Wave 1 Scope

Implement a logical persistent object graph collector, tentatively named
`PersistentObjectMarker`.

Inputs:

- a borrowed `ObjectTable`
- an explicit root set of `ObjectId`s
- an execution precondition that no transaction is active

Outputs:

- `reachable: BTreeSet<ObjectId>`
- `unreachable_persistent: BTreeSet<ObjectId>`
- `dangling_refs: Vec<DanglingObjectRef>`
- `invalid_roots: Vec<PersistentRootError>`

The marker only considers live persistent objects. Volatile transaction-local
objects are ignored except when they appear as dangling or unsupported edges in
test scenarios. The first implementation should not mutate the object table.

## Root Model

Wave 1 roots are explicit inputs. Tests should pass roots from:

- direct test root sets
- recovered `root_object_ids` from file-backed recovery

The collector should not discover roots by scanning Wasmtime stacks, ordinary
Wasmtime GC roots, or host `Rooted<T>` values. Those belong to ordinary Wasmtime
GC, not persistent reachability.

Later waves can add durable root registries, runtime root integration, and
transaction workspace roots.

## Trace Model

Tracing uses the existing persistent object infrastructure:

1. Start from each root `ObjectId`.
2. Confirm the object exists and is persistent.
3. Call the object-table trace path, which resolves the object's
   `type_layout_id` and uses layout-guided tracing for persistent records.
4. Add each referenced `ObjectId` to the work queue.
5. Report a dangling reference if a traced `ObjectId` has no live persistent
   slot.

Cycles are handled by the reachable set. Shared children are marked once.
Scalar-only objects produce no outgoing edges.

## Error Handling

The marker should distinguish diagnostics:

- missing root object, reported in `invalid_roots`
- root exists but is not persistent, reported in `invalid_roots`
- dangling persistent reference from a live object
- layout/tracing error while scanning an object
- attempted collection while a transaction is active

For Wave 1, dangling references and invalid roots should be reported in the
output rather than silently ignored. Layout/tracing failures and active
transaction precondition failures should return an error because they mean the
mark pass cannot safely interpret the graph.

## Transaction Boundary

Wave 1 collection is stop-the-world by precondition. It must not run while an
active transaction exists.

Rationale:

- staged writes may not be committed durable graph edges
- abort may remove staged references
- promotion maps are not persistent roots yet
- write ownership and optimistic reads should not be mixed with graph marking

Later collectors can treat active transaction read sets, write sets, staged
payloads, and promotion maps as temporary roots, but that is explicitly out of
scope for Wave 1.

## Non-Goals

Wave 1 does not:

- reclaim durable storage
- publish GC tombstone metadata
- free object-table slots
- rewrite roots
- move object records
- compact chunks
- update Immix line marks
- persist free-list or allocator state
- integrate with ordinary Wasmtime `GcHeap`
- replace the current `VMGcRef -> ObjectId` bridge

## Testing Plan

Unit tests should cover:

- root reaches a chain: `A -> B -> C`
- root reaches a cycle: `A -> B -> A`
- two roots share one child
- scalar-only object has no outgoing edges
- unreachable persistent object is reported
- missing root is reported
- dangling child ref is reported
- layout/tracing error returns an error
- recovered file-backed object table plus recovered roots can be marked
- active transaction precondition rejects marking

Useful focused commands:

```bash
cargo test -p wasmtime --lib persistent_object_marker -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
```

## Future Roadmap

Wave 2 should add commit-coupled incremental marking before any sweep or durable
deletion work. Persistent graph mutation only becomes visible at transaction
commit, so the persistent collector does not need ordinary per-field write
barriers on staged writes. Instead, the transaction commit path should produce a
commit barrier record from the committed graph delta:

- newly published or updated persistent object records
- newly published persistent roots from `TGlobal`/`TTable`
- promoted objects that became persistent during commit
- persistent ref edges introduced by those publications

The first barrier policy should use direct child marking: when commit publishes
an edge `A -> B` and `A` is already known reachable in the active mark cycle,
enqueue `B` into the persistent GC grey queue. If `A` is not yet reachable, no
immediate action is needed because `A` will be scanned if the current cycle
later reaches it. Root publications enqueue their root `ObjectId`s directly.

Commit should also perform a bounded marking minibatch. The collector keeps a
volatile `PersistentGcState` with a mark epoch, reachable set, and grey queue.
Each successful commit calls the barrier observer and then scans a small budget
of queued objects. Budgets can initially be counted in scanned objects; later
versions may budget by edges or payload bytes. This ties GC progress to
transaction activity and avoids a periodic background collector for the first
incremental version.

The minibatch should run only after the commit graph is frozen and committed for
runtime visibility. The current Wave 1 marker still rejects arbitrary active
transactions. The future commit-coupled marker may run in a commit epilogue or
under a store-level commit/GC gate, but it must not scan mutable staged
workspaces as if they were committed graph state.

This design is conservative. Removing a root or deleting an edge during an
active mark cycle may keep the old target marked until the next cycle. That is
acceptable before durable reclamation because over-marking only delays
collection; it does not corrupt persistent reachability.

If the workload becomes read-heavy with few commits, commit-coupled marking may
not make progress. A later explicit maintenance API such as
`persistent_gc_step(budget)` can reuse the same minibatch scanner without
changing the commit-barrier design.

Wave 3 should add a non-durable sweep/report mode only after commit-coupled
incremental marking proves reliable. It may remove unreachable objects from
volatile runtime indices or report reclaim candidates to a test-only harness,
but it should still avoid durable deletion.

Wave 4 should design durable GC tombstones and recovery behavior. Tombstones
belong in GC-owned metadata blocks, not in per-thread user transaction logs.
Each fixed-size tombstone entry records an `ObjectId`, the object-data version
that was proven unreachable, a GC epoch, and a checksum. Recovery should first
select the highest committed live object-data version from transaction logs,
then apply the highest valid GC tombstone for that `ObjectId`; a tombstone only
suppresses a live record when `tombstone.target_version >= live.version`.

Wave 5 should integrate Immix-style block/line reclamation using the existing
Wizard-shaped block region. Immix should be a storage reuse policy under stable
`ObjectId` identity, not a reason to change persistent reference encoding.

LXR should remain a later research branch after we have data showing whether its
locality and reclamation model fits persistent Wasm workloads.
