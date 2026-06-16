# Wave 6 Object Permission Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:subagent-driven-development` or `superpowers:executing-plans` to
> implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** Finish the remaining Wave 6 object permission and transaction
semantics hardening items from the transactional object model stabilization
roadmap.

**Architecture:** Runtime object permissions are keyed by
`GranuleId::Object(ObjectId)`. This wave verifies that object operations obey
the same lock-based optimistic-read/pessimistic-write transaction model as
tmemory, globals, and tables. Durable object record domains remain unchanged.

**Tech Stack:** Rust, Wasmtime runtime transaction tests,
`TransactionState`, `ObjectTable`, `LockBased`, `TMemory`.

---

## Scope

Implement runtime test coverage, the object-aware trap rollback fix exposed by
that coverage, and roadmap/log status updates. Do not change WAST files,
durable log encoding, object headers, or the transaction object ABI.

The Wave 6 identity cleanup is already committed in:

```text
073e05df5 Use ObjectId granules for transaction object permissions
```

This plan completes the remaining Wave 6 checklist:

- object writes acquire lock-based ownership
- abort/trap/`tfail` release ownership and discard staged object changes
- lower-id conflict preemption frees object records allocated by the aborted
  suspended transaction
- generic transaction terminal paths reject object-cleanup cases unless the
  object-aware API receives an `ObjectTable`
- object reads/writes require an active transaction
- two transaction ids conflict on the same object
- mixed tmemory/object transactions keep correct conflict and commit behavior
- transactional table element writes stay private until commit
- transactional table growth stays private to the transaction until commit
  while the staged region remains readable and writable by transactional ops
- transactional table range-bound helpers use staged table size for privately
  grown regions; bulk table data COW remains deferred behind the existing mock
  boundary

---

## Task 1: Add Object Conflict And Mixed Runtime Tests

**Files:**

- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/func.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`

**Tests:**

- Add tests to the existing `#[cfg(test)] mod tests` in
  `crates/wasmtime/src/runtime/transaction.rs`.

- [x] **Step 1.1: Add a failing object write conflict test**

Add this test near the existing lock-based transaction-id tests:

```rust
#[test]
fn lower_transaction_id_aborts_higher_suspended_object_writer() {
    clear_current_thread_transaction_for_test();
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_persistent_struct_for_gc_ref(0x6601, vec![ObjectValue::I32(1)])
        .unwrap();
    let mut state = TransactionState::default();
    let higher = TransactionId::from_raw(100_001);
    let lower = TransactionId::from_raw(1);

    state.enter_transaction(higher).unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_struct_field(&mut objects, object, 0, ObjectValue::I32(100))
        .unwrap();
    state.restore_transaction(None).unwrap();
    assert!(state.transaction_is_open(higher));

    state.enter_transaction(lower).unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_struct_field(&mut objects, object, 0, ObjectValue::I32(7))
        .unwrap();

    assert!(!state.transaction_is_open(higher));
    assert!(state.transaction_is_open(lower));
    assert!(state.owns_object_write(object));
    assert_eq!(
        state.read_struct_field(&mut objects, object, 0).unwrap(),
        ObjectValue::I32(7)
    );

    state.abort().unwrap();
    assert_eq!(current_thread_transaction_for_test(), None);
    assert_eq!(
        objects.payload(object).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(1)])
    );
    clear_current_thread_transaction_for_test();
}
```

Run:

```sh
cargo test -p wasmtime lower_transaction_id_aborts_higher_suspended_object_writer --lib
```

Expected before any required fix: fail if object writes do not participate in
the lock-based transaction-id conflict scheme. If it already passes, keep it as
the regression test proving the existing implementation satisfies the roadmap
item.

- [x] **Step 1.2: Add a failing higher-writer conflict test**

Add this test next to the previous test:

