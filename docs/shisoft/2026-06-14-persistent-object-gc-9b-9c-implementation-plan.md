# Persistent Object GC 9B/9C Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the current runtime-only full marker into a basic persistent GC pipeline with commit-coupled incremental marking, then add the first durable tombstone/recovery behavior without block reuse.

**Architecture:** Refactor Wave 9A into a focused `object_gc` module and keep `ObjectId` as the only persistent identity. Wave 9B adds volatile `PersistentGcState`, commit barrier deltas, and bounded marking minibatches after successful commits. Wave 9C adds tombstone publications and recovery precedence; Immix/block reclamation is explicitly out of scope for this plan.

**Tech Stack:** Rust, Wasmtime runtime internals, `ObjectTable`, `ObjectHeap`, `TxDurableLog`, file-backed recovery tests, `cargo test`.

---

## Scope

This plan deliberately does not implement Immix line reuse, block free-list
reclamation, compaction, moving objects, or persistent object-index storage.
Those require a separate allocator/reclamation plan after durable tombstones are
correct.

The plan does update the current Wave 9A shape where needed. The existing
`PersistentObjectMarker::mark` API should remain as a compatibility wrapper, but
its implementation should move onto the same worklist machinery used by the
incremental marker.

## File Map

- Create: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
  - Owns persistent GC report types, mark state, commit deltas, sweep plans, and
    tombstone publication helpers.
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
  - Adds `mod object_gc;`, re-exports current marker/report names, exposes narrow
    object-table helper methods to the child module, and wires transaction commit
    epilogue APIs.
- Modify: `crates/wasmtime/src/runtime/transaction/object_heap.rs`
  - Adds durable object tombstone flag and tombstone record encoding.
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
  - Adds `PendingPublication::persistent_object_tombstone`.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
  - Teaches recovery that the highest committed tombstone suppresses older live
    object records.
- Modify: `docs/shisoft/2026-06-14-persistent-object-gc-wave1-design.md`
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify: `docs/shisoft/transactional-wasm-remaining-work-roadmap.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

## Task 1: Move Wave 9A Marker Onto A Reusable GC Module

**Files:**
- Create: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_object_marker -- --format terse`

- [ ] **Step 1: Write the failing module-shape test**

Add this test inside the existing `#[cfg(test)] mod tests` in
`crates/wasmtime/src/runtime/transaction.rs`, beside the other
`persistent_object_marker_*` tests:

```rust
#[test]
fn persistent_object_marker_exposes_budgeted_core_report() {
    let mut objects = ObjectTable::default();
    let child = objects
        .allocate_persistent_struct_for_gc_ref(0x541, vec![ObjectValue::I32(1)])
        .unwrap();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(
            0x542,
            vec![ObjectValue::Ref(Some(child))],
        )
        .unwrap();

    let mut marker = PersistentGcState::start_full_mark([root]);
    let first = marker
        .mark_step(&objects, PersistentGcBudget::objects(1))
        .unwrap();
    assert_eq!(first.scanned_objects, 1);
    assert_eq!(marker.reachable_for_test(), object_set([root, child]));
    assert_eq!(marker.grey_for_test(), vec![child]);

    let second = marker
        .mark_step(&objects, PersistentGcBudget::objects(1))
        .unwrap();
    assert_eq!(second.scanned_objects, 1);
    assert!(marker.grey_for_test().is_empty());

    let report = marker.finish_report(&objects).unwrap();
    assert_eq!(report.reachable, object_set([root, child]));
    assert!(report.unreachable_persistent.is_empty());
}
```

- [ ] **Step 2: Run the test to verify RED**

Run:

```bash
cargo test -p wasmtime --lib persistent_object_marker_exposes_budgeted_core_report -- --format terse
```

Expected: compile failure naming missing `PersistentGcState` or
`PersistentGcBudget`.

- [ ] **Step 3: Add `object_gc.rs` with the reusable marker core**

Create `crates/wasmtime/src/runtime/transaction/object_gc.rs` with:

```rust
use super::{ObjectId, ObjectTable, current_thread_transaction};
use crate::prelude::*;
use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use std::collections::VecDeque;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PersistentRootErrorKind {
    Missing,
    NotPersistent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistentRootError {
    pub(crate) root: ObjectId,
    pub(crate) kind: PersistentRootErrorKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DanglingObjectRefKind {
    Missing,
    NotPersistent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DanglingObjectRef {
    pub(crate) from: ObjectId,
    pub(crate) to: ObjectId,
    pub(crate) kind: DanglingObjectRefKind,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentObjectMarkReport {
    pub(crate) reachable: BTreeSet<ObjectId>,
    pub(crate) unreachable_persistent: BTreeSet<ObjectId>,
    pub(crate) dangling_refs: Vec<DanglingObjectRef>,
    pub(crate) invalid_roots: Vec<PersistentRootError>,
}

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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum PersistentGcPhase {
    #[default]
    Idle,
    Marking,
    Complete,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PersistentGcState {
    phase: PersistentGcPhase,
    epoch: u64,
    reachable: BTreeSet<ObjectId>,
    grey: VecDeque<ObjectId>,
    dangling_refs: Vec<DanglingObjectRef>,
    invalid_roots: Vec<PersistentRootError>,
}

impl PersistentGcState {
    pub(crate) fn start_full_mark<I>(roots: I) -> Self
    where
        I: IntoIterator<Item = ObjectId>,
    {
        let mut state = Self {
            phase: PersistentGcPhase::Marking,
            epoch: 1,
            ..Self::default()
        };
        state.enqueue_roots(roots);
        state
    }

    fn enqueue_roots<I>(&mut self, roots: I)
    where
        I: IntoIterator<Item = ObjectId>,
    {
        for root in roots {
            if self.reachable.insert(root) {
                self.grey.push_back(root);
            }
        }
    }

    pub(crate) fn mark_step(
        &mut self,
        objects: &ObjectTable,
        budget: PersistentGcBudget,
    ) -> Result<PersistentGcStepReport> {
        ensure!(
            current_thread_transaction().is_none(),
            "persistent object marker cannot run while a transaction is active"
        );
        let mut report = PersistentGcStepReport::default();
        for _ in 0..budget.objects {
            let Some(object) = self.grey.pop_front() else {
                self.phase = PersistentGcPhase::Complete;
                break;
            };
            report.scanned_objects += 1;
            for child in objects
                .trace_object_ids(object)
                .with_context(|| format!("failed to trace persistent object {object:?}"))?
            {
                match objects.live_persistent_status_for_gc(child) {
                    PersistentObjectStatus::Persistent => {
                        if self.reachable.insert(child) {
                            self.grey.push_back(child);
                            report.enqueued_objects += 1;
                        }
                    }
                    PersistentObjectStatus::Missing => {
                        self.dangling_refs.push(DanglingObjectRef {
                            from: object,
                            to: child,
                            kind: DanglingObjectRefKind::Missing,
                        });
                    }
                    PersistentObjectStatus::NotPersistent => {
                        self.dangling_refs.push(DanglingObjectRef {
                            from: object,
                            to: child,
                            kind: DanglingObjectRefKind::NotPersistent,
                        });
                    }
                }
            }
        }
        if self.grey.is_empty() && self.phase == PersistentGcPhase::Marking {
            self.phase = PersistentGcPhase::Complete;
        }
        Ok(report)
    }

    pub(crate) fn finish_report(
        &self,
        objects: &ObjectTable,
    ) -> Result<PersistentObjectMarkReport> {
        Ok(PersistentObjectMarkReport {
            reachable: self.reachable.clone(),
            unreachable_persistent: objects
                .persistent_object_ids_for_gc()?
                .difference(&self.reachable)
                .copied()
                .collect(),
            dangling_refs: self.dangling_refs.clone(),
            invalid_roots: self.invalid_roots.clone(),
        })
    }

    #[cfg(test)]
    pub(crate) fn reachable_for_test(&self) -> BTreeSet<ObjectId> {
        self.reachable.clone()
    }

    #[cfg(test)]
    pub(crate) fn grey_for_test(&self) -> Vec<ObjectId> {
        self.grey.iter().copied().collect()
    }
}

pub(crate) struct PersistentObjectMarker;

impl PersistentObjectMarker {
    pub(crate) fn mark<I>(objects: &ObjectTable, roots: I) -> Result<PersistentObjectMarkReport>
    where
        I: IntoIterator<Item = ObjectId>,
    {
        ensure!(
            current_thread_transaction().is_none(),
            "persistent object marker cannot run while a transaction is active"
        );
        let mut state = PersistentGcState::default();
        state.phase = PersistentGcPhase::Marking;
        state.epoch = 1;

        for root in roots {
            match objects.live_persistent_status_for_gc(root) {
                PersistentObjectStatus::Persistent => {
                    if state.reachable.insert(root) {
                        state.grey.push_back(root);
                    }
                }
                PersistentObjectStatus::Missing => {
                    state.invalid_roots.push(PersistentRootError {
                        root,
                        kind: PersistentRootErrorKind::Missing,
                    });
                }
                PersistentObjectStatus::NotPersistent => {
                    state.invalid_roots.push(PersistentRootError {
                        root,
                        kind: PersistentRootErrorKind::NotPersistent,
                    });
                }
            }
        }

        while !state.grey.is_empty() {
            state.mark_step(objects, PersistentGcBudget::objects(usize::MAX))?;
        }
        state.finish_report(objects)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PersistentObjectStatus {
    Persistent,
    NotPersistent,
    Missing,
}
```

- [ ] **Step 4: Wire the module from `transaction.rs`**

At the top of `crates/wasmtime/src/runtime/transaction.rs`, add:

```rust
#[path = "transaction/object_gc.rs"]
mod object_gc;
pub(crate) use object_gc::{
    DanglingObjectRef, DanglingObjectRefKind, PersistentGcBudget, PersistentGcPhase,
    PersistentGcState, PersistentGcStepReport, PersistentObjectMarkReport,
    PersistentObjectMarker, PersistentObjectStatus, PersistentRootError,
    PersistentRootErrorKind,
};
```

Remove the old inline definitions of:

```rust
PersistentRootErrorKind
PersistentRootError
DanglingObjectRefKind
DanglingObjectRef
PersistentObjectMarkReport
PersistentObjectMarker
```

Rename the existing `ObjectTable::persistent_object_ids` helper to:

```rust
fn persistent_object_ids_for_gc(&self) -> Result<BTreeSet<ObjectId>> {
    let mut objects = BTreeSet::new();
    for (index, slot) in self.slots.iter().enumerate() {
        if slot.as_ref().is_some_and(|slot| slot.persistent) {
            objects.insert(ObjectId {
                object_index: u64::try_from(index)
                    .context("object table slot index does not fit u64")?,
            });
        }
    }
    Ok(objects)
}

fn live_persistent_status_for_gc(&self, object_id: ObjectId) -> PersistentObjectStatus {
    match self.live_slot(object_id) {
        Ok(slot) if slot.persistent => PersistentObjectStatus::Persistent,
        Ok(_) => PersistentObjectStatus::NotPersistent,
        Err(_) => PersistentObjectStatus::Missing,
    }
}
```

- [ ] **Step 5: Run focused tests to verify GREEN**

Run:

```bash
cargo test -p wasmtime --lib persistent_object_marker -- --format terse
```

