# Transactional Wasm Next Test Waves Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the next correctness tests for persistent roots, table/global permission semantics, and mixed object-redo plus linear-memory-undo recovery while keeping `loom` deferred.

**Architecture:** This plan is test-first and should not change transaction semantics unless a test exposes a real bug. Root/recovery tests live beside the block-region recovery scanner, permission models live beside `TransactionState`, and backend conformance scenarios live beside `TxDurableLog`.

**Tech Stack:** Rust unit tests, Wasmtime runtime test helpers, file-backed transaction log backend, deterministic model tests, `cargo test`.

---

### Task 1: Persistent Root Recovery Tests

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Test: `cargo test -p wasmtime --lib root -- --format terse`

- [ ] **Step 1: Write failing root-version and loose-end tests**

Add tests near `recovery_rebuilds_persistent_object_root_from_tglobal`:

```rust
#[test]
fn recovery_selects_latest_committed_tglobal_root_version() {
    let region = sample_region_with_rewritten_global_root();
    let recovered = recover_region_for_test(&region).unwrap();

    assert_eq!(recovered.root_object_ids, vec![42]);
}

#[test]
fn recovery_ignores_loose_end_ttable_root_update() {
    let region = sample_region_with_loose_end_table_root_update();
    let recovered = recover_region_for_test(&region).unwrap();

    assert_eq!(recovered.root_object_ids, vec![41]);
}
```

These tests should initially fail to compile because the sample helpers do not exist.

- [ ] **Step 2: Verify RED**

Run:

```bash
cargo test -p wasmtime --lib root -- --format terse
```

Expected: compile failure naming the missing sample helpers.

- [ ] **Step 3: Add minimal recovery sample helpers**

Add helpers in the same test module:

```rust
fn sample_region_with_rewritten_global_root() -> VMemoryBlockRegion {
    let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
    let stream = region.alloc_stream(1).unwrap();
    let logical_id = pack_test_granule_id(PackedGranuleDomain::TGlobal, 1);
    append_committed_root_update(&mut region, 1, stream, 0, logical_id, 1, &[Some(41)]);
    append_committed_root_update(&mut region, 1, stream, 1, logical_id, 2, &[Some(42)]);
    region
}

fn sample_region_with_loose_end_table_root_update() -> VMemoryBlockRegion {
    let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
    let committed_stream = region.alloc_stream(1).unwrap();
    let loose_stream = region.alloc_stream(2).unwrap();
    let logical_id = pack_test_granule_id(PackedGranuleDomain::TTable, 1);
    append_committed_root_update(&mut region, 1, committed_stream, 0, logical_id, 1, &[Some(41)]);
    append_loose_end_root_update(&mut region, 2, loose_stream, 0, logical_id, 2, &[Some(42)]);
    region
}
```

Add `append_loose_end_root_update` by reusing the same payload encoding as `append_committed_root_update`, but write a non-final log entry only.

- [ ] **Step 4: Verify GREEN**

Run:

```bash
cargo test -p wasmtime --lib root -- --format terse
```

Expected: root-related tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs
git commit -m "Add persistent root recovery tests"
```

### Task 2: Table and Global Permission Model Tests

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib model_permissions -- --format terse`

- [ ] **Step 1: Write failing generic granule permission tests**

Add table/global tests near `model_permissions_generic_granule_state_is_not_object_specific`:

```rust
#[test]
fn model_permissions_ttable_granule_reacquires_after_abort() {
    assert_generic_granule_reacquires_after_abort(GranuleId::TTable {
        instance: Some(3),
        table_index: 2,
        granule_index: 1,
    });
}

#[test]
fn model_permissions_ttable_size_reacquires_after_commit() {
    assert_generic_granule_reacquires_after_commit(GranuleId::TTableSize {
        instance: Some(3),
        table_index: 2,
    });
}

#[test]
fn model_permissions_tglobal_reacquires_after_commit() {
    assert_generic_granule_reacquires_after_commit(GranuleId::TGlobal {
        instance: Some(3),
        global_index: 4,
    });
}
```

These tests should initially fail to compile because the assertion helpers do not exist.

- [ ] **Step 2: Verify RED**

Run:

```bash
cargo test -p wasmtime --lib model_permissions -- --format terse
```

Expected: compile failure naming missing generic assertion helpers.

- [ ] **Step 3: Add minimal generic permission helpers**

Add helpers in `mod model_permissions`:

```rust
fn assert_generic_granule_reacquires_after_abort(granule: GranuleId) {
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(101));
    assert!(state.acquire_granule_write(granule, 0).unwrap());
    assert_eq!(
        granule_permission_snapshot(&state, granule),
        GranulePermissionModelState {
            active: true,
            read: true,
            write: true,
        }
    );
    state.abort().unwrap();
    assert_eq!(
        granule_permission_snapshot(&state, granule),
        GranulePermissionModelState {
            active: false,
            read: false,
            write: false,
        }
    );
    state.begin().unwrap();
    assert!(state.acquire_granule_write(granule, 1).unwrap());
    state.abort().unwrap();
    clear_current_thread_transaction_for_test();
}

fn assert_generic_granule_reacquires_after_commit(granule: GranuleId) {
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(102));
    assert!(state.acquire_granule_read(granule, 0).unwrap());
    assert!(state.acquire_granule_write(granule, 0).unwrap());
    state.complete_commit().unwrap();
    assert_eq!(
        granule_permission_snapshot(&state, granule),
        GranulePermissionModelState {
            active: false,
            read: false,
            write: false,
        }
    );
    state.begin().unwrap();
    assert!(state.acquire_granule_read(granule, 1).unwrap());
    assert!(state.acquire_granule_write(granule, 1).unwrap());
    state.abort().unwrap();
    clear_current_thread_transaction_for_test();
}
```

