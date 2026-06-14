# Persistent Object GC 9B/9C Tombstone-Less Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox syntax for tracking; checked items reflect implemented status.

**Goal:** Keep Wave 9B commit-coupled marking, then implement Wave 9C recovery-time persistent object GC without durable tombstones.

**Architecture:** Wave 9A marker code is refactored into reusable object-GC machinery. Wave 9B adds volatile commit-barrier marking. Wave 9C follows the Makalu/Ralloc-style tradeoff: committed object data, roots, and layout metadata are durable; object-table entries, mark bits, line marks, free lists, reclaim queues, and dead-object state are rebuilt by recovery-time reachability.

**Tech Stack:** Rust, Wasmtime runtime internals, `ObjectTable`, `ObjectHeap`, `RecoveredRegion`, `VMemoryBlockRegion`, file-backed recovery tests, `cargo test`.

---

## Scope

This plan supersedes both older tombstone plans:

- `docs/shisoft/2026-06-14-persistent-object-gc-9b-9c-implementation-plan.md`
- `docs/shisoft/2026-06-14-persistent-object-gc-9b-9c-gc-blocks-plan.md`

Wave 9C in this plan does not implement durable tombstones, object-data
tombstone records, GC tombstone blocks, `ObjectId` reuse, persistent block reuse,
compaction, moving objects, or a persisted object table.

The plan does implement recovery-time filtering of object winners, volatile
runtime sweep/reporting, and reconstruction hooks for allocator liveness
metadata. Durable block/chunk retirement is a separate follow-up wave because
recovery must have durable block-generation or checkpoint metadata before old
object-data blocks can be safely reused.

## Design Basis From Makalu And Ralloc

- **Recoverability:** after recovery, volatile allocator and object-table
  metadata must describe all and only persistent objects reachable from
  persistent roots. Crashes may create temporary leaks, but recovery must not
  expose unreachable objects as live or allow one persistent storage range to be
  allocated for two live objects.
- **Ownership before use:** future persistent block/chunk allocation should
  durably claim storage before handing it to the mutator. If a crash happens
  before the object becomes reachable, recovery-time GC excludes it from the
  rebuilt object table and returns the space to volatile reclaim knowledge once
  block retirement is safe.
- **Typed tracing:** Ralloc filter functions map to our persistent type/layout
  metadata. Recovery traces only fields/elements that the recovered layout marks
  as `ObjectId` references. Missing layout metadata is a recovery error.
- **Volatile fast path:** mark bits, line marks, free lists, reclaim queues,
  per-thread caches, and object table entries are fast runtime structures. They
  are rebuilt; they are not maintained as durable GC metadata.

## File Map

- Create: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
  - Owns persistent GC report types, mark state, commit deltas, recovery-filter
    helpers, and volatile sweep reports.
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
  - Re-exports object-GC types, exposes narrow object-table helpers, and adds
    recovery-time reachable rebuild helpers.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
  - Carries object-data block locations in `RecoveredObjectWinner` so recovery
    GC can reconstruct volatile block liveness summaries.
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify: `docs/shisoft/transactional-wasm-remaining-work-roadmap.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

## Task 1: Preserve The Wave 9A Refactor

**Files:**
- Create: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_object_marker -- --format terse`

- [x] **Step 1: Move marker-only code into `object_gc.rs`**

Move these existing types from `transaction.rs` into `object_gc.rs`:

```rust
PersistentRootErrorKind
PersistentRootError
DanglingObjectRefKind
DanglingObjectRef
PersistentObjectMarkReport
PersistentObjectMarker
```

Keep `PersistentObjectMarker::mark` as the compatibility entry used by current
tests.

- [x] **Step 2: Add budgeted mark state**

Add these runtime-only types in `object_gc.rs`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PersistentGcBudget {
    objects: usize,
}

