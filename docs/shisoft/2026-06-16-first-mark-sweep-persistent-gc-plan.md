# First Mark-Sweep Persistent GC Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** turn the existing persistent-object marker, recovery filter, and volatile sweep scaffolding into the first operational non-moving mark-sweep persistent GC.

**Architecture:** The collector is tombstone-less and reachability-based. It marks committed persistent `ObjectId` graphs from durable roots, rejects sweep when graph-integrity diagnostics are present, removes unreachable objects from volatile runtime indices, and optionally retires whole-dead object-data chunks through existing block-generation metadata. Persistent object data is not moved or rewritten in this milestone.

**Tech Stack:** Rust, Wasmtime `transaction` feature, `ObjectTable`, `PersistentGcState`, file-backed `tmemory` block region, existing `TObjectPub` durable logs.

---

## Starting Point

The branch already has the main pieces:

- `crates/wasmtime/src/runtime/transaction/object_gc.rs`
  - `PersistentObjectMarker`
  - `PersistentGcState`
  - `PersistentObjectMarkReport`
  - `PersistentVolatileSweepReport`
  - `PersistentRecoveryGcReport`
  - `PersistentGcCommitDelta`
- `crates/wasmtime/src/runtime/transaction.rs`
  - `ObjectTable::rebuild_reachable_from_recovered_object_winners`
  - `ObjectTable::persistent_recovery_gc_report`
  - `ObjectTable::apply_volatile_persistent_sweep`
  - `TransactionState::persistent_root_ids`
  - `TransactionState::observe_persistent_gc_commit_delta_after_commit`
  - `retire_unreachable_object_chunks_for_test`
- `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
  - `FileBackedMemoryBlockRegion::retire_whole_dead_object_chunks`
  - block generation checks for stale durable entries

This plan finishes the first collector layer. It does not implement Immix, LXR, object compaction, line-level reuse, durable tombstones, background threads, or `ObjectId` reuse.

## File Responsibilities

- Modify `crates/wasmtime/src/runtime/transaction/object_gc.rs`
  - owns GC reports, graph-integrity checks, and incremental mark state.
- Modify `crates/wasmtime/src/runtime/transaction.rs`
  - owns `ObjectTable` sweep integration, `TransactionState` root integration, and unit tests that use transaction-private helpers.
- Modify `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
  - only if tests expose missing whole-dead chunk retirement behavior.
- Modify `crates/wasmtime/tests/transaction_persistence.rs`
  - add end-to-end file-backed recovery/GC smoke tests if the existing in-module tests are not enough.
- Modify `docs/shisoft/transactional-wasm-runtime-core-design.md`
  - document the first mark-sweep collector contract.
- Modify `docs/shisoft/transactional-wasm-remaining-work-roadmap.md`
  - mark this milestone as implemented and leave Immix/LXR as future work.
- Modify `docs/shisoft/transactional-wasm-implementation-log.md`
  - record what was implemented and what remains mocked/deferred.

## Collector Invariants

- Persistence is by reachability from committed durable roots.
- A transaction-local object that aborts is discarded, not collected.
- A committed object that becomes unreachable is persistent garbage until runtime GC or recovery filtering removes it from the live runtime view.
- Runtime sweep only mutates volatile metadata: object-table slots, live indexes, free-list holes, and volatile block summaries.
- The first collector never appends GC tombstones.
- Durable object chunks may be retired only when every current object record in the chunk is unreachable.
- Sweep must not proceed if the mark report contains invalid roots or dangling persistent references.
- Collector entrypoints must reject active transactions.

## Task 1: Add Full Mark-Sweep Report and Sweep Safety Checks

**Files:**

- Modify: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
- Test: `crates/wasmtime/src/runtime/transaction.rs`

- [ ] **Step 1: Write failing tests for unsafe sweep diagnostics**

Add these tests near the existing persistent GC marker tests in `crates/wasmtime/src/runtime/transaction.rs`:

```rust
#[test]
fn persistent_mark_sweep_rejects_invalid_root_before_sweep() {
    let mut objects = ObjectTable::default();
    let live = objects
        .allocate_persistent_struct_for_gc_ref(0x650, vec![ObjectValue::I32(1)])
        .unwrap();
    let missing = ObjectId { object_index: 99_999 };

    let err = objects
        .persistent_mark_sweep_from_roots_for_test([missing])
        .unwrap_err();

    assert!(
        err.to_string()
            .contains("persistent object sweep requires a valid mark graph"),
        "{err:?}"
    );
    assert!(objects.live_slot(live).is_ok());
}

#[test]
fn persistent_mark_sweep_rejects_dangling_ref_before_sweep() {
    let mut objects = ObjectTable::default();
    let missing = ObjectId { object_index: 88_888 };
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x651, vec![ObjectValue::Ref(Some(missing))])
        .unwrap();
    let garbage = objects
        .allocate_persistent_struct_for_gc_ref(0x652, vec![ObjectValue::I32(2)])
        .unwrap();

    let err = objects
        .persistent_mark_sweep_from_roots_for_test([root])
        .unwrap_err();

    assert!(
        err.to_string()
            .contains("persistent object sweep requires a valid mark graph"),
        "{err:?}"
    );
    assert!(objects.live_slot(root).is_ok());
    assert!(objects.live_slot(garbage).is_ok());
}
```

- [ ] **Step 2: Run the focused tests and confirm failure**

Run:

```bash
cargo test -p wasmtime --lib persistent_mark_sweep_rejects_ -- --format terse
```

Expected: compile failure because `persistent_mark_sweep_from_roots_for_test` does not exist.

- [ ] **Step 3: Add mark-sweep report and diagnostics**

In `crates/wasmtime/src/runtime/transaction/object_gc.rs`, add:

```rust
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentMarkSweepReport {
    pub(crate) mark: PersistentObjectMarkReport,
    pub(crate) sweep: PersistentVolatileSweepReport,
}

impl PersistentObjectMarkReport {
    pub(crate) fn has_integrity_diagnostics(&self) -> bool {
        !self.invalid_roots.is_empty() || !self.dangling_refs.is_empty()
    }

    pub(crate) fn ensure_sweepable(&self) -> Result<()> {
        ensure!(
            !self.has_integrity_diagnostics(),
            "persistent object sweep requires a valid mark graph: {} invalid roots, {} dangling refs",
            self.invalid_roots.len(),
            self.dangling_refs.len()
        );
        Ok(())
    }
}
```

- [ ] **Step 4: Add the object-table collect helper**

In `crates/wasmtime/src/runtime/transaction.rs`, import the new report type from `object_gc`, then add this method inside `impl ObjectTable` near `apply_volatile_persistent_sweep`:

```rust
pub(crate) fn persistent_mark_sweep_from_roots<I>(
    &mut self,
    roots: I,
) -> Result<PersistentMarkSweepReport>
where
    I: IntoIterator<Item = ObjectId>,
{
    let mark = PersistentObjectMarker::mark(self, roots)?;
    mark.ensure_sweepable()?;
    let sweep = self.apply_volatile_persistent_sweep(&mark)?;
    Ok(PersistentMarkSweepReport { mark, sweep })
}

#[cfg(test)]
fn persistent_mark_sweep_from_roots_for_test<I>(
    &mut self,
    roots: I,
) -> Result<PersistentMarkSweepReport>
where
    I: IntoIterator<Item = ObjectId>,
{
    self.persistent_mark_sweep_from_roots(roots)
}
```

- [ ] **Step 5: Run focused tests**

Run:

```bash
cargo test -p wasmtime --lib persistent_mark_sweep_rejects_ -- --format terse
```

Expected: both tests pass.

## Task 2: Add Stop-the-World Runtime Collect API

**Files:**

- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `crates/wasmtime/src/runtime/transaction.rs`

- [ ] **Step 1: Write failing tests for root-driven collection**

Add these tests near the persistent root index tests:

```rust
#[test]
fn transaction_state_persistent_mark_sweep_uses_committed_roots() {
    let mut objects = ObjectTable::default();
    let child = objects
        .allocate_persistent_struct_for_gc_ref(0x660, vec![ObjectValue::I32(3)])
        .unwrap();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x661, vec![ObjectValue::Ref(Some(child))])
        .unwrap();
    let garbage = objects
        .allocate_persistent_struct_for_gc_ref(0x662, vec![ObjectValue::I32(4)])
        .unwrap();

    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(0x660));
    state.stage_global(0, GlobalSnapshot::GcRef(0x661)).unwrap();
    let delta = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    state.complete_commit().unwrap();
    state
        .apply_committed_persistent_root_delta_for_test(delta)
        .unwrap();

    let report = state
        .persistent_mark_sweep_collect_for_test(&mut objects)
        .unwrap();

    assert_eq!(report.mark.reachable, object_set([root, child]));
    assert_eq!(report.sweep.retained_objects, vec![root, child]);
    assert_eq!(report.sweep.removed_objects, vec![garbage]);
    assert!(objects.live_slot(root).is_ok());
    assert!(objects.live_slot(child).is_ok());
    assert!(objects.live_slot(garbage).is_err());
}

#[test]
fn transaction_state_persistent_mark_sweep_rejects_active_transaction() {
    let mut objects = ObjectTable::default();
    objects
        .allocate_persistent_struct_for_gc_ref(0x663, vec![ObjectValue::I32(5)])
        .unwrap();

    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(0x663));

    let err = state
        .persistent_mark_sweep_collect_for_test(&mut objects)
        .unwrap_err();

    assert!(
        err.to_string()
            .contains("persistent object marker cannot run while a transaction is active"),
        "{err:?}"
    );
}
```

- [ ] **Step 2: Run the focused tests and confirm failure**

Run:

```bash
cargo test -p wasmtime --lib transaction_state_persistent_mark_sweep_ -- --format terse
```

Expected: compile failure because `persistent_mark_sweep_collect_for_test` does not exist.

- [ ] **Step 3: Implement the runtime collect helper**

In `impl TransactionState` in `crates/wasmtime/src/runtime/transaction.rs`, add:

```rust
pub(crate) fn persistent_mark_sweep_collect(
    &mut self,
    objects: &mut ObjectTable,
) -> Result<PersistentMarkSweepReport> {
    ensure!(
        self.active.is_none() && current_thread_transaction().is_none(),
        "persistent object marker cannot run while a transaction is active"
    );
    let roots = self.persistent_root_ids();
    let report = objects.persistent_mark_sweep_from_roots(roots)?;
    self.persistent_gc_state = None;
    Ok(report)
}

#[cfg(test)]
fn persistent_mark_sweep_collect_for_test(
    &mut self,
    objects: &mut ObjectTable,
) -> Result<PersistentMarkSweepReport> {
    self.persistent_mark_sweep_collect(objects)
}
```

This API is stop-the-world at the transaction layer. It does not scan Rust stacks or ordinary Wasmtime GC roots.

- [ ] **Step 4: Run focused tests**

Run:

```bash
cargo test -p wasmtime --lib transaction_state_persistent_mark_sweep_ -- --format terse
```

Expected: both tests pass.

## Task 3: Add Maintenance Step and Finish-and-Sweep for Commit-Coupled Marking

**Files:**

- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `crates/wasmtime/src/runtime/transaction.rs`

- [ ] **Step 1: Write failing tests for explicit maintenance**

Add these tests near `persistent_gc_after_commit_observer_stores_marker_progress`:

```rust
#[test]
fn persistent_gc_maintenance_step_advances_read_heavy_cycle() {
    let mut objects = ObjectTable::default();
    let leaf = objects
        .allocate_persistent_struct_for_gc_ref(0x670, vec![ObjectValue::I32(1)])
        .unwrap();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x671, vec![ObjectValue::Ref(Some(leaf))])
        .unwrap();
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::default();
    state.install_recovered_persistent_roots([root]).unwrap();

    let first = state
        .persistent_gc_maintenance_step_for_test(
            &objects,
            PersistentGcBudget::objects(1),
        )
        .unwrap();
    let second = state
        .persistent_gc_maintenance_step_for_test(
            &objects,
            PersistentGcBudget::objects(1),
        )
        .unwrap();

    assert_eq!(
        first,
        PersistentGcStepReport {
            scanned_objects: 1,
            enqueued_objects: 1,
        }
    );
    assert_eq!(
        second,
        PersistentGcStepReport {
            scanned_objects: 1,
            enqueued_objects: 0,
        }
    );
}

#[test]
fn persistent_gc_finish_cycle_sweeps_unreachable_after_incremental_marking() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x672, vec![ObjectValue::I32(1)])
        .unwrap();
    let garbage = objects
        .allocate_persistent_struct_for_gc_ref(0x673, vec![ObjectValue::I32(2)])
        .unwrap();
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::default();
    state.install_recovered_persistent_roots([root]).unwrap();

    state
        .persistent_gc_maintenance_step_for_test(
            &objects,
            PersistentGcBudget::objects(1),
        )
        .unwrap();

    let report = state
        .finish_persistent_gc_cycle_and_sweep_for_test(&mut objects)
        .unwrap()
        .expect("maintenance cycle should exist");

    assert_eq!(report.mark.reachable, object_set([root]));
    assert_eq!(report.sweep.removed_objects, vec![garbage]);
    assert!(objects.live_slot(garbage).is_err());
}
```