Expected: all `persistent_object_marker` tests pass, including
`persistent_object_marker_exposes_budgeted_core_report`.

- [ ] **Step 6: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_gc.rs
git commit -m "Refactor persistent object marker core"
```

## Task 2: Add Commit Barrier Delta Extraction

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_gc_commit_delta -- --format terse`

- [ ] **Step 1: Write the failing object-publication delta test**

Add this test in `transaction.rs`:

```rust
#[test]
fn persistent_gc_commit_delta_extracts_edges_from_object_publications() {
    let mut objects = ObjectTable::default();
    let child = objects
        .allocate_persistent_struct_for_gc_ref(0x551, vec![ObjectValue::I32(1)])
        .unwrap();
    let parent = objects
        .allocate_persistent_struct_for_gc_ref(
            0x552,
            vec![ObjectValue::Ref(None), ObjectValue::I32(2)],
        )
        .unwrap();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(551));

    state.acquire_object_write(&objects, parent).unwrap();
    state
        .stage_struct_field(&objects, parent, 0, ObjectValue::Ref(Some(child)))
        .unwrap();
    let mut publications = Vec::new();
    assert!(
        state
            .commit_object_payloads_into(&mut objects, &mut publications)
            .unwrap()
    );

    let delta =
        PersistentGcCommitDelta::from_object_publications(&objects, &publications).unwrap();

    assert_eq!(delta.roots, Vec::<ObjectId>::new());
    assert_eq!(
        delta.edges,
        vec![PersistentObjectEdge {
            from: parent,
            to: child,
        }]
    );
    clear_current_thread_transaction_for_test();
}
```