```rust
#[test]
fn higher_transaction_id_cannot_write_lower_owned_object() {
    clear_current_thread_transaction_for_test();
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_persistent_struct_for_gc_ref(0x6602, vec![ObjectValue::I32(1)])
        .unwrap();
    let mut state = TransactionState::default();
    let lower = TransactionId::from_raw(1);
    let higher = TransactionId::from_raw(100_001);

    state.enter_transaction(lower).unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_struct_field(&mut objects, object, 0, ObjectValue::I32(11))
        .unwrap();
    state.restore_transaction(None).unwrap();

    state.enter_transaction(higher).unwrap();
    let error = state.acquire_object_write(&mut objects, object).unwrap_err();
    assert!(
        error.to_string().contains("transaction write conflict"),
        "{error:?}"
    );
    assert!(state.transaction_is_open(lower));
    assert!(state.transaction_is_open(higher));
    assert!(!state.owns_object_write(object));

    state.abort().unwrap();
    assert_eq!(current_thread_transaction_for_test(), None);
    state.restore_transaction(Some(lower)).unwrap();
    assert!(state.owns_object_write(object));
    assert_eq!(
        state.read_struct_field(&mut objects, object, 0).unwrap(),
        ObjectValue::I32(11)
    );

    state.abort().unwrap();
    assert_eq!(current_thread_transaction_for_test(), None);
    clear_current_thread_transaction_for_test();
}
```

Run:

```sh
cargo test -p wasmtime higher_transaction_id_cannot_write_lower_owned_object --lib
```

Expected before any required fix: fail if a higher transaction can acquire the
same object write lock owned by a lower transaction.

- [x] **Step 1.3: Add a failing mixed tmemory/object conflict and commit test**

Add this test near the object conflict tests:

```rust
#[test]
fn mixed_object_and_tmemory_transaction_survives_object_conflict_and_commits_both() {
    clear_current_thread_transaction_for_test();
    let mut tmemory = crate::runtime::vm::TMemory::new_vmemory(1).expect("tmemory");
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_persistent_struct_for_gc_ref(0x6603, vec![ObjectValue::I32(1)])
        .unwrap();
    let mut state = TransactionState::default();
    let higher = TransactionId::from_raw(100_001);
    let lower = TransactionId::from_raw(1);
    let lower_addr = TMEMORY_GRANULE_SIZE as u64 + 8;

    state.enter_transaction(higher).unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_struct_field(&mut objects, object, 0, ObjectValue::I32(100))
        .unwrap();
    state
        .stage_tmemory_write_for_test(0, 0, 4, &[1, 2, 3, 4], &tmemory)
        .unwrap();
    state.restore_transaction(None).unwrap();

    state.enter_transaction(lower).unwrap();
    state
        .stage_tmemory_write_for_test(0, 0, lower_addr, &[5, 6, 7, 8], &tmemory)
        .unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();
    state
        .stage_struct_field(&mut objects, object, 0, ObjectValue::I32(7))
        .unwrap();

    assert!(!state.transaction_is_open(higher));
    assert!(state.owns_object_write(object));
    assert!(state.owns_memory_granule_write_owned(
        Some(InstanceId::from_u32(0)),
        0,
        1
    ));

    assert!(state.commit_tmemory_for_test(&mut tmemory).unwrap());
    assert!(state.commit_object_payloads(&mut objects).unwrap());
    state.complete_commit().unwrap();
    assert_eq!(current_thread_transaction_for_test(), None);

    assert_eq!(tmemory.read_committed(0..4).unwrap(), vec![0, 0, 0, 0]);
    let lower_range = lower_addr as usize..lower_addr as usize + 4;
    assert_eq!(tmemory.read_committed(lower_range).unwrap(), vec![5, 6, 7, 8]);
    assert_eq!(
        objects.payload(object).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::I32(7)])
    );
    clear_current_thread_transaction_for_test();
}
```

Run:

```sh
cargo test -p wasmtime mixed_object_and_tmemory_transaction_survives_object_conflict_and_commits_both --lib
```