impl PersistentGcBudget {
    pub(crate) fn objects(objects: usize) -> Self {
        Self { objects }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentGcStepReport {
    pub(crate) scanned_objects: usize,
    pub(crate) enqueued_objects: usize,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PersistentGcState {
    reachable: BTreeSet<ObjectId>,
    grey: VecDeque<ObjectId>,
    dangling_refs: Vec<DanglingObjectRef>,
    invalid_roots: Vec<PersistentRootError>,
}
```

`PersistentObjectMarker::mark` should construct `PersistentGcState`, enqueue
validated roots, run mark steps until the grey queue is empty, and return the
same report shape as Wave 9A.

- [x] **Step 3: Verify marker compatibility**

Run:

```bash
cargo test -p wasmtime --lib persistent_object_marker -- --format terse
```

Expected:

```text
test result: ok. 7 passed; 0 failed; 0 ignored
```

- [x] **Step 4: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_gc.rs
git commit -m "Refactor persistent object marker core"
```

## Task 2: Keep Wave 9B Commit-Coupled Marking

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_gc_commit -- --format terse`

- [x] **Step 1: Add commit delta types**

Add these types to `object_gc.rs`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct PersistentObjectEdge {
    pub(crate) from: ObjectId,
    pub(crate) to: ObjectId,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentGcCommitDelta {
    pub(crate) new_roots: BTreeSet<ObjectId>,
    pub(crate) edges: BTreeSet<PersistentObjectEdge>,
}
```

Object publication commits should produce edges from the committed object to
each persistent `ObjectId` in its payload. Root commits from `tglobal` and
`ttable` produce `new_roots`.

- [x] **Step 2: Add commit barrier observation**

Add:

```rust
impl PersistentGcState {
    pub(crate) fn observe_commit_delta(
        &mut self,
        objects: &ObjectTable,
        delta: &PersistentGcCommitDelta,
        budget: PersistentGcBudget,
    ) -> Result<PersistentGcStepReport> {
        for &root in &delta.new_roots {
            self.enqueue_reachable(root);
        }
        for edge in &delta.edges {
            if self.reachable.contains(&edge.from) {
                self.enqueue_reachable(edge.to);
            }
        }
        self.mark_step(objects, budget)
    }
}
```

If `A -> B` is published and `A` is unmarked, do not enqueue `B` immediately;
scanning `A` in the same cycle will see the committed edge.

- [x] **Step 3: Add focused tests**

Add tests named:

```text
persistent_gc_commit_barrier_enqueues_new_root
persistent_gc_commit_barrier_enqueues_child_when_owner_is_marked
persistent_gc_commit_barrier_ignores_child_when_owner_is_unmarked
```

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_commit -- --format terse
```

Expected: all three tests pass.

- [x] **Step 4: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_gc.rs
git commit -m "Add commit-coupled persistent marking"
```

## Task 3: Add Recovery-Time Reachable Winner Filtering

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_gc_recovery_filter -- --format terse`

- [x] **Step 1: Write recovery filtering tests**

Add tests in `transaction.rs` named:

```text
persistent_gc_recovery_filter_installs_only_reachable_winners
persistent_gc_recovery_filter_keeps_reachable_child_not_explicit_root
persistent_gc_recovery_filter_reports_unreachable_winners
```

The first test should construct recovered winners for:

```text
root object 41 -> child object 42
child object 42 -> no refs
garbage object 43 -> no refs
root ids = [41]
```

After recovery-time filtering, `ObjectTable` should contain 41 and 42, but not
43.

Add one failure test named:

```text
persistent_gc_recovery_filter_rejects_missing_type_layout
```

It should prove typed tracing is mandatory by giving a recovered object winner
whose `type_layout_id` is not present in the recovered layout registry and
asserting recovery returns an error.

- [x] **Step 2: Add recovery filter report**

Add to `object_gc.rs`:

```rust
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentRecoveryGcReport {
    pub(crate) mark: PersistentObjectMarkReport,
    pub(crate) installed_winners: Vec<u64>,
    pub(crate) skipped_unreachable_winners: Vec<u64>,
}
```

- [x] **Step 3: Add reachable rebuild helper**

Add to `ObjectTable` in `transaction.rs`:

```rust
fn rebuild_reachable_from_recovered_object_winners(
    &mut self,
    recovered_type_layouts: &TypeLayoutRegistry,
    winners: &[crate::runtime::vm::RecoveredObjectWinner],
    root_object_ids: &[u64],
) -> Result<PersistentRecoveryGcReport> {
    self.rebuild_from_recovered_object_winners(recovered_type_layouts, winners)?;

    let roots = root_object_ids
        .iter()
        .copied()
        .map(|object_index| ObjectId { object_index });
    let mark = PersistentObjectMarker::mark(self, roots)?;
    let reachable_winners = winners
        .iter()
        .filter(|winner| {
            mark.reachable.contains(&ObjectId {
                object_index: winner.object_id,
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    let skipped_unreachable_winners = winners
        .iter()
        .filter(|winner| {
            !mark.reachable.contains(&ObjectId {
                object_index: winner.object_id,
            })
        })
        .map(|winner| winner.object_id)
        .collect::<Vec<_>>();
    let installed_winners = reachable_winners
        .iter()
        .map(|winner| winner.object_id)
        .collect::<Vec<_>>();

    self.rebuild_from_recovered_object_winners(recovered_type_layouts, &reachable_winners)?;

    Ok(PersistentRecoveryGcReport {
        mark,
        installed_winners,
        skipped_unreachable_winners,
    })
}
```

This helper deliberately rebuilds all winners once to get a traceable object
graph, then clears and rebuilds only reachable winners. It writes no persistent
storage.

The first implementation may use this two-phase rebuild because it stays inside
volatile runtime structures. A later implementation can mark directly over the
temporary winner map without first installing unreachable winners.

- [x] **Step 4: Expose a test wrapper**

Add in the existing `#[cfg(test)] impl ObjectTable` block:

```rust
fn rebuild_reachable_from_recovery_for_test(
    &mut self,
    recovered_type_layouts: &TypeLayoutRegistry,
    winners: &[crate::runtime::vm::RecoveredObjectWinner],
    root_object_ids: &[u64],
) -> Result<PersistentRecoveryGcReport> {
    self.rebuild_reachable_from_recovered_object_winners(
        recovered_type_layouts,
        winners,
        root_object_ids,
    )
}
```

- [x] **Step 5: Verify**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_recovery_filter -- --format terse
cargo test -p wasmtime --lib persistent_object_marker -- --format terse
```

Expected: recovery-filter tests and existing marker tests pass.

- [x] **Step 6: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_gc.rs
git commit -m "Filter recovered persistent objects by reachability"
```

## Task 4: Carry Recovered Object Block Locations

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_gc_recovery_locations -- --format terse`

- [x] **Step 1: Write location tests**

Add tests named:

```text
persistent_gc_recovery_winner_records_data_location
persistent_gc_recovery_filter_reports_reachable_record_locations
persistent_gc_recovery_filter_reports_unreachable_record_locations
```

The first test should recover a committed object publication and assert:

```rust
assert!(winner.data_block > 0);
assert!(winner.record_len > 0);
```

- [x] **Step 2: Extend `RecoveredObjectWinner`**

Modify `RecoveredObjectWinner` in `recovery.rs`:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveredObjectWinner {
    pub(crate) object_id: u64,
    pub(crate) version: u32,
    pub(crate) kind: u16,
    pub(crate) type_layout_id: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) record_len: u64,
    pub(crate) record_bytes: Vec<u8>,
}
```

When constructing the winner in `replay_object_winners`, set:

```rust
data_block: winner.data_block,
data_offset: winner.data_offset,
record_len: object_header.record_len,
```

Update existing test helper constructors to include these fields.

- [x] **Step 3: Add recovered liveness report types**

Add to `object_gc.rs`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct PersistentRecoveredRecordLocation {
    pub(crate) object_id: ObjectId,
    pub(crate) version: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) record_len: u64,
}
```

Extend `PersistentRecoveryGcReport` with:

```rust
pub(crate) reachable_record_locations: Vec<PersistentRecoveredRecordLocation>,
pub(crate) unreachable_record_locations: Vec<PersistentRecoveredRecordLocation>,
```

- [x] **Step 4: Populate liveness report locations**

In `rebuild_reachable_from_recovered_object_winners`, classify each winner into
reachable or unreachable location vectors using the mark report.

This report is the bridge to Makalu-style allocator reconstruction. It gives
future block/line reconstruction code enough information to rebuild volatile
line marks, block live-byte summaries, free lists, and reclaim queues without
persisting those structures.

- [x] **Step 5: Verify**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_recovery_locations -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
```

Expected: recovery location tests and existing persistence tests pass.

- [x] **Step 6: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_gc.rs
git commit -m "Report recovered persistent object liveness"
```

## Task 5: Add Volatile Sweep Report

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_gc_volatile_sweep -- --format terse`

- [x] **Step 1: Write volatile sweep tests**

Add tests named:

```text
persistent_gc_volatile_sweep_reports_unreachable_objects
persistent_gc_volatile_sweep_removes_unreachable_slots_when_requested
persistent_gc_volatile_sweep_does_not_persist_dead_state
```

The removal test should allocate root and garbage objects, mark from the root,
apply volatile sweep, and assert that the garbage object has no live slot.

- [x] **Step 2: Add sweep report types**

Add to `object_gc.rs`:

```rust
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentVolatileSweepReport {
    pub(crate) removed_objects: Vec<ObjectId>,
    pub(crate) retained_objects: Vec<ObjectId>,
}
```

- [x] **Step 3: Add object-table volatile sweep helper**

Add to `ObjectTable`:

```rust
fn apply_volatile_persistent_sweep(
    &mut self,
    mark: &PersistentObjectMarkReport,
) -> Result<PersistentVolatileSweepReport> {
    let mut report = PersistentVolatileSweepReport::default();
    for object_id in self.persistent_object_ids() {
        if mark.reachable.contains(&object_id) {
            report.retained_objects.push(object_id);
        } else {
            self.clear_live_slot_for_volatile_gc(object_id)?;
            report.removed_objects.push(object_id);
        }
    }
    self.rebuild_free_list_holes()?;
    Ok(report)
}
```

`clear_live_slot_for_volatile_gc` must only mutate the volatile slot table and
runtime heap handles. It must not write object data, transaction logs,
tombstones, or block metadata.

- [x] **Step 4: Verify**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_volatile_sweep -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
```

Expected: volatile sweep tests pass and transaction tests remain green.

- [x] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_gc.rs
git commit -m "Add volatile persistent object sweep"
```

## Task 6: End-To-End File-Backed Tombstone-Less Recovery Test

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_gc_recovery_file_backed -- --format terse`

- [x] **Step 1: Write file-backed test**

Add a test named:

```text
persistent_gc_file_backed_recovery_rebuilds_only_reachable_object_table
```

The test should:

1. Create a file-backed durable log.
2. Commit object `root` and object `garbage`.
3. Publish a durable root that references `root`, or pass recovered root IDs
   from `RecoveredRegion`.
4. Reopen and recover the region.
5. Call `ObjectTable::rebuild_reachable_from_recovery_for_test`.
6. Assert `root` is installed and `garbage` is skipped.

- [x] **Step 2: Verify the test**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_file_backed_recovery_rebuilds_only_reachable_object_table -- --format terse
```

Expected: the test passes without any tombstone or GC metadata block.

- [x] **Step 3: Guard against tombstone regressions**

Add this assertion to the test or a companion test:

```rust
assert!(
    report
        .skipped_unreachable_winners
        .contains(&garbage.object_index)
);
```

Also run:

```bash
rg -n "GcTombstone|persistent_object_tombstone|TX_OBJECT_FLAG_DELETED" crates/wasmtime/src/runtime
```

Expected: no matches unless a future optional tombstone feature is explicitly
introduced behind a separate design.

- [x] **Step 4: Verify recoverability invariant**

In the file-backed test, assert the report shape:

```rust
assert!(report.installed_winners.contains(&root.object_index));
assert!(report.skipped_unreachable_winners.contains(&garbage.object_index));
assert!(
    report
        .reachable_record_locations
        .iter()
        .any(|location| location.object_id == root)
);
assert!(
    report
        .unreachable_record_locations
        .iter()
        .any(|location| location.object_id == garbage)
);
```

This verifies the Ralloc-style recoverability rule at the object-table level:
after recovery filtering, live metadata names reachable objects and omits
unreachable winners.

- [x] **Step 5: Run persistence tests**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_recovery_file_backed -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
```

Expected: file-backed recovery test and existing persistence tests pass.

- [x] **Step 6: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs
git commit -m "Test tombstone-less persistent object recovery"
```

## Task 7: Documentation And Final Verification

**Files:**
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify: `docs/shisoft/transactional-wasm-remaining-work-roadmap.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [x] **Step 1: Record implemented status**

Add this status text to the docs:

```markdown
Wave 9C uses tombstone-less Makalu/Ralloc-style recovery. Persistent storage
contains committed object records, roots, type/layout metadata, and scan-critical
region metadata. Recovery selects live object-data winners, marks from
persistent roots, rebuilds the volatile object table only for reachable
objects, and reconstructs auxiliary allocator state. Durable block reuse remains
deferred until block-generation or checkpoint retirement metadata exists.
```

- [x] **Step 2: Run final verification**

Run:

```bash
cargo fmt --check
cargo test -p wasmtime --lib persistent_gc -- --format terse
cargo test -p wasmtime --lib persistent_object_marker -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
cargo test -p wasmtime --lib -- --format terse
git diff --check
```

Expected results:

```text
persistent_gc filters: all pass
persistent_object_marker filters: all pass
transaction filter: all pass
transaction_persistence test: all pass
wasmtime lib tests: all pass with the existing ignored count only
git diff --check: no output
```

- [x] **Step 3: Commit docs**

Run:

```bash
git add docs/shisoft/transactional-wasm-runtime-core-design.md docs/shisoft/transactional-wasm-remaining-work-roadmap.md docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Document tombstone-less persistent object GC"
```

## Completion Criteria

- Wave 9A `PersistentObjectMarker::mark` remains available and passes existing
  tests.
- Wave 9B commit barriers advance the volatile marker without scanning staged
  transaction workspaces.
- Wave 9C recovery rebuilds the volatile object table only for reachable
  recovered object winners.
- No durable GC tombstone records, tombstone blocks, object-data tombstone
  payloads, or per-object dead-state metadata are added.
- Recovery reports reachable and unreachable recovered object record locations
  so future allocator work can reconstruct volatile liveness summaries.
- Runtime sweep can remove unreachable entries from volatile indices without
  writing persistent dead-object state.
- Persistent block reuse remains disabled until a block/chunk-retirement
  checkpoint design exists.
- `ObjectId` reuse remains disabled.

## Rollback Plan

- If the `object_gc.rs` refactor causes broad compile churn, keep
  `PersistentObjectMarker` in `transaction.rs` and add only the recovery filter
  helpers needed by 9C.
- If recovery-time filtering needs more type-layout plumbing than expected,
  keep the temporary all-winner rebuild approach in the private helper and
  restrict the first file-backed test to type layouts already recovered by the
  existing persistence tests.
- If volatile sweep conflicts with object-table free-list behavior, keep the
  sweep report-only mode and split slot removal into a separate task.
- If block-location reporting requires too many test helper updates, add
  `data_block`, `data_offset`, and `record_len` behind `#[cfg(test)]` first,
  then widen the fields after the recovery tests pass.
