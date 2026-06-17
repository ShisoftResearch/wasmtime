# Persistent GC Maintenance Transaction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** add a GC maintenance transaction that evacuates live records out of mixed live/dead object-data chunks and retires the old chunks after the copied records commit.

**Architecture:** The cleaner is a physical copying pass over durable object-data chunks. It preserves `ObjectId`, republishes live current records as higher-version `TObjectPub` entries in a GC-owned transaction stream, then retires the old mixed chunks through existing block-generation metadata. Recovery before LP ignores copies; recovery after LP chooses higher versions; recovery after retirement ignores old records.

**Tech Stack:** Rust, Wasmtime transaction runtime, file-backed `TxDurableLog`, block/chunk region metadata, existing persistent object recovery and mark-sweep infrastructure.

---

## File Map

- Modify `crates/wasmtime/src/runtime/transaction/object_gc.rs`
  - Add chunk summary/report structs for compaction results.
- Modify `crates/wasmtime/src/runtime/transaction.rs`
  - Add `ObjectTable` helper to refresh a persistent object record for GC copying.
  - Add `TransactionState` GC maintenance transaction entry point.
  - Add file-backed mixed-chunk evacuation tests.
- Modify `crates/wasmtime/src/runtime/transaction/persist.rs`
  - Add durable-log backend hooks for object chunk compaction/retirement when needed.
- Modify `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
  - Add helper to summarize and retire evacuated mixed object-data chunks when needed.
- Update `docs/shisoft/transactional-wasm-runtime-core-design.md`
  - Fold in the committed maintenance-transaction design.
- Update `docs/shisoft/transactional-wasm-implementation-log.md`
  - Record the compaction milestone and remaining limits.

## Task 1: Test Mixed-Chunk Evacuation Semantics

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`

- [ ] **Step 1: Add a failing file-backed mixed-chunk evacuation test**

Add a test near `file_backed_persistent_gc_keeps_mixed_live_dead_object_chunk`:

```rust
#[test]
fn file_backed_persistent_gc_evacuates_mixed_chunk_and_retires_old_chunk() {
    let dir = tempfile::tempdir().unwrap();
    let tx_log_path = dir.path().join("persistent-gc-evacuate-mixed.bin");
    let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64).unwrap();
    let mut state = TransactionState::new_for_test_with_durable_log(
        TransactionId::from_raw(0x900),
        durable_log,
    );
    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
    let mut objects = ObjectTable::default();
    let live = objects
        .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
            0x9000,
            900,
            1,
            vec![ObjectValue::I32(1)],
        )
        .unwrap();
    let dead = objects
        .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
            0x9001,
            900,
            2,
            vec![ObjectValue::I32(2)],
        )
        .unwrap();

    state.acquire_object_write(&mut objects, live).unwrap();
    state.stage_struct_field(&objects, live, 0, ObjectValue::I32(11)).unwrap();
    state.acquire_object_write(&mut objects, dead).unwrap();
    state.stage_struct_field(&objects, dead, 0, ObjectValue::I32(22)).unwrap();
    state.stage_global(0, GlobalSnapshot::GcRef(0x9000)).unwrap();
    commit_active_file_backed_publications_for_test(0x900, 0x900, &mut objects, &mut state)
        .unwrap();

    let (before_recovered, before_winners) =
        recover_file_backed_recovery_inputs_for_test(&tx_log_path).unwrap();
    let live_before = recovered_object_winner_by_id_for_test(&before_winners, live).clone();
    let dead_before = recovered_object_winner_by_id_for_test(&before_winners, dead).clone();
    let (old_chunk, old_generation) =
        file_backed_recovered_chunk_meta_for_test(&tx_log_path, &live_before).unwrap();
    assert_eq!(
        old_chunk,
        file_backed_recovered_chunk_meta_for_test(&tx_log_path, &dead_before)
            .unwrap()
            .0
    );

    let mut rebuilt = ObjectTable::default();
    let report = rebuilt
        .rebuild_reachable_from_recovery_for_test(
            &before_recovered.type_layouts,
            &before_winners,
            &before_recovered.root_object_ids,
        )
        .unwrap();
    assert_eq!(report.mark.reachable, object_set([live]));
    assert_eq!(report.mark.unreachable_persistent, object_set([dead]));

    let compact = state
        .compact_persistent_object_chunks_for_test(&mut objects, &tx_log_path, &report)
        .unwrap();
    assert_eq!(compact.copied_objects, vec![live]);
    assert_eq!(compact.retired_chunks, vec![old_chunk]);

    let (_, after_winners) = recover_file_backed_recovery_inputs_for_test(&tx_log_path).unwrap();
    let live_after = recovered_object_winner_by_id_for_test(&after_winners, live);
    assert!(after_winners.iter().all(|winner| winner.object_id != dead.object_index));
    assert!(live_after.version > live_before.version);
    let (new_chunk, new_generation) =
        file_backed_recovered_chunk_meta_for_test(&tx_log_path, live_after).unwrap();
    assert_ne!(new_chunk, old_chunk);
    assert_eq!(new_generation, old_generation);

    let object_c = ObjectId { object_index: 901 };
    crate::runtime::vm::block_region::publish_committed_struct_object(
        &tx_log_path,
        901,
        object_c.object_index,
        1,
        12,
        &[3, 4],
    )
    .unwrap();
    let (_, reused_winners) = recover_file_backed_recovery_inputs_for_test(&tx_log_path).unwrap();
    let object_c_winner = recovered_object_winner_by_id_for_test(&reused_winners, object_c);
    let (reused_chunk, reused_generation) =
        file_backed_recovered_chunk_meta_for_test(&tx_log_path, object_c_winner).unwrap();
    assert_eq!(reused_chunk, old_chunk);
    assert_eq!(reused_generation, old_generation + 1);
}
```