Expected before any required fix: fail if conflict abort leaks the higher
transaction's tmemory/object staging, or if the lower mixed transaction cannot
commit both sides.

- [x] **Step 1.4: Add an object trap cleanup regression**

Add this test near the existing object `tfail` regression:

```rust
#[test]
fn transaction_object_tstruct_trap_frees_new_object_record() {
    clear_current_thread_transaction_for_test();
    let mut config = crate::Config::new();
    config.wasm_gc(true);
    let engine = crate::Engine::new(&config).unwrap();
    let module = transaction_test_module(
        &engine,
        r#"
        (module
          (type $s (struct (field (mut i32))))
          (tfunc (export "create_trap")
            (drop (tstruct.new $s (i32.const 41)))
            (unreachable))
          (tfunc (export "create_ok") (result i32)
            (local $s (tref $s))
            (local.set $s (tstruct.new $s (i32.const 7)))
            (tstruct.get $s 0 (tref.cast_read (local.get $s)))))
        "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let create_trap = instance
        .get_typed_func::<(), ()>(&mut store, "create_trap")
        .unwrap();
    let create_ok = instance
        .get_typed_func::<(), i32>(&mut store, "create_ok")
        .unwrap();

    assert!(create_trap.call(&mut store, ()).is_err());
    assert_eq!(current_thread_transaction_for_test(), None);
    assert_eq!(store.transaction_object_table().live_count(), 0);

    assert_eq!(create_ok.call(&mut store, ()).unwrap(), 7);
    assert_eq!(current_thread_transaction_for_test(), None);
    assert_eq!(store.transaction_object_table().live_count(), 1);
}
```

Run:

```sh
cargo test -p wasmtime transaction_object_tstruct_trap_frees_new_object_record --lib
```

Expected before any required fix: fail if an object allocation followed by a
trap leaves a staged object record live or leaves the thread-local transaction
set.

- [x] **Step 1.5: Make export trap rollback object-aware**

In `crates/wasmtime/src/runtime/func.rs`, update
`invoke_wasm_and_catch_traps` so a trapped active transaction aborts through
`TransactionState::abort_allocated_objects` with the store's `ObjectTable`,
instead of calling `TransactionState::abort` directly:

```rust
let result = crate::runtime::vm::catch_traps(store, &mut previous_runtime_state, closure);
if result.is_err() {
    let (transaction, object_table) = store.0.transaction_state_and_object_table_mut();
    if transaction.active_transaction().is_some() {
        let _ = transaction.abort_allocated_objects(object_table);
    }
}
```

Run:

```sh
cargo test -p wasmtime transaction_object_tstruct_trap_frees_new_object_record --lib
```

Expected after the fix: pass.

- [x] **Step 1.6: Add existing-object staged rollback coverage**

Add this test near the object trap cleanup regression:

```rust
#[test]
fn transaction_object_existing_staged_write_rolls_back_on_tfail_and_trap() {
    clear_current_thread_transaction_for_test();
    let mut config = crate::Config::new();
    config.wasm_gc(true);
    let engine = crate::Engine::new(&config).unwrap();
    let module = transaction_test_module(
        &engine,
        r#"
        (module
          (type $s (struct (field (mut i32))))
          (tfunc (export "new") (result (ref $s))
            (tstruct.new $s (i32.const 1)))
          (tfunc (export "read") (param (ref $s)) (result i32)
            (tstruct.get $s 0 (tref.cast_read (local.get 0))))
          (tfunc (export "write_fail") (param (ref $s))
            (tstruct.set $s 0 (tref.cast_write (local.get 0)) (i32.const 99))
            (tfail))
          (tfunc (export "write_trap") (param (ref $s))
            (tstruct.set $s 0 (tref.cast_write (local.get 0)) (i32.const 77))
            (unreachable))
          (tfunc (export "write_ok") (param (ref $s) i32)
            (tstruct.set $s 0 (tref.cast_write (local.get 0)) (local.get 1))))
        "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let new = instance.get_func(&mut store, "new").unwrap();
    let read = instance.get_func(&mut store, "read").unwrap();
    let write_fail = instance.get_func(&mut store, "write_fail").unwrap();
    let write_trap = instance.get_func(&mut store, "write_trap").unwrap();
    let write_ok = instance.get_func(&mut store, "write_ok").unwrap();
    let mut object_result = [crate::Val::null_any_ref()];
    let mut read_result = [crate::Val::I32(0)];
    let mut no_results: [crate::Val; 0] = [];

    new.call(&mut store, &[], &mut object_result).unwrap();
    let object = object_result[0];
    assert_eq!(store.transaction_object_table().live_count(), 1);

    read.call(&mut store, &[object], &mut read_result).unwrap();
    assert_eq!(read_result[0].unwrap_i32(), 1);

    write_fail
        .call(&mut store, &[object], &mut no_results)
        .unwrap();
    assert_eq!(current_thread_transaction_for_test(), None);
    read.call(&mut store, &[object], &mut read_result).unwrap();
    assert_eq!(read_result[0].unwrap_i32(), 1);

    write_trap
        .call(&mut store, &[object], &mut no_results)
        .unwrap_err();
    assert_eq!(current_thread_transaction_for_test(), None);
    read.call(&mut store, &[object], &mut read_result).unwrap();
    assert_eq!(read_result[0].unwrap_i32(), 1);

    write_ok
        .call(&mut store, &[object, crate::Val::I32(5)], &mut no_results)
        .unwrap();
    assert_eq!(current_thread_transaction_for_test(), None);
    read.call(&mut store, &[object], &mut read_result).unwrap();
    assert_eq!(read_result[0].unwrap_i32(), 5);
    assert_eq!(store.transaction_object_table().live_count(), 1);
}
```

Run:

```sh
cargo test -p wasmtime transaction_object_existing_staged_write_rolls_back_on_tfail_and_trap --lib
```

Expected: pass, proving `tfail` and ordinary trap both discard staged writes to
an already-committed persistent object and release ownership for a later write.

- [x] **Step 1.7: Add conflict-aborted allocation cleanup**

Update `TransactionState` so lower-id conflict preemption does not lose track of
objects allocated by the discarded suspended workspace:

- Add a `pending_conflict_aborted_allocated_objects: Vec<ObjectId>` cleanup
  queue to `TransactionState`.
- In `discard_conflict_aborted_transaction`, append the discarded workspace's
  `allocated_objects` in reverse allocation order.
- Add `drain_conflict_aborted_allocated_objects(&mut self, &mut ObjectTable)`.
- Drain the queue from object-aware boundaries:
  `acquire_object_read`, `acquire_object_write`,
  `commit_object_payloads_with`, `abort_allocated_objects`, and
  `abort_transaction_allocated_objects`.

Add these tests near the object conflict tests:

```rust
#[test]
fn object_conflict_aborted_suspended_transaction_frees_new_object_records() {
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_persistent_struct_for_gc_ref(0x6604, vec![ObjectValue::I32(1)])
        .unwrap();
    let mut state = TransactionState::default();
    let higher = TransactionId::from_raw(100_001);
    let lower = TransactionId::from_raw(1);

    state.enter_transaction(higher).unwrap();
    let allocated = objects.allocate_struct(vec![ObjectValue::I32(99)]).unwrap();
    state.record_allocated_object(allocated).unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();
    state.restore_transaction(None).unwrap();

    state.enter_transaction(lower).unwrap();
    state.acquire_object_write(&mut objects, object).unwrap();

    assert!(!state.transaction_is_open(higher));
    assert_eq!(objects.live_count(), 1);
    assert!(objects.kind(allocated).is_err());
}

#[test]
fn tmemory_conflict_aborted_allocations_are_reclaimed_before_object_commit() {
    let tmemory = crate::runtime::vm::TMemory::new_vmemory(1).expect("tmemory");
    let mut objects = ObjectTable::default();
    let mut state = TransactionState::default();
    let higher = TransactionId::from_raw(100_001);
    let lower = TransactionId::from_raw(1);

    state.enter_transaction(higher).unwrap();
    let allocated = objects.allocate_struct(vec![ObjectValue::I32(99)]).unwrap();
    state.record_allocated_object(allocated).unwrap();
    state
        .stage_tmemory_write_for_test(0, 0, 0, &[1, 2, 3, 4], &tmemory)
        .unwrap();
    state.restore_transaction(None).unwrap();

    state.enter_transaction(lower).unwrap();
    state
        .stage_tmemory_write_for_test(0, 0, 0, &[5, 6, 7, 8], &tmemory)
        .unwrap();
    assert!(!state.transaction_is_open(higher));
    assert_eq!(objects.live_count(), 1);

    assert!(!state.commit_object_payloads(&mut objects).unwrap());
    assert_eq!(objects.live_count(), 0);
    assert!(objects.kind(allocated).is_err());
}
```