- [ ] **Step 2: Run the tests and confirm failure**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_maintenance_step -- --format terse
cargo test -p wasmtime --lib persistent_gc_finish_cycle -- --format terse
```

Expected: compile failure because the maintenance and finish helpers do not exist.

- [ ] **Step 3: Implement explicit maintenance**

In `impl TransactionState`, add:

```rust
pub(crate) fn persistent_gc_maintenance_step(
    &mut self,
    object_table: &ObjectTable,
    budget: PersistentGcBudget,
) -> Result<PersistentGcStepReport> {
    ensure!(
        self.active.is_none() && current_thread_transaction().is_none(),
        "persistent object marker cannot run while a transaction is active"
    );
    let roots = if self.persistent_gc_state.is_none() {
        Some(self.persistent_root_ids())
    } else {
        None
    };
    let state = match self.persistent_gc_state.as_mut() {
        Some(state) => state,
        None => self
            .persistent_gc_state
            .insert(PersistentGcState::new(object_table, roots.unwrap_or_default())?),
    };
    state.mark_step(object_table, budget)
}

#[cfg(test)]
fn persistent_gc_maintenance_step_for_test(
    &mut self,
    object_table: &ObjectTable,
    budget: PersistentGcBudget,
) -> Result<PersistentGcStepReport> {
    self.persistent_gc_maintenance_step(object_table, budget)
}
```

- [ ] **Step 4: Implement finish-and-sweep**

In `impl TransactionState`, add:

```rust
pub(crate) fn finish_persistent_gc_cycle_and_sweep(
    &mut self,
    objects: &mut ObjectTable,
) -> Result<Option<PersistentMarkSweepReport>> {
    ensure!(
        self.active.is_none() && current_thread_transaction().is_none(),
        "persistent object marker cannot run while a transaction is active"
    );
    let Some(mut state) = self.persistent_gc_state.take() else {
        return Ok(None);
    };
    while !state.is_complete() {
        let pending = state.pending_object_count();
        state.mark_step(objects, PersistentGcBudget::objects(pending.max(1)))?;
    }
    let mark = state.into_report(objects)?;
    mark.ensure_sweepable()?;
    let sweep = objects.apply_volatile_persistent_sweep(&mark)?;
    Ok(Some(PersistentMarkSweepReport { mark, sweep }))
}

#[cfg(test)]
fn finish_persistent_gc_cycle_and_sweep_for_test(
    &mut self,
    objects: &mut ObjectTable,
) -> Result<Option<PersistentMarkSweepReport>> {
    self.finish_persistent_gc_cycle_and_sweep(objects)
}
```

Also add this accessor to `PersistentGcState` in `object_gc.rs`:

```rust
pub(crate) fn pending_object_count(&self) -> usize {
    self.grey.len()
}
```

- [ ] **Step 5: Run focused tests**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_maintenance_step -- --format terse
cargo test -p wasmtime --lib persistent_gc_finish_cycle -- --format terse
```

Expected: both tests pass.

## Task 4: Add File-Backed Recovery and Whole-Dead Chunk GC Tests

**Files:**

- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/tests/transaction_persistence.rs` if the existing in-module file-backed helpers are insufficient.

- [ ] **Step 1: Write a recovery test for unreachable objects after root replacement**

Add a file-backed test that performs these exact phases:

1. Create a file-backed transaction region.
2. Commit object `A` and publish it through a persistent root.
3. Commit object `B` and replace the root with `B`.
4. Reopen the region.
5. Assert recovery winners include both `A` and `B`.
6. Rebuild the volatile object table through `rebuild_reachable_from_recovered_object_winners`.
7. Assert the rebuilt table installs only `B`.

Use this assertion shape:

```rust
assert!(recovered.object_winners.iter().any(|winner| winner.object_id == object_a.object_index));
assert!(recovered.object_winners.iter().any(|winner| winner.object_id == object_b.object_index));
assert_eq!(
    report.mark.reachable,
    object_set([object_b])
);
assert_eq!(
    report.mark.unreachable_persistent,
    object_set([object_a])
);
assert!(rebuilt.payload(object_a).is_err());
assert!(rebuilt.payload(object_b).is_ok());
```

Name the test:

```rust
file_backed_persistent_gc_recovery_filters_root_replaced_object
```

- [ ] **Step 2: Write a whole-dead object chunk retirement test**

Add a file-backed test that:

1. Publishes a persistent object into its own object-data chunk.
2. Replaces the root so that object becomes unreachable.
3. Reopens and gets the recovery GC report.
4. Calls `retire_unreachable_object_chunks_for_test(path, &[unreachable_id])`.
5. Asserts at least one retired chunk is returned.
6. Publishes another object record.
7. Asserts the next object-data chunk reuses the retired chunk with incremented generation.

Use this assertion shape:

```rust
let retired = retire_unreachable_object_chunks_for_test(path.path(), &[unreachable.object_index])
    .unwrap();
