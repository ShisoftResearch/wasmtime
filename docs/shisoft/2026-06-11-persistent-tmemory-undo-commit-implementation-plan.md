# Persistent TMemory Undo Commit Implementation Plan

> Superseded note, 2026-06-11: this plan captured the first undo-before-in-place
> slice. The later unified-log slice in
> `docs/shisoft/2026-06-11-unified-tmemory-durable-log-plan.md` moved durable
> stream ownership out of `TMemory` and into `TransactionState`. Mentions of
> `TMemory::commit_transaction_granules_with_undo` and per-`TMemory` test log
> inspection below are historical, not the current implementation shape.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move persistent `tmemory` commits from redo-style publication toward undo-before-in-place persistence while keeping volatile `VMemory` behavior unchanged.

**Architecture:** Durable object/global/table publication records use the role name `TObjectPub`. Persistent linear-memory commits keep the existing transaction workspace until commit, then log old granule bytes as `TMemoryUndo`, write staged granules in place, flush/fence through the backend, and publish an LP. Recovery exposes rollback records and `TMemory` gets a helper to apply them to the base image.

**Tech Stack:** Rust, Wasmtime runtime internals, `crates/wasmtime/src/runtime/vm/memory/tmemory`, `crates/wasmtime/src/runtime/transaction`, `crates/wasmtime/src/runtime/vm/libcalls.rs`, unit tests with `cargo test -p wasmtime --lib`.

---

### Task 1: Rename Publication Role To TObjectPub

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
- Test: `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`

- [x] **Step 1: Update tests to expect `TObjectPub`**

The role round-trip/default-role assertions should use:

```rust
assert_eq!(entry.role().unwrap(), TxLogEntryRole::TObjectPub);
```

Add a data-record role assertion if one is missing:

```rust
let header = TxDataRecordHeader {
    logical_id: 0x1000_0000_0000_0007,
    version: 3,
    kind: PackedGranuleDomain::TMemory as u16,
    role: 0,
    payload_len: 4,
    type_info: 0,
};
assert_eq!(header.role().unwrap(), TxDataRecordRole::TObjectPub);
```

- [x] **Step 2: Run focused test to verify failure**

Run:

```bash
cargo test -p wasmtime --lib tmemory_undo_log_entry_role_roundtrips -- --format terse
```

Expected: compile failure until the enum variants are renamed.

- [x] **Step 3: Rename enum variants and usages**

The target enum variants are:

```rust
TxLogEntryRole::TObjectPub
TxDataRecordRole::TObjectPub
```

Keep the on-media encoding unchanged:

```rust
0 => TObjectPub
1 => TMemoryUndo
```

- [x] **Step 4: Update docs**

Replace role-specific prose saying `Publication` with `TObjectPub`. Keep ordinary prose such as "publication records" when referring to the concept rather than the enum role.

- [x] **Step 5: Verify**

Run:

```bash
cargo test -p wasmtime --lib tmemory_undo -- --format terse
```

Expected: all `tmemory_undo` tests pass.

### Task 2: Add Persistent TMemory Undo Commit API

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Test: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`

- [x] **Step 1: Add failing tests**

Add tests that demonstrate persistent and volatile backend differences:

```rust
#[test]
fn nvmemory_commit_staged_granules_logs_undo_before_in_place_write() {
    let mut memory = TMemory::new_with_backend_limits(TMemoryBackend::NVMemory, 1, Some(1)).unwrap();
    memory.commit_range(0, &[1, 2, 3, 4]).unwrap();

    memory
        .commit_transaction_granules_with_undo(7, 12, Some(3), 0, &[(0, vec![9; TMEMORY_GRANULE_SIZE])])
        .unwrap();

    assert_eq!(memory.read_committed(0..4).unwrap(), vec![9, 9, 9, 9]);
    let entries = memory.durable_log_entries_for_test(7);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].role().unwrap(), TxLogEntryRole::TMemoryUndo);
    assert!((entries[1].tx_meta & 1) != 0);
}