Run:

```sh
cargo test -p wasmtime object_conflict_aborted_suspended_transaction_frees_new_object_records --lib
cargo test -p wasmtime tmemory_conflict_aborted_allocations_are_reclaimed_before_object_commit --lib
```

Expected: both commands pass.

- [x] **Step 1.8: Keep `ttable.set` private until commit**

Add `transaction_ttable_set_is_private_until_commit` in
`crates/wasmtime/src/runtime/transaction.rs`. The test imports a host callback
called from inside an active `tfunc` after `ttable.set` but before the boundary
commit. The host callback reads the exported table through the embedder API and
must still observe the committed value.

Run before the fix:

```sh
cargo test -p wasmtime transaction_ttable_set_is_private_until_commit --lib
```

Expected before the fix: FAIL because the host callback sees the staged
function reference.

Then remove the eager `write_table_element_snapshot` call from
`transaction_ttable_set_impl` in
`crates/wasmtime/src/runtime/vm/libcalls.rs`. `ttable.set` should only stage a
`TableElement` record; `ttable.get` already reads that staged overlay, and
`transaction_commit_impl` applies the staged record to the live table.

Run after the fix:

```sh
cargo test -p wasmtime transaction_ttable_set_is_private_until_commit --lib
cargo test -p wasmtime mock_transaction_ttable_funcref_paths_hit_runtime_libcalls --lib
cargo test -p wasmtime mock_transaction_ttable_bulk_funcref_paths_hit_runtime_libcalls --lib
```

Expected: all commands exit 0.

- [x] **Step 1.9: Make privately grown table regions visible to the transaction**

Add `transaction_ttable_grow_new_region_uses_staged_overlay` in
`crates/wasmtime/src/runtime/transaction.rs`. The test should run a `tfunc`
that grows a `ttable`, reads the newly grown element, writes it, and then
confirms the host-visible table is updated only after commit.

Run before the fix:

```sh
cargo test -p wasmtime transaction_ttable_grow_new_region_uses_staged_overlay --lib
```

Expected before the fix: FAIL with an out-of-bounds table access after
`ttable.grow`, because `ttable.get` checks the committed table size.

Then add staged-size-aware transactional table bounds checks in
`crates/wasmtime/src/runtime/vm/libcalls.rs` and use them from
`transaction_ttable_get_impl`, `transaction_ttable_set_impl`,
`transaction_ttable_read_range_impl`, and `transaction_ttable_write_range_impl`.
Keep the ordinary committed-table bounds helpers unchanged for non-transactional
commit replay.

Run after the fix:

```sh
cargo test -p wasmtime transaction_ttable_grow_new_region_uses_staged_overlay --lib
cargo test -p wasmtime transaction_ttable_set_is_private_until_commit --lib
cargo test -p wasmtime mock_transaction_ttable_funcref_paths_hit_runtime_libcalls --lib
cargo test -p wasmtime mock_transaction_ttable_bulk_funcref_paths_hit_runtime_libcalls --lib
```