assert_eq!(retired, vec![unreachable_chunk_start]);
assert_eq!(
    region.block_generation(reused_chunk_start).unwrap(),
    old_generation + 1
);
```

Name the test:

```rust
file_backed_persistent_gc_retires_whole_dead_object_chunk
```

- [ ] **Step 3: Write a mixed live/dead chunk non-retirement test**

Add a file-backed test that publishes two object records into the same object-data chunk, keeps one reachable, and makes the other unreachable. Then call `retire_unreachable_object_chunks_for_test` for the dead object id.

Assert:

```rust
assert!(retired.is_empty());
```

Name the test:

```rust
file_backed_persistent_gc_keeps_mixed_live_dead_object_chunk
```

- [ ] **Step 4: Run focused file-backed tests**

Run:

```bash
cargo test -p wasmtime --lib file_backed_persistent_gc_ -- --format terse
cargo test -p wasmtime --test transaction_persistence file_backed_persistent_gc_ -- --format terse
```

Expected: all new tests pass. If the tests live only in one target, the other command may report zero matching tests and should exit successfully.

## Task 5: Enforce Commit-Time Reachability Semantics for Newly Produced Objects

**Files:**

- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `crates/wasmtime/src/runtime/transaction.rs`

- [ ] **Step 1: Write tests for transaction-local discard and commit reachability**

Add these tests near object publication tests:

```rust
#[test]
fn persistent_gc_does_not_consider_aborted_transaction_local_object_persistent() {
    let mut objects = ObjectTable::default();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(0x680));
    let local = objects
        .allocate_struct(vec![ObjectValue::I32(7)])
        .unwrap();

    state.abort().unwrap();

    let report = PersistentObjectMarker::mark(&objects, []).unwrap();
    assert!(report.reachable.is_empty());
    assert!(report.unreachable_persistent.is_empty());
    assert!(objects.live_slot(local).is_ok());
}

#[test]
fn persistent_gc_collects_committed_object_only_after_root_is_removed() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x681, vec![ObjectValue::I32(8)])
        .unwrap();
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(0x681));
    state.stage_global(0, GlobalSnapshot::GcRef(0x681)).unwrap();
    let install = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    state.complete_commit().unwrap();
    state
        .apply_committed_persistent_root_delta_for_test(install)
        .unwrap();

    let first = state
        .persistent_mark_sweep_collect_for_test(&mut objects)
        .unwrap();
    assert_eq!(first.sweep.retained_objects, vec![root]);

    state.begin().unwrap();
    state.stage_global(0, GlobalSnapshot::I32(0)).unwrap();
    let removal = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    state.complete_commit().unwrap();
    state
        .apply_committed_persistent_root_delta_for_test(removal)
        .unwrap();

    let second = state
        .persistent_mark_sweep_collect_for_test(&mut objects)
        .unwrap();
    assert_eq!(second.sweep.removed_objects, vec![root]);
    assert!(objects.live_slot(root).is_err());
}
```

- [ ] **Step 2: Run focused tests**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_does_not_consider_aborted -- --format terse
cargo test -p wasmtime --lib persistent_gc_collects_committed_object -- --format terse
```

Expected: both tests pass. If the abort test exposes that volatile objects are not cleaned up by abort today, update only the assertion to the current invariant: the object must not appear in `unreachable_persistent`.

## Task 6: Documentation and Roadmap Update

**Files:**

- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify: `docs/shisoft/transactional-wasm-remaining-work-roadmap.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
- Modify: `docs/shisoft/2026-06-14-persistent-object-gc-wave1-design.md`

- [ ] **Step 1: Update the core design doc**

Add a short section titled `First Mark-Sweep Persistent GC` under the persistent GC section. It must state:

- the collector is non-moving and tombstone-less
- roots come from committed `tglobal`, `ttable`, and explicit root state
- mark traces `ObjectId` edges with type-layout metadata
- sweep removes volatile object-table entries only
- durable chunk retirement is whole-dead only
- object data remains append-only until future copying cleanup
- invalid roots or dangling refs stop sweep

- [ ] **Step 2: Update the roadmap**

In Wave 9B/9C, mark the explicit maintenance API, finish-and-sweep API, stop-the-world runtime collect API, and whole-dead chunk tests as complete once implemented.

Leave these as deferred:

- moving/compacting object records
- line-level Immix reuse
- LXR
- object identity reuse
- background collector thread
- durable collection checkpoints

- [ ] **Step 3: Update the implementation log**

Add an entry dated 2026-06-16:

```markdown
## 2026-06-16: First mark-sweep persistent GC

- Added a stop-the-world persistent mark-sweep API over committed `ObjectId` roots.
- Added explicit maintenance stepping and finish-and-sweep for commit-coupled marking.
- Sweep mutates volatile object-table state only and refuses to run with invalid roots or dangling references.
- Added file-backed coverage for recovery filtering and whole-dead object-data chunk retirement.
- Deferred moving collection, Immix/LXR, line-level reuse, object-id reuse, and background collection.
```

## Task 7: Final Verification

**Files:**

- Verify the full working tree.

- [ ] **Step 1: Format**

Run:

```bash
cargo fmt --check
```

Expected: exit 0.

- [ ] **Step 2: Whitespace check**

Run:

```bash
git diff --check
```

Expected: exit 0.

- [ ] **Step 3: Focused transaction GC tests**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc -- --format terse
cargo test -p wasmtime --lib persistent_mark_sweep -- --format terse
cargo test -p wasmtime --lib persistent_object_marker -- --format terse
```

Expected: exit 0 for all commands.

- [ ] **Step 4: Persistence tests**

Run:

```bash
cargo test -p wasmtime --test transaction_persistence -- --format terse
```

Expected: exit 0.

- [ ] **Step 5: Broad transaction library tests**

Run:

```bash
cargo test -p wasmtime transaction --lib -- --format terse
```

Expected: exit 0.

- [ ] **Step 6: WAST transaction gate**

Run:

```bash
CARGO_TARGET_DIR=/tmp/wasmtime-persistent-gc-target \
CARGO_INCREMENTAL=0 \
WASMTIME_TEST_TRANSACTION_WAST=1 \
CARGO_BUILD_JOBS=2 \
python3 ./ci/run-tests.py --locked --exclude=wasi-preview1-component-adapter -- --format terse
```

Expected: exit 0.

- [ ] **Step 7: Commit**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction/object_gc.rs \
        crates/wasmtime/src/runtime/transaction.rs \
        crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs \
        crates/wasmtime/tests/transaction_persistence.rs \
        docs/shisoft/transactional-wasm-runtime-core-design.md \
        docs/shisoft/transactional-wasm-remaining-work-roadmap.md \
        docs/shisoft/transactional-wasm-implementation-log.md \
        docs/shisoft/2026-06-14-persistent-object-gc-wave1-design.md \
        docs/shisoft/2026-06-16-first-mark-sweep-persistent-gc-plan.md
git commit -m "Implement first persistent mark-sweep GC"
```

Do not add any `Co-authored-by` annotation.

## Self-Review

- Spec coverage: the plan covers reachability-defined persistence, aborted transaction-local discard semantics, runtime mark-sweep, recovery filtering, commit-coupled progress, read-heavy maintenance, and whole-dead chunk reuse.
- Deferred by design: Immix/LXR, background collection, copying cleanup, line-level reuse, persistent mark bits, durable tombstones, object-id reuse, and public API stabilization.
- Type consistency: report names use `PersistentMarkSweepReport`; root-driven entrypoints use `persistent_mark_sweep_from_roots`; transaction entrypoints use `persistent_mark_sweep_collect`, `persistent_gc_maintenance_step`, and `finish_persistent_gc_cycle_and_sweep`.
- Safety policy: sweep is blocked by invalid roots or dangling refs, so the first collector does not silently turn graph corruption into data loss.