#[test]
fn vmemory_commit_staged_granules_keeps_volatile_commit_path() {
    let mut memory = TMemory::new_vmemory(1).unwrap();
    memory
        .commit_transaction_granules_with_undo(7, 12, Some(3), 0, &[(0, vec![9; TMEMORY_GRANULE_SIZE])])
        .unwrap();

    assert_eq!(memory.read_committed(0..4).unwrap(), vec![9, 9, 9, 9]);
    assert!(memory.durable_log_entries_for_test(7).is_empty());
}
```

- [x] **Step 2: Run tests to verify failure**

Run:

```bash
cargo test -p wasmtime --lib commit_staged_granules -- --format terse
```

Expected: compile failure because the commit helper and test accessors do not exist.

- [x] **Step 3: Add compact `TMemory` logical-id packing**

Add a helper in `durable_log.rs`:

```rust
pub(crate) fn pack_tmemory_granule_id(
    instance: Option<u32>,
    memory_index: u32,
    granule_index: u64,
) -> Result<u64>
```

Use low 60 bits as:

```text
instance: 20 bits, `None` encoded as 0
memory_index: 12 bits
granule_index: 28 bits
```

Return an error if any field does not fit.

- [x] **Step 4: Implement `TMemory::commit_transaction_granules_with_undo`**

Add:

```rust
pub(crate) fn commit_transaction_granules_with_undo(
    &mut self,
    stream_id: u32,
    txid: u32,
    owner_instance: Option<u32>,
    memory_index: u32,
    staged: &[(u64, Vec<u8>)],
) -> Result<()>
```

Behavior:

- If `self.backend() == TMemoryBackend::VMemory`, call `commit_range` for each staged granule and do not write undo logs.
- For `NVMemory` and `FileBackedMemory`, for each staged granule:
  - read old granule bytes from committed storage,
  - create `PendingGranuleUndo::tmemory(logical_id, version, old_bytes)`,
  - call `StreamPublisher::publish_tmemory_undo_before_in_place_write`,
  - call `commit_range(addr, bytes)`.
- After all persistent granules are written, publish final LP using the last returned marker.
- If `staged` is empty, return `Ok(())`.

- [x] **Step 5: Verify**

Run:

```bash
cargo test -p wasmtime --lib commit_staged_granules -- --format terse
cargo test -p wasmtime --lib tmemory_undo -- --format terse
```

Expected: new tests and existing undo tests pass.

### Task 3: Route Runtime Commit Through Persistent Undo Path

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `crates/wasmtime/src/runtime/transaction.rs`

- [x] **Step 1: Add failing integration-style unit test**

Add a test near the existing `transaction_commit_copies_staged_granules_to_tmemory` coverage:

```rust
#[test]
fn transaction_commit_uses_persistent_tmemory_undo_for_nvmemory() {
    let mut tmemory =
        crate::runtime::vm::TMemory::new_with_backend_limits(TMemoryBackend::NVMemory, 1, Some(1))
            .expect("tmemory");
    tmemory.commit_range(0, &[1, 2, 3, 4]).unwrap();
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(12));

    state
        .stage_tmemory_write_for_test(0, 0, 4, &[9, 8, 7, 6], &tmemory)
        .expect("stage tmemory");
    state
        .commit_tmemory_for_test(&mut tmemory)
        .expect("commit tmemory");

    assert_eq!(tmemory.read_committed(4..8).expect("read committed"), vec![9, 8, 7, 6]);
    assert_eq!(tmemory.durable_log_entries_for_test(12).len(), 2);
}
```

- [x] **Step 2: Run test to verify failure**

Run:

```bash
cargo test -p wasmtime --lib transaction_commit_uses_persistent_tmemory_undo_for_nvmemory -- --format terse
```

Expected: failure because `commit_tmemory_owned` still calls `commit_range` directly.

- [x] **Step 3: Expose active transaction raw id**

Add a small method:

```rust
pub(crate) fn active_transaction_required_raw(&self) -> Result<u64>
```

which returns `self.active_transaction_required()?.as_raw()`.

- [x] **Step 4: Route staged granule commit through the new helper**

Update both:

- `TransactionState::commit_tmemory_owned`
- `apply_staged_transaction_record` for `StagedRecord::MemoryGranule`

to call `TMemory::commit_transaction_granules_with_undo` instead of looping over `commit_range` directly.

Convert `TransactionId` to `u32` with checked conversion and use the transaction id as `stream_id` for the current single-thread research stream.

- [x] **Step 5: Verify**

Run:

```bash
cargo test -p wasmtime --lib transaction_commit_uses_persistent_tmemory_undo_for_nvmemory -- --format terse
cargo test -p wasmtime --lib transaction_commit_copies_staged_granules_to_tmemory -- --format terse
```

Expected: persistent undo and volatile commit tests pass.

### Task 4: Apply Recovered TMemory Undo Rollbacks

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Test: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`

- [x] **Step 1: Add failing rollback-apply test**

Add:

```rust
#[test]
fn recovered_tmemory_undo_rollbacks_restore_base_bytes() {
    let mut memory = TMemory::new_with_backend_limits(TMemoryBackend::NVMemory, 1, Some(1)).unwrap();
    memory.commit_range(0, &[9, 9, 9, 9]).unwrap();
    let rollback = recovery::RecoveredTMemoryUndoRollback {
        logical_id: pack_tmemory_granule_id(Some(3), 0, 0).unwrap(),
        version: 1,
        data_block: 0,
        data_offset: 0,
        old_granule_bytes: vec![1; TMEMORY_GRANULE_SIZE],
    };

    memory.apply_recovered_tmemory_undo_rollbacks([rollback]).unwrap();

    assert_eq!(memory.read_committed(0..4).unwrap(), vec![1, 1, 1, 1]);
}
```

- [x] **Step 2: Run test to verify failure**

Run:

```bash
cargo test -p wasmtime --lib recovered_tmemory_undo_rollbacks_restore_base_bytes -- --format terse
```

Expected: compile failure because rollback apply is not implemented.

- [x] **Step 3: Add unpack helper and rollback apply**

Add:

```rust
pub(crate) fn unpack_tmemory_granule_id(logical_id: u64) -> Result<(Option<u32>, u32, u64)>
```

Add `TMemory::apply_recovered_tmemory_undo_rollbacks` that:

- filters for `PackedGranuleDomain::TMemory`,
- decodes the granule index,
- checks `old_granule_bytes.len()` matches the current granule length,
- writes old bytes back with `commit_range`.

- [x] **Step 4: Verify**

Run:

```bash
cargo test -p wasmtime --lib recovered_tmemory_undo_rollbacks_restore_base_bytes -- --format terse
cargo test -p wasmtime --lib tmemory -- --format terse
```

Expected: rollback test and tmemory suite pass.

### Task 5: Full Verification And Notes

**Files:**
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`

- [x] **Step 1: Update current-status notes**

Record that this wave implements persistent-backend staged-commit undo logging and rollback-apply helpers, but does not yet implement real process-restart wiring from file-backed log scan into instance construction.

- [x] **Step 2: Run final checks**

Run:

```bash
cargo test -p wasmtime --lib tmemory -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
CARGO_INCREMENTAL=0 WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
cargo fmt --check
git diff --check
```

- [x] **Step 3: Report remaining gap**

State clearly that the persistent backend now logs undo during staged commit, but full restart recovery still needs instance/backing-file discovery and invocation of `apply_recovered_tmemory_undo_rollbacks` during backend reopen.