Expected: all commands exit 0.

- [x] **Step 1.10: Prove `ttable.grow` is host-private until commit**

Add `transaction_ttable_grow_is_private_until_commit` in
`crates/wasmtime/src/runtime/transaction.rs`. The test imports a host callback
called after `ttable.grow`, `ttable.get`, and `ttable.set` in the newly grown
region. The host callback reads the exported table through the embedder API and
must still observe the committed table size and no visible new slot.

Run:

```sh
cargo test -p wasmtime transaction_ttable_grow_is_private_until_commit --lib
```

Expected: exit 0.

- [x] **Step 1.11: Guard generic terminal paths that cannot clean object records**

Add regressions in `crates/wasmtime/src/runtime/transaction.rs`:

- `generic_abort_rejects_active_object_allocations_without_object_table`
- `generic_commit_rejects_pending_conflict_aborted_object_allocations`
- `generic_abort_transaction_rejects_suspended_object_allocations_without_object_table`

Run:

```sh
cargo test -p wasmtime generic_abort_rejects_active_object_allocations_without_object_table --lib
cargo test -p wasmtime generic_commit_rejects_pending_conflict_aborted_object_allocations --lib
cargo test -p wasmtime generic_abort_transaction_rejects_suspended_object_allocations_without_object_table --lib
```

Expected: all commands exit 0.

- [x] **Step 1.12: Add direct staged-size range-bound coverage**

Add `transaction_table_range_bounds_use_staged_size_for_private_grow` in
`crates/wasmtime/src/runtime/vm/libcalls.rs`. The test stages a private table
size larger than the committed table and directly calls the read/write-range
helpers in the private region. It does not claim bulk table data COW is
complete; the existing `ttable.fill` / `ttable.copy` / `ttable.init` data path
still carries `SHISOFT-TWASM-MOCK`.

Run:

```sh
cargo test -p wasmtime transaction_table_range_bounds_use_staged_size_for_private_grow --lib
```

Expected: exit 0.

- [x] **Step 1.13: Verify full transaction unit suite**

Run:

```sh
cargo fmt --check
git diff --check
cargo test -p wasmtime transaction_ttable_set_is_private_until_commit --lib
cargo test -p wasmtime transaction_ttable_grow_new_region_uses_staged_overlay --lib
cargo test -p wasmtime transaction_ttable_grow_is_private_until_commit --lib
cargo test -p wasmtime transaction_table_range_bounds_use_staged_size_for_private_grow --lib
cargo test -p wasmtime generic_abort_rejects_active_object_allocations_without_object_table --lib
cargo test -p wasmtime generic_commit_rejects_pending_conflict_aborted_object_allocations --lib
cargo test -p wasmtime generic_abort_transaction_rejects_suspended_object_allocations_without_object_table --lib
cargo test -p wasmtime transaction:: --lib
```

Expected: all commands exit 0.

---

## Task 2: Update Wave 6 Status Documentation

**Files:**

- Modify:
  `docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [x] **Step 2.1: Mark Wave 6 checklist items complete**

In
`docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md`,
mark the remaining Wave 6 checklist items complete after Task 1 tests pass.

- [x] **Step 2.2: Add a dated log entry**

Add a short entry to
`docs/shisoft/transactional-wasm-implementation-log.md`:

```markdown
## 2026-06-16 Wave 6 Object Permission Hardening

- Added runtime regressions for object write ownership under the lock-based
  transaction-id scheme.
- Added two-transaction object conflict coverage for lower-id preemption and
  higher-id conflict rejection.
- Added mixed tmemory/object transaction coverage proving object conflicts do
  not leak aborted staged tmemory or object state, and a surviving transaction
  can commit both sides.
- Made the exported-Wasm trap path use object-aware transaction abort so newly
  allocated persistent object records are freed on ordinary Wasm traps.
- Added an existing-object staged-write rollback regression covering both
  `tfail` and ordinary trap paths, followed by a successful write to prove the
  object lock can be reacquired.