- [ ] **Step 4: Verify GREEN**

Run:

```bash
cargo test -p wasmtime --lib model_permissions -- --format terse
```

Expected: permission model tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction.rs
git commit -m "Add table and global permission model tests"
```

### Task 3: Mixed Recovery Scenarios with Roots

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Test: `cargo test -p wasmtime --lib model_backend_conformance -- --format terse`

- [ ] **Step 1: Write failing mixed root scenarios**

Extend `BackendScenarioRecord` with a root publication variant and add two tests:

```rust
#[test]
fn model_backend_conformance_mixed_commit_with_root_publication() {
    assert_scenario_across_backends(mixed_commit_with_root_publication_scenario());
}

#[test]
fn model_backend_conformance_mixed_loose_end_with_root_publication() {
    assert_scenario_across_backends(mixed_loose_end_with_root_publication_scenario());
}
```

Expected initial failure: missing enum variant/helper functions and recovery summary support for roots.

- [ ] **Step 2: Verify RED**

Run:

```bash
cargo test -p wasmtime --lib model_backend_conformance -- --format terse
```

Expected: compile failure naming missing root scenario pieces.

- [ ] **Step 3: Add root publication support to the backend conformance model**

Add a `TRootPub` variant containing `logical_id`, `version`, and `object_ids`. Treat it as `TxLogEntryRole::TObjectPub` because root publications are durable publications, not undo records. Add `root_object_ids: BTreeSet<u64>` to `BackendRecoverySummary`.

Implement `pending_publication` so root records call:

```rust
let mut publication = PendingPublication::tmemory_for_test(logical_id, version, &encoded_root_payload);
publication.kind = root_publication_kind(logical_id);
publication
```

Add `root_publication_kind` locally in the backend conformance test module:

```rust
fn root_publication_kind(logical_id: u64) -> u16 {
    match logical_id >> 60 {
        value if value == PackedGranuleDomain::TGlobal as u64 => PackedGranuleDomain::TGlobal as u16,
        value if value == PackedGranuleDomain::TTable as u64 => PackedGranuleDomain::TTable as u16,
        other => panic!("unsupported root publication domain {other:#x}"),
    }
}
```

The encoded root payload stores `object_id + 1` little-endian and `0` for nulls, matching `decode_root_object_refs`.

- [ ] **Step 4: Add exact mixed scenarios**

Add:

```rust
fn mixed_commit_with_root_publication_scenario() -> BackendScenario {
    BackendScenario {
        name: "mixed_commit_with_root_publication",
        transactions: vec![BackendScenarioTx {
            stream_id: 123,
            txid: 89,
            records: vec![
                BackendScenarioRecord::TMemoryUndo { logical_id: tmemory_logical_id(9), version: 1, payload_byte: 0xb1 },
                BackendScenarioRecord::TObjectPub { object_id: 62, version: 2, payload_byte: 0xb2 },
                BackendScenarioRecord::TRootPub { logical_id: root_global_logical_id(0), version: 3, object_ids: vec![Some(62)] },
            ],
            commit: true,
        }],
    }
}

fn mixed_loose_end_with_root_publication_scenario() -> BackendScenario {
    BackendScenario {
        name: "mixed_loose_end_with_root_publication",
        transactions: vec![BackendScenarioTx {
            stream_id: 124,
            txid: 90,
            records: vec![
                BackendScenarioRecord::TMemoryUndo { logical_id: tmemory_logical_id(10), version: 1, payload_byte: 0xc1 },
                BackendScenarioRecord::TObjectPub { object_id: 63, version: 2, payload_byte: 0xc2 },
                BackendScenarioRecord::TRootPub { logical_id: root_global_logical_id(0), version: 3, object_ids: vec![Some(63)] },
            ],
            commit: false,
        }],
    }
}
```

Expected recovery: committed scenario has object winner `62`, root id `62`, and no `tmemory` rollback; loose-end scenario has no object/root winner and one `tmemory` rollback.

- [ ] **Step 5: Verify GREEN**

Run:

```bash
cargo test -p wasmtime --lib model_backend_conformance -- --format terse
```

Expected: backend conformance tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/persist.rs
git commit -m "Add mixed root recovery model tests"
```

### Task 4: Final Verification

**Files:**
- No new edits expected.

- [ ] **Step 1: Run focused tests**

```bash
cargo test -p wasmtime --lib root -- --format terse
cargo test -p wasmtime --lib model_permissions -- --format terse
cargo test -p wasmtime --lib model_backend_conformance -- --format terse
```

- [ ] **Step 2: Run transaction gate**

```bash
cargo test -p wasmtime --lib transaction -- --format terse
cargo test -p wasmtime --lib transaction::persist -- --format terse
```

- [ ] **Step 3: Run formatting checks**

```bash
cargo fmt --check
git diff --check
```

- [ ] **Step 4: Report status**

Report commits, test results, and any remaining untracked files.