- [ ] **Step 2: Run the test to verify RED**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_commit_delta_extracts_edges_from_object_publications -- --format terse
```

Expected: compile failure naming missing `PersistentGcCommitDelta` or
`PersistentObjectEdge`.

- [ ] **Step 3: Add commit delta types**

Add to `object_gc.rs`:

```rust
use crate::runtime::vm::unpack_object_granule_id;
use super::persist;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PersistentObjectEdge {
    pub(crate) from: ObjectId,
    pub(crate) to: ObjectId,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentGcCommitDelta {
    pub(crate) roots: Vec<ObjectId>,
    pub(crate) edges: Vec<PersistentObjectEdge>,
}

impl PersistentGcCommitDelta {
    pub(crate) fn from_object_publications(
        objects: &ObjectTable,
        publications: &[persist::PendingPublication],
    ) -> Result<Self> {
        let mut delta = Self::default();
        for publication in publications {
            let Ok((_, object_index)) = unpack_object_granule_id(publication.logical_id) else {
                continue;
            };
            let from = ObjectId { object_index };
            for to in objects.trace_object_ids(from)? {
                delta.edges.push(PersistentObjectEdge { from, to });
            }
        }
        Ok(delta)
    }

    pub(crate) fn from_roots<I>(roots: I) -> Self
    where
        I: IntoIterator<Item = ObjectId>,
    {
        Self {
            roots: roots.into_iter().collect(),
            edges: Vec::new(),
        }
    }

    pub(crate) fn extend(&mut self, other: Self) {
        self.roots.extend(other.roots);
        self.edges.extend(other.edges);
    }
}
```

Update the `pub(crate) use object_gc::{ ... }` list in `transaction.rs` with:

```rust
PersistentGcCommitDelta, PersistentObjectEdge,
```

- [ ] **Step 4: Run focused tests to verify GREEN**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_commit_delta -- --format terse
```

Expected: the new delta test passes.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_gc.rs
git commit -m "Add persistent GC commit deltas"
```

## Task 3: Apply Commit Barriers To Incremental Marking

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_gc_commit_barrier -- --format terse`

- [ ] **Step 1: Write failing direct-child barrier tests**

Add these tests in `transaction.rs`:

```rust
#[test]
fn persistent_gc_commit_barrier_enqueues_new_root() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x561, vec![ObjectValue::I32(1)])
        .unwrap();
    let mut gc = PersistentGcState::default();

    let report = gc
        .observe_commit_delta(
            &objects,
            &PersistentGcCommitDelta::from_roots([root]),
            PersistentGcBudget::objects(1),
        )
        .unwrap();

    assert_eq!(report.scanned_objects, 1);
    assert_eq!(gc.reachable_for_test(), object_set([root]));
}

#[test]
fn persistent_gc_commit_barrier_enqueues_child_when_owner_is_marked() {
    let mut objects = ObjectTable::default();
    let child = objects
        .allocate_persistent_struct_for_gc_ref(0x562, vec![ObjectValue::I32(1)])
        .unwrap();
    let owner = objects
        .allocate_persistent_struct_for_gc_ref(0x563, vec![ObjectValue::I32(2)])
        .unwrap();
    let mut gc = PersistentGcState::start_full_mark([owner]);
    gc.mark_step(&objects, PersistentGcBudget::objects(1)).unwrap();

    let delta = PersistentGcCommitDelta {
        roots: Vec::new(),
        edges: vec![PersistentObjectEdge {
            from: owner,
            to: child,
        }],
    };
    gc.observe_commit_delta(&objects, &delta, PersistentGcBudget::objects(0))
        .unwrap();

    assert_eq!(gc.reachable_for_test(), object_set([owner, child]));
    assert_eq!(gc.grey_for_test(), vec![child]);
}

#[test]
fn persistent_gc_commit_barrier_ignores_child_when_owner_is_unmarked() {
    let mut objects = ObjectTable::default();
    let child = objects
        .allocate_persistent_struct_for_gc_ref(0x564, vec![ObjectValue::I32(1)])
        .unwrap();
    let owner = objects
        .allocate_persistent_struct_for_gc_ref(0x565, vec![ObjectValue::I32(2)])
        .unwrap();
    let mut gc = PersistentGcState::default();

    let delta = PersistentGcCommitDelta {
        roots: Vec::new(),
        edges: vec![PersistentObjectEdge {
            from: owner,
            to: child,
        }],
    };
    gc.observe_commit_delta(&objects, &delta, PersistentGcBudget::objects(1))
        .unwrap();

    assert!(gc.reachable_for_test().is_empty());
    assert!(gc.grey_for_test().is_empty());
}
```

- [ ] **Step 2: Run tests to verify RED**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_commit_barrier -- --format terse
```

Expected: compile failure naming missing `observe_commit_delta`.

- [ ] **Step 3: Implement commit barrier observation**

Add to `impl PersistentGcState` in `object_gc.rs`:

```rust
pub(crate) fn observe_commit_delta(
    &mut self,
    objects: &ObjectTable,
    delta: &PersistentGcCommitDelta,
    budget: PersistentGcBudget,
) -> Result<PersistentGcStepReport> {
    ensure!(
        current_thread_transaction().is_none(),
        "persistent object GC commit barrier cannot run while a transaction is active"
    );
    if self.epoch == 0 {
        self.epoch = 1;
    }
    if self.phase == PersistentGcPhase::Idle {
        self.phase = PersistentGcPhase::Marking;
    }

    for &root in &delta.roots {
        match objects.live_persistent_status_for_gc(root) {
            PersistentObjectStatus::Persistent => {
                if self.reachable.insert(root) {
                    self.grey.push_back(root);
                }
            }
            PersistentObjectStatus::Missing => {
                self.invalid_roots.push(PersistentRootError {
                    root,
                    kind: PersistentRootErrorKind::Missing,
                });
            }
            PersistentObjectStatus::NotPersistent => {
                self.invalid_roots.push(PersistentRootError {
                    root,
                    kind: PersistentRootErrorKind::NotPersistent,
                });
            }
        }
    }

    for &PersistentObjectEdge { from, to } in &delta.edges {
        if !self.reachable.contains(&from) {
            continue;
        }
        match objects.live_persistent_status_for_gc(to) {
            PersistentObjectStatus::Persistent => {
                if self.reachable.insert(to) {
                    self.grey.push_back(to);
                }
            }
            PersistentObjectStatus::Missing => {
                self.dangling_refs.push(DanglingObjectRef {
                    from,
                    to,
                    kind: DanglingObjectRefKind::Missing,
                });
            }
            PersistentObjectStatus::NotPersistent => {
                self.dangling_refs.push(DanglingObjectRef {
                    from,
                    to,
                    kind: DanglingObjectRefKind::NotPersistent,
                });
            }
        }
    }

    self.mark_step(objects, budget)
}
```

- [ ] **Step 4: Run focused tests to verify GREEN**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_commit_barrier -- --format terse
```

Expected: all commit-barrier tests pass.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_gc.rs
git commit -m "Add commit-coupled persistent marking"
```

## Task 4: Add TransactionState Commit-Epilogue GC Hook

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_gc_commit_epilogue -- --format terse`

- [ ] **Step 1: Write the failing transaction epilogue test**

Add this test in `transaction.rs`:

```rust
#[test]
fn persistent_gc_commit_epilogue_observes_object_publications_after_commit() {
    let mut objects = ObjectTable::default();
    let child = objects
        .allocate_persistent_struct_for_gc_ref(0x571, vec![ObjectValue::I32(1)])
        .unwrap();
    let parent = objects
        .allocate_persistent_struct_for_gc_ref(
            0x572,
            vec![ObjectValue::Ref(None)],
        )
        .unwrap();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(571));

    state
        .record_persistent_gc_roots_for_test([parent])
        .unwrap();
    state
        .persistent_gc_observe_commit_for_test(
            &objects,
            &PersistentGcCommitDelta::from_roots([parent]),
            PersistentGcBudget::objects(1),
        )
        .unwrap();

    state.begin().unwrap();
    state.acquire_object_write(&objects, parent).unwrap();
    state
        .stage_struct_field(&objects, parent, 0, ObjectValue::Ref(Some(child)))
        .unwrap();
    let mut publications = Vec::new();
    assert!(
        state
            .commit_object_payloads_into(&mut objects, &mut publications)
            .unwrap()
    );
    state.complete_commit().unwrap();

    let report = state
        .persistent_gc_after_object_commit_for_test(
            &objects,
            &publications,
            PersistentGcBudget::objects(1),
        )
        .unwrap();

    assert_eq!(report.scanned_objects, 1);
    assert_eq!(
        state.persistent_gc_reachable_for_test(),
        object_set([parent, child])
    );
    clear_current_thread_transaction_for_test();
}
```

- [ ] **Step 2: Run the test to verify RED**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_commit_epilogue_observes_object_publications_after_commit -- --format terse
```

Expected: compile failure naming missing `persistent_gc_*` helpers or missing
`persistent_gc` field.

- [ ] **Step 3: Add GC state to `TransactionState`**

Add a field to `TransactionState`:

```rust
persistent_gc: PersistentGcState,
persistent_gc_roots: BTreeSet<ObjectId>,
```

Update `Default` initialization through the derived/default path if
`TransactionState` uses `#[derive(Default)]`. If the struct has manual defaults,
initialize:

```rust
persistent_gc: PersistentGcState::default(),
persistent_gc_roots: BTreeSet::new(),
```

- [ ] **Step 4: Add commit-epilogue helpers**

Add methods on `TransactionState`:

```rust
pub(crate) fn persistent_gc_observe_commit(
    &mut self,
    object_table: &ObjectTable,
    delta: &PersistentGcCommitDelta,
    budget: PersistentGcBudget,
) -> Result<PersistentGcStepReport> {
    self.persistent_gc
        .observe_commit_delta(object_table, delta, budget)
}

pub(crate) fn persistent_gc_after_object_commit(
    &mut self,
    object_table: &ObjectTable,
    publications: &[persist::PendingPublication],
    budget: PersistentGcBudget,
) -> Result<PersistentGcStepReport> {
    let mut delta = PersistentGcCommitDelta::from_roots(
        self.persistent_gc_roots.iter().copied(),
    );
    delta.extend(PersistentGcCommitDelta::from_object_publications(
        object_table,
        publications,
    )?);
    self.persistent_gc_observe_commit(object_table, &delta, budget)
}
```

Add test-only helpers:

```rust
#[cfg(test)]
impl TransactionState {
    fn record_persistent_gc_roots_for_test<I>(&mut self, roots: I) -> Result<()>
    where
        I: IntoIterator<Item = ObjectId>,
    {
        self.persistent_gc_roots.extend(roots);
        Ok(())
    }

    fn persistent_gc_observe_commit_for_test(
        &mut self,
        object_table: &ObjectTable,
        delta: &PersistentGcCommitDelta,
        budget: PersistentGcBudget,
    ) -> Result<PersistentGcStepReport> {
        self.persistent_gc_observe_commit(object_table, delta, budget)
    }

    fn persistent_gc_after_object_commit_for_test(
        &mut self,
        object_table: &ObjectTable,
        publications: &[persist::PendingPublication],
        budget: PersistentGcBudget,
    ) -> Result<PersistentGcStepReport> {
        self.persistent_gc_after_object_commit(object_table, publications, budget)
    }

    fn persistent_gc_reachable_for_test(&self) -> BTreeSet<ObjectId> {
        self.persistent_gc.reachable_for_test()
    }
}
```

- [ ] **Step 5: Run focused tests to verify GREEN**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_commit_epilogue -- --format terse
```

Expected: the commit epilogue test passes.

- [ ] **Step 6: Run Wave 9B tests**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc -- --format terse
cargo test -p wasmtime --lib persistent_object_marker -- --format terse
```

Expected: all persistent GC and marker tests pass.

- [ ] **Step 7: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_gc.rs
git commit -m "Wire persistent GC commit epilogue"
```

## Task 5: Add Tombstone Record Encoding

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_heap.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_object_tombstone -- --format terse`

- [ ] **Step 1: Write the failing tombstone encoding test**

Add this test in `transaction.rs`:

```rust
#[test]
fn persistent_object_tombstone_publication_uses_deleted_header_flag() {
    let object = ObjectId { object_index: 41 };
    let publication = persist::PendingPublication::persistent_object_tombstone(
        crate::runtime::vm::PackedGranuleDomain::TStruct,
        object.object_index,
        7,
        ObjectKind::Struct,
    )
    .unwrap();

    let header = TxObjectHeader::read_from_prefix(&publication.payload).unwrap();
    assert_eq!(header.object_id, object.object_index);
    assert_eq!(header.version, 7);
    assert_eq!(header.kind, ObjectKind::Struct as u16);
    assert_eq!(header.flags, object_heap::TX_OBJECT_FLAG_DELETED);
    assert_eq!(header.type_layout_id, 0);
    assert_eq!(
        header.record_len,
        core::mem::size_of::<TxObjectHeader>() as u64
    );
}
```

- [ ] **Step 2: Run the test to verify RED**

Run:

```bash
cargo test -p wasmtime --lib persistent_object_tombstone_publication_uses_deleted_header_flag -- --format terse
```

Expected: compile failure naming missing `TX_OBJECT_FLAG_DELETED` or
`persistent_object_tombstone`.

- [ ] **Step 3: Add tombstone flag and encoder**

Add to `object_heap.rs`:

```rust
pub(crate) const TX_OBJECT_FLAG_DELETED: u16 = 0x0001;

pub(crate) fn encode_object_tombstone_record(
    object_id: ObjectId,
    version: u32,
    kind: ObjectKind,
) -> Result<Vec<u8>> {
    let header = TxObjectHeader {
        record_len: TxObjectHeader::BYTE_LEN as u64,
        object_id: object_id.object_index,
        version,
        kind: kind as u16,
        flags: TX_OBJECT_FLAG_DELETED,
        type_layout_id: 0,
    };
    Ok(header.as_bytes().to_vec())
}
```

Add to `persist.rs`:

```rust
pub(crate) fn persistent_object_tombstone(
    domain: PackedGranuleDomain,
    object_id: u64,
    version: u32,
    kind: crate::runtime::transaction::ObjectKind,
) -> Result<Self> {
    Ok(Self {
        logical_id: pack_object_granule_id(domain, object_id)?,
        version,
        kind: domain as u16,
        type_layout_id: 0,
        payload: crate::runtime::transaction::object_heap::encode_object_tombstone_record(
            crate::runtime::transaction::ObjectId {
                object_index: object_id,
            },
            version,
            kind,
        )?,
    })
}
```

If `object_heap` is private to `transaction.rs`, expose a narrow wrapper from
`transaction.rs` instead:

```rust
fn encode_object_tombstone_record_for_publication(
    object_id: ObjectId,
    version: u32,
    kind: ObjectKind,
) -> Result<Vec<u8>> {
    object_heap::encode_object_tombstone_record(object_id, version, kind)
}
```

Then call that wrapper from `persist.rs`.

- [ ] **Step 4: Run focused tests to verify GREEN**

Run:

```bash
cargo test -p wasmtime --lib persistent_object_tombstone -- --format terse
```

Expected: tombstone encoding test passes.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_heap.rs crates/wasmtime/src/runtime/transaction/persist.rs
git commit -m "Encode persistent object tombstones"
```

## Task 6: Teach Recovery Tombstone Precedence

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib object_tombstone_recovery -- --format terse`

- [ ] **Step 1: Write failing recovery precedence tests**

Add these tests in `transaction.rs`:

```rust
#[test]
fn object_tombstone_recovery_higher_delete_suppresses_live_record() {
    let mut region =
        crate::runtime::vm::block_region::VMemoryBlockRegion::new_for_test(32).unwrap();
    let stream1 = region.alloc_stream(1).unwrap();
    let stream2 = region.alloc_stream(2).unwrap();
    region
        .append_type_layout_metadata(&recovery_test_struct_layout(7))
        .unwrap();
    let live = encoded_object_publication_for_recovery_test(
        41,
        1,
        7,
        ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)]),
    );
    let delete = persist::PendingPublication::persistent_object_tombstone(
        crate::runtime::vm::PackedGranuleDomain::TStruct,
        41,
        2,
        ObjectKind::Struct,
    )
    .unwrap();
    append_committed_object_winner(&mut region, 1, stream1, 0, &live);
    append_committed_object_winner(&mut region, 2, stream2, 0, &delete);

    let recovered = crate::runtime::vm::recover_region_for_test(&region).unwrap();
    assert!(recovered.committed_object_winners().unwrap().is_empty());
}

#[test]
fn object_tombstone_recovery_older_delete_loses_to_newer_live_record() {
    let mut region =
        crate::runtime::vm::block_region::VMemoryBlockRegion::new_for_test(32).unwrap();
    let stream1 = region.alloc_stream(1).unwrap();
    let stream2 = region.alloc_stream(2).unwrap();
    region
        .append_type_layout_metadata(&recovery_test_struct_layout(7))
        .unwrap();
    let delete = persist::PendingPublication::persistent_object_tombstone(
        crate::runtime::vm::PackedGranuleDomain::TStruct,
        41,
        1,
        ObjectKind::Struct,
    )
    .unwrap();
    let live = encoded_object_publication_for_recovery_test(
        41,
        2,
        7,
        ObjectPayload::Struct(vec![ObjectValue::I32(2), ObjectValue::Ref(None)]),
    );
    append_committed_object_winner(&mut region, 1, stream1, 0, &delete);
    append_committed_object_winner(&mut region, 2, stream2, 0, &live);

    let recovered = crate::runtime::vm::recover_region_for_test(&region).unwrap();
    let winners = recovered.committed_object_winners().unwrap();
    assert_eq!(winners.len(), 1);
    assert_eq!(winners[0].object_id, 41);
    assert_eq!(winners[0].version, 2);
}
```

- [ ] **Step 2: Run tests to verify RED**

Run:

```bash
cargo test -p wasmtime --lib object_tombstone_recovery -- --format terse
```

Expected: recovery rejects the tombstone record because `type_layout_id` is zero
or because the object record has no payload.

- [ ] **Step 3: Implement recovery selection with tombstones**

In `recovery.rs`, add:

```rust
#[derive(Clone)]
enum ObjectRecoveryCandidate {
    Live(RecoveredObjectWinner),
    Deleted {
        object_id: u64,
        version: u32,
        kind: u16,
    },
}

impl ObjectRecoveryCandidate {
    fn version(&self) -> u32 {
        match self {
            ObjectRecoveryCandidate::Live(winner) => winner.version,
            ObjectRecoveryCandidate::Deleted { version, .. } => *version,
        }
    }
}
```

Change `object_winners` in `replay_object_winners` to:

```rust
let mut object_winners = BTreeMap::<u64, ObjectRecoveryCandidate>::new();
```

After reading `object_header`, add tombstone handling before layout validation:

```rust
let is_deleted =
    object_header.flags & crate::runtime::transaction::object_heap::TX_OBJECT_FLAG_DELETED != 0;
if is_deleted {
    ensure!(
        object_header.type_layout_id == 0,
        "recovered object tombstone must not reference a type layout"
    );
    ensure!(
        data_header.type_info == 0,
        "recovered object tombstone outer type layout id must be zero"
    );
    ensure!(
        object_header.record_len == TxObjectHeader::BYTE_LEN as u64,
        "recovered object tombstone record must contain only the base header"
    );
    let candidate = ObjectRecoveryCandidate::Deleted {
        object_id,
        version: winner.version,
        kind: object_header.kind,
    };
    merge_object_recovery_candidate(&mut object_winners, object_id, candidate)?;
    continue;
}
```

Add helper:

```rust
fn merge_object_recovery_candidate(
    object_winners: &mut BTreeMap<u64, ObjectRecoveryCandidate>,
    object_id: u64,
    candidate: ObjectRecoveryCandidate,
) -> Result<()> {
    match object_winners.get(&object_id) {
        Some(current) if current.version() > candidate.version() => Ok(()),
        Some(current) if current.version() == candidate.version() => {
            bail!(
                "duplicate committed object version {} for object id {}",
                candidate.version(),
                object_id
            )
        }
        _ => {
            object_winners.insert(object_id, candidate);
            Ok(())
        }
    }
}
```

At the end of `replay_object_winners`, return only live candidates:

```rust
Ok(object_winners
    .into_values()
    .filter_map(|candidate| match candidate {
        ObjectRecoveryCandidate::Live(winner) => Some(winner),
        ObjectRecoveryCandidate::Deleted { .. } => None,
    })
    .collect())
```

- [ ] **Step 4: Run focused tests to verify GREEN**

Run:

```bash
cargo test -p wasmtime --lib object_tombstone_recovery -- --format terse
```

Expected: both tombstone recovery tests pass.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs crates/wasmtime/src/runtime/transaction.rs
git commit -m "Recover persistent object tombstones"
```

## Task 7: Add Basic Sweep Plan And Tombstone Publications

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_gc_sweep -- --format terse`

- [ ] **Step 1: Write failing sweep-plan tests**

Add these tests in `transaction.rs`:

```rust
#[test]
fn persistent_gc_sweep_plan_lists_unreachable_persistent_objects() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x581, vec![ObjectValue::I32(1)])
        .unwrap();
    let garbage = objects
        .allocate_persistent_array_for_gc_ref(0x582, vec![ObjectValue::I32(2)])
        .unwrap();

    let report = PersistentObjectMarker::mark(&objects, [root]).unwrap();
    let sweep = PersistentObjectSweepPlan::from_mark_report(&objects, &report).unwrap();

    assert_eq!(
        sweep.tombstones,
        vec![PersistentObjectTombstoneCandidate {
            object_id: garbage,
            kind: ObjectKind::Array,
            next_version: objects.next_object_record_version_for_gc(garbage).unwrap(),
        }]
    );
}

#[test]
fn persistent_gc_sweep_plan_builds_tombstone_publications() {
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref(0x583, vec![ObjectValue::I32(1)])
        .unwrap();
    let garbage = objects
        .allocate_persistent_struct_for_gc_ref(0x584, vec![ObjectValue::I32(2)])
        .unwrap();

    let report = PersistentObjectMarker::mark(&objects, [root]).unwrap();
    let sweep = PersistentObjectSweepPlan::from_mark_report(&objects, &report).unwrap();
    let publications = sweep.tombstone_publications().unwrap();

    assert_eq!(publications.len(), 1);
    let (domain, object_id) =
        crate::runtime::vm::unpack_object_granule_id(publications[0].logical_id).unwrap();
    assert_eq!(domain, crate::runtime::vm::PackedGranuleDomain::TStruct);
    assert_eq!(object_id, garbage.object_index);
}
```

- [ ] **Step 2: Run tests to verify RED**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_sweep -- --format terse
```

Expected: compile failure naming missing `PersistentObjectSweepPlan` or
`PersistentObjectTombstoneCandidate`.

- [ ] **Step 3: Add sweep-plan types**

Add to `object_gc.rs`:

```rust
use super::ObjectKind;
use crate::runtime::vm::PackedGranuleDomain;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PersistentObjectTombstoneCandidate {
    pub(crate) object_id: ObjectId,
    pub(crate) kind: ObjectKind,
    pub(crate) next_version: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentObjectSweepPlan {
    pub(crate) tombstones: Vec<PersistentObjectTombstoneCandidate>,
}

impl PersistentObjectSweepPlan {
    pub(crate) fn from_mark_report(
        objects: &ObjectTable,
        report: &PersistentObjectMarkReport,
    ) -> Result<Self> {
        let mut tombstones = Vec::new();
        for &object_id in &report.unreachable_persistent {
            let kind = objects.kind(object_id)?;
            if matches!(kind, ObjectKind::Struct | ObjectKind::Array) {
                tombstones.push(PersistentObjectTombstoneCandidate {
                    object_id,
                    kind,
                    next_version: objects.next_object_record_version_for_gc(object_id)?,
                });
            }
        }
        Ok(Self { tombstones })
    }

    pub(crate) fn tombstone_publications(&self) -> Result<Vec<persist::PendingPublication>> {
        self.tombstones
            .iter()
            .map(|candidate| {
                let domain = match candidate.kind {
                    ObjectKind::Struct => PackedGranuleDomain::TStruct,
                    ObjectKind::Array => PackedGranuleDomain::TArray,
                    ObjectKind::I31 | ObjectKind::Extern | ObjectKind::Func => {
                        bail!("object kind {:?} cannot be tombstoned as a persistent object granule", candidate.kind)
                    }
                };
                persist::PendingPublication::persistent_object_tombstone(
                    domain,
                    candidate.object_id.object_index,
                    candidate.next_version,
                    candidate.kind,
                )
            })
            .collect()
    }
}
```

Add to `ObjectTable` in `transaction.rs`:

```rust
fn next_object_record_version_for_gc(&self, object_id: ObjectId) -> Result<u32> {
    self.live_slot(object_id)?;
    self.next_record_version
        .checked_add(1)
        .context("object record version overflow")
}
```

Update re-exports with:

```rust
PersistentObjectSweepPlan, PersistentObjectTombstoneCandidate,
```

- [ ] **Step 4: Run focused tests to verify GREEN**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_sweep -- --format terse
```

Expected: sweep-plan tests pass.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_gc.rs
git commit -m "Plan persistent object tombstones from GC"
```

## Task 8: End-To-End Tombstone Publication And Recovery

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_gc_tombstone_file_backed -- --format terse`

- [ ] **Step 1: Write failing file-backed tombstone test**

Add this test in `transaction.rs`:

```rust
#[test]
fn persistent_gc_tombstone_file_backed_recovery_drops_unreachable_object() {
    let dir = tempfile::tempdir().unwrap();
    let tx_log_path = dir.path().join("tx-log.bin");
    let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64).unwrap();
    let mut state = TransactionState::new_for_test_with_durable_log(
        TransactionId::from_raw(591),
        durable_log,
    );
    let mut objects = ObjectTable::default();
    let root = objects
        .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
            0x591,
            591,
            1,
            vec![ObjectValue::I32(1)],
        )
        .unwrap();
    let garbage = objects
        .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
            0x592,
            591,
            2,
            vec![ObjectValue::I32(2)],
        )
        .unwrap();

    state.acquire_object_write(&objects, root).unwrap();
    state.stage_struct_field(&objects, root, 0, ObjectValue::I32(11)).unwrap();
    state.acquire_object_write(&objects, garbage).unwrap();
    state
        .stage_struct_field(&objects, garbage, 0, ObjectValue::I32(22))
        .unwrap();
    let mut publications = Vec::new();
    assert!(
        state
            .commit_object_payloads_into(&mut objects, &mut publications)
            .unwrap()
    );
    let marker = state
        .publish_object_publications_before_commit(591, 591, &objects, &publications)
        .unwrap()
        .unwrap();
    state.publish_commit_lp(591, 591, marker).unwrap();
    clear_current_thread_transaction_for_test();

    let report = PersistentObjectMarker::mark(&objects, [root]).unwrap();
    let sweep = PersistentObjectSweepPlan::from_mark_report(&objects, &report).unwrap();
    let tombstones = sweep.tombstone_publications().unwrap();
    let marker = state
        .publish_object_publications_before_commit(592, 592, &objects, &tombstones)
        .unwrap()
        .unwrap();
    state.publish_commit_lp(592, 592, marker).unwrap();
    drop(state);

    let recovered_region =
        crate::runtime::vm::block_region::reopen_and_recover_file_backed_region_for_test(
            &tx_log_path,
        )
        .unwrap();
    let winners = recovered_region.committed_object_winners().unwrap();
    assert_eq!(winners.len(), 1);
    assert_eq!(winners[0].object_id, root.object_index);
    assert!(winners.iter().all(|winner| winner.object_id != garbage.object_index));
}
```

- [ ] **Step 2: Run the test to verify RED**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_tombstone_file_backed_recovery_drops_unreachable_object -- --format terse
```

Expected: failure if tombstone publication or recovery filtering is incomplete.

- [ ] **Step 3: Fix integration issues exposed by the file-backed test**

Apply only fixes required by the failure. Expected integration fixes are:

```rust
// publish_object_publications_before_commit must not require a type layout for
// tombstone publications where type_layout_id == 0.
if publication.is_object_tombstone()? {
    continue;
}
```

Add this helper to `PendingPublication` if needed:

```rust
pub(crate) fn is_object_tombstone(&self) -> Result<bool> {
    if self.kind != PackedGranuleDomain::TStruct as u16
        && self.kind != PackedGranuleDomain::TArray as u16
    {
        return Ok(false);
    }
    let header = crate::runtime::transaction::TxObjectHeader::read_from_prefix(&self.payload)?;
    Ok(header.flags & crate::runtime::transaction::object_heap::TX_OBJECT_FLAG_DELETED != 0)
}
```

- [ ] **Step 4: Run focused tests to verify GREEN**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_tombstone_file_backed -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
```

Expected: file-backed tombstone recovery passes and existing persistence tests
remain green.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/persist.rs crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs
git commit -m "Recover file-backed persistent object tombstones"
```

## Task 9: Documentation And Final Verification

**Files:**
- Modify: `docs/shisoft/2026-06-14-persistent-object-gc-wave1-design.md`
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify: `docs/shisoft/transactional-wasm-remaining-work-roadmap.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Update the design docs**

Record the implemented 9B/9C status:

```markdown
## Persistent GC 9B/9C Status

- Commit-coupled incremental marking is implemented with volatile
  `PersistentGcState`, commit deltas, direct child marking, and bounded
  minibatches.
- Basic durable tombstones are implemented as committed object publications with
  `TX_OBJECT_FLAG_DELETED`.
- Recovery selects the highest committed object version; a winning tombstone
  suppresses older live records.
- Block/chunk reuse, Immix line reclamation, compaction, and persistent
  object-index storage remain outside this plan.
```

- [ ] **Step 2: Update roadmap checkboxes**

In `docs/shisoft/transactional-wasm-remaining-work-roadmap.md`, mark Wave 9B and
the implemented part of Wave 9C complete. Move block/chunk reclamation and
Immix line reuse into a separate named reclamation wave.

- [ ] **Step 3: Run final verification**

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

- [ ] **Step 4: Commit docs**

Run:

```bash
git add docs/shisoft/2026-06-14-persistent-object-gc-wave1-design.md docs/shisoft/transactional-wasm-runtime-core-design.md docs/shisoft/transactional-wasm-remaining-work-roadmap.md docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Document persistent GC 9B and 9C"
```

## Completion Criteria

- `PersistentObjectMarker::mark` remains available and passes all existing Wave
  9A tests.
- `PersistentGcState` can make bounded marking progress across multiple commit
  minibatches.
- Commit barrier deltas enqueue new roots and direct children of already marked
  objects.
- Object publication deltas are derived from committed persistent object
  publications.
- Tombstone records are encoded as object publications with
  `TX_OBJECT_FLAG_DELETED`.
- Recovery suppresses older live object records when a higher-version tombstone
  wins.
- No task in this plan reuses blocks, updates Immix line marks, compacts
  records, or persists an object index.

## Rollback Plan

- If Task 1 causes broad compile churn, keep `PersistentObjectMarker` in
  `transaction.rs` and create `object_gc.rs` only for `PersistentGcState`; do
  not change public names used by existing tests.
- If Task 4 commit-epilogue wiring conflicts with current runtime commit paths,
  keep `persistent_gc_after_object_commit` as an explicit transaction helper and
  require call sites to invoke it after LP publication in this wave.
- If Task 6 tombstone recovery conflicts with object layout validation, keep the
  tombstone flag and tests, but make `replay_object_winners` return a separate
  `deleted_object_ids` set until the winner enum refactor is complete.
- If file-backed tombstone publication exposes ordering issues, keep in-memory
  recovery tests and split file-backed tombstone publication into a separate
  named task without reverting marker or commit-barrier work.