- Added conflict-aborted allocation cleanup: lower-id conflict preemption now
  queues allocated objects from discarded suspended workspaces and object-aware
  boundaries free those records. Regression coverage includes direct object
  conflicts and tmemory conflicts drained before object commit.
- Hardened generic transaction terminal paths: generic abort/commit helpers now
  reject pending object cleanup cases that require an `ObjectTable`, and
  object-aware abort cleanup clears the active transaction even when cleanup
  reports an error.
- Removed the eager live-table write from `ttable.set`. Transactional table
  writes now stay private in `TransactionState` until commit, matching the
  existing `ttable.get` staged-overlay behavior and avoiding suspended-conflict
  rollback holes for live table state.
- Added staged-size-aware `ttable` bounds checks so a transaction that privately
  grows a table can use `ttable.get`/`ttable.set` in the newly grown region
  before commit. Added a direct range-bound regression for read/write-range
  helper bounds; full transactional bulk data COW remains behind the existing
  bulk-table mock boundary.
- Added host-callback coverage proving `ttable.grow` remains host-private until
  commit: embedder table size and element access still observe committed state
  during an active transaction.
- Active-transaction, abort, object trap, and `tfail` tests complete the Wave 6
  semantic gate.

Verification:

```sh
cargo fmt --check
git diff --check
cargo test -p wasmtime transaction_ttable_set_is_private_until_commit --lib
cargo test -p wasmtime transaction_ttable_grow_new_region_uses_staged_overlay --lib
cargo test -p wasmtime transaction_ttable_grow_is_private_until_commit --lib
cargo test -p wasmtime transaction_table_range_bounds_use_staged_size_for_private_grow --lib
cargo test -p wasmtime generic_abort_rejects_active_object_allocations_without_object_table --lib
cargo test -p wasmtime generic_commit_rejects_pending_conflict_aborted_object_allocations --lib
cargo test -p wasmtime generic_abort_transaction_rejects_suspended_object_allocations_without_object_table --lib
cargo test -p wasmtime transaction:: --lib
```
```

- [x] **Step 2.3: Verify docs**

Run:

```sh
rg -n "Wave 6|object conflict|GranuleId::Object" docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md docs/shisoft/transactional-wasm-implementation-log.md
cargo fmt --check
git diff --check
```

Expected: all commands exit 0.

---

## Task 3: Review And Commit Wave 6

**Files:**

- Review all files changed by Tasks 1 and 2.

- [x] **Step 3.1: Run final verification**

Run:

```sh
cargo fmt --check
git diff --check
cargo test -p wasmtime transaction_ttable_set_is_private_until_commit --lib
cargo test -p wasmtime transaction_ttable_grow_new_region_uses_staged_overlay --lib
cargo test -p wasmtime transaction_ttable_grow_is_private_until_commit --lib
cargo test -p wasmtime transaction_table_range_bounds_use_staged_size_for_private_grow --lib
cargo test -p wasmtime generic_abort_rejects_active_object_allocations_without_object_table --lib
cargo test -p wasmtime generic_commit_rejects_pending_conflict_aborted_object_allocations --lib
cargo test -p wasmtime generic_abort_transaction_rejects_suspended_object_allocations_without_object_table --lib
cargo test -p wasmtime transaction:: --lib
```

Expected: all commands exit 0.

- [ ] **Step 3.2: Commit**

Commit without any `Co-authored-by` annotation:

```sh
git add crates/wasmtime/src/runtime/func.rs \
  crates/wasmtime/src/runtime/transaction.rs \
  crates/wasmtime/src/runtime/vm/libcalls.rs \
  docs/shisoft/2026-06-16-transactional-object-model-stabilization-implementation-plan.md \
  docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md \
  docs/shisoft/transactional-wasm-implementation-log.md \
  docs/shisoft/2026-06-16-wave6-object-permission-hardening-plan.md
git commit -m "Harden transaction object permission semantics"
```