- [ ] **Step 2: Verify the test fails**

Run:

```bash
cargo test -p wasmtime --lib file_backed_persistent_gc_evacuates_mixed_chunk_and_retires_old_chunk -- --exact
```

Expected: fail because `compact_persistent_object_chunks_for_test` and the compaction report do not exist.

## Task 2: Add Compaction Report Types

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_gc.rs`

- [ ] **Step 1: Add report structs**

Add:

```rust
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentObjectCompactionReport {
    pub(crate) copied_objects: Vec<ObjectId>,
    pub(crate) retired_chunks: Vec<u32>,
    pub(crate) skipped_chunks: Vec<u32>,
}
```

- [ ] **Step 2: Re-run the failing test**

Run the exact test command from Task 1. Expected: still fail because the cleaner method is missing.

## Task 3: Add ObjectTable GC Copy Helper

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`

- [ ] **Step 1: Add a focused unit test**

Add a test near `transaction_object_id_stays_stable_when_record_handle_changes`:

```rust
#[test]
fn persistent_gc_copy_refreshes_record_version_without_changing_object_id() {
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
            0x9100,
            910,
            1,
            vec![ObjectValue::I32(7)],
        )
        .unwrap();
    let before = objects.pending_publication_for_test(object).unwrap();
    let copied = objects
        .persistent_gc_copy_publication_for_test(object)
        .unwrap();

    assert_eq!(before.logical_id, copied.logical_id);
    assert!(copied.version > before.version);
    assert_eq!(objects.payload(object).unwrap(), ObjectPayload::Struct(vec![ObjectValue::I32(7)]));
}
```

- [ ] **Step 2: Verify the test fails**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_copy_refreshes_record_version_without_changing_object_id -- --exact
```

Expected: fail because `persistent_gc_copy_publication_for_test` does not exist.

- [ ] **Step 3: Implement the helper**

Add to `impl ObjectTable`:

```rust
fn persistent_gc_copy_publication(&mut self, object_id: ObjectId) -> Result<persist::PendingPublication> {
    ensure!(
        self.is_persistent(object_id)?,
        "persistent GC copy requires a persistent object: {object_id:?}"
    );
    let payload = self.payload(object_id)?;
    self.update_payload(object_id, payload)?;
    self.object_pending_publication(object_id)
}

#[cfg(test)]
fn persistent_gc_copy_publication_for_test(
    &mut self,
    object_id: ObjectId,
) -> Result<persist::PendingPublication> {
    self.persistent_gc_copy_publication(object_id)
}
```

- [ ] **Step 4: Verify the helper test passes**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_copy_refreshes_record_version_without_changing_object_id -- --exact
```

Expected: pass.

## Task 4: Implement GC Maintenance Transaction Cleaner

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`

- [ ] **Step 1: Add cleaner implementation**

Add a method on `TransactionState` that:

1. rejects active and suspended transactions;
2. reopens the file-backed region and reads committed object winners;
3. builds a chunk-to-winners map;
4. selects chunks that contain at least one reachable and one unreachable winner;
5. calls `ObjectTable::persistent_gc_copy_publication` for reachable objects in those chunks;
6. publishes copied records to a new stream id;
7. flips LP;
8. retires the old chunk with `retire_unreachable_object_chunks_for_test`;
9. returns `PersistentObjectCompactionReport`.

The initial implementation may be file-backed only because durable compaction requires physical block/chunk metadata.

- [ ] **Step 2: Verify mixed-chunk evacuation passes**

Run:

```bash
cargo test -p wasmtime --lib file_backed_persistent_gc_evacuates_mixed_chunk_and_retires_old_chunk -- --exact
```

Expected: pass.

## Task 5: Crash-Ordering Tests

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`

- [ ] **Step 1: Add pre-LP crash test**

Add a test that writes copied object records and log entries without LP, drops the state, recovers, and asserts the old live winner still wins.

- [ ] **Step 2: Add post-LP pre-retirement test**

Add a test that commits copied object records with LP but skips retirement, recovers, and asserts the copied higher-version winner wins while the old chunk has not been retired.

- [ ] **Step 3: Run both tests**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_compaction_crash -- --format terse
```

Expected: both pass after implementation.

## Task 6: Docs and Verification

**Files:**
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
- Modify: `docs/shisoft/transactional-wasm-remaining-work-roadmap.md`

- [ ] **Step 1: Update docs**

State that mixed live/dead chunk evacuation is implemented by a GC maintenance transaction, while line-level Immix reuse, background collection, and object-id reuse remain future work.

- [ ] **Step 2: Run focused verification**

Run:

```bash
cargo fmt --check
git diff --check
cargo test -p wasmtime --lib persistent_gc -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
```

Expected: all pass.

- [ ] **Step 3: Run transaction WAST gate**

Run:

```bash
CARGO_TARGET_DIR=/tmp/wasmtime-persistent-gc-compaction-target CARGO_INCREMENTAL=0 WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_BUILD_JOBS=2 python3 ./ci/run-tests.py --locked --exclude=wasi-preview1-component-adapter -- --format terse
```

Expected: exit code 0.

