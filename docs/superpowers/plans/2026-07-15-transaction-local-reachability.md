# Transaction-Local Reachability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make transactional Wasm object allocation bypass WasmGC while persisting only transaction-local objects reachable from committed persistent roots and discarding every unreachable local object.

**Architecture:** Transactional constructors allocate into `TransactionLocalObjectTable` and receive store-wide monotonic transaction handles. Transaction handle allocation exclusion is monotonic for an `ObjectTable` lifetime: an unpromoted or expired handle remains in `reserved_transaction_ref_handles` as a tombstone until full `ObjectTable` reset, while a promoted handle leaves that set only when installed in the permanent handle mapping, which still prevents reuse. Neither case makes a handle a persistence root. Existing promotion tracing remains the only graph-copy mechanism: persistent roots and persistent-object writes promote reachable local graphs into persistent `ObjectId`s. Commit rebinds handles only for promoted objects, drops unpromoted handles and all local slots, never publishes local objects into the shared volatile object table, and leaves pre-LP durable atomicity unchanged.

**Tech Stack:** Rust, Wasmtime transaction runtime, transactional Wasm/WAT integration tests, existing persistent promotion and mark/sweep machinery.

---

## Invariants

- `tstruct.new*` and `tarray.new*` inside an active transaction allocate only in `TransactionLocalObjectTable`.
- `tglobal`, persistent table/root entries, and edges written into persistent objects are persistence roots or root-reachable edges.
- Ordinary Wasm globals, tables, locals, results, and raw transaction handles are not persistence roots.
- Promotion recursively copies only root-reachable local objects and preserves cycles and aliasing through `promoted_objects`.
- Commit never creates a non-persistent shared object slot for a transaction-local object.
- A handle for a promoted local object is rebound to the promoted persistent `ObjectId`; an unpromoted handle expires.
- Transaction handle allocation exclusion remains monotonic for the `ObjectTable` lifetime: unpromoted or expired handles stay in `reserved_transaction_ref_handles` as tombstones until full `ObjectTable` reset, while promoted handles leave that set only when installed in the permanent handle mapping, which still prevents reuse.
- Abort discards all local slots and local handle mappings.
- Static initialization outside an active transaction remains separate and is not changed by this plan.

## File Structure

- Modify `crates/wasmtime/src/runtime/transaction/object_table.rs`: centralize store-wide transaction-handle reservation and `reserved_transaction_ref_handles` tombstones so local handles do not restart from the same value each transaction.
- Modify `crates/wasmtime/src/runtime/transaction/state.rs`: remove the local handle counter, finalize local objects by rebinding promoted handles and discarding local slots, and stop allocating shared volatile slots at commit.
- Modify `crates/wasmtime/src/runtime/transaction/promotion.rs`: resolve local handles through `TransactionState` until finalization and require a promotion mapping before a local object can become a persistent root.
- Modify `crates/wasmtime/src/runtime/transaction/tests.rs`: add state-level red/green tests and update Wasm execution tests to distinguish persistent roots from ordinary globals/results.
- Modify `docs/superpowers/specs/2026-07-01-kotlin-txref-design.md`: clarify that promoted handles may be rebound while unpromoted handles expire.
- Modify `docs/superpowers/plans/2026-07-01-kotlin-txref-boundary.md`: replace the stale unconditional-publication wording and point runtime execution to this focused plan.

### Task 1: Allocate Transaction-Local Handles Store-Wide

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_table.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Write the failing handle-reuse test**

Add this focused test beside the other transaction-local allocation tests:

```rust
#[test]
fn transaction_local_handles_are_not_reused_after_abort() {
    let mut objects = ObjectTable::default();
    let mut state = TransactionState::default();

    state.begin().unwrap();
    let first = state
        .allocate_transaction_local_struct(vec![ObjectValue::I32(1)], None)
        .unwrap();
    let first_handle = state
        .transaction_ref_handle_for_object_id_avoiding(&mut objects, first, |_| false)
        .unwrap();
    state.abort().unwrap();

    state.begin().unwrap();
    let second = state
        .allocate_transaction_local_struct(vec![ObjectValue::I32(2)], None)
        .unwrap();
    let second_handle = state
        .transaction_ref_handle_for_object_id_avoiding(&mut objects, second, |_| false)
        .unwrap();

    assert_ne!(first_handle, second_handle);
    assert_eq!(
        state.known_object_id_for_transaction_ref_handle(&objects, first_handle),
        None
    );
    state.abort().unwrap();
}
```

- [ ] **Step 2: Run the test and verify RED**

Run:

```bash
cargo test -p wasmtime --lib transaction_local_handles_are_not_reused_after_abort --features transaction -- --nocapture
```

Expected: FAIL at `assert_ne!` because `TransactionLocalObjectTable::default()` restarts its private handle counter.

- [ ] **Step 3: Add store-wide handle reservation**

In `ObjectTable`, extract handle candidate selection from `transaction_ref_handle_for_object_id_avoiding` into a reservation helper. The helper must advance `ObjectTable::next_transaction_ref_handle` without installing an object mapping:

```rust
pub(crate) fn reserve_transaction_ref_handle_avoiding<F>(
    &mut self,
    is_reserved_live_ref_raw: F,
) -> Result<u32>
where
    F: Fn(u32) -> bool,
{
    let start = if self.next_transaction_ref_handle == 0 {
        FIRST_TRANSACTION_OBJECT_REF_HANDLE
    } else {
        self.next_transaction_ref_handle
    };
    let mut candidate = start;
    loop {
        if !self.transaction_ref_handles_to_objects.contains_key(&candidate)
            && !self.live_bridge_gc_refs_to_objects.contains_key(&candidate)
            && !is_reserved_live_ref_raw(candidate)
        {
            self.next_transaction_ref_handle = candidate
                .checked_add(TRANSACTION_OBJECT_REF_HANDLE_STEP)
                .unwrap_or(TRANSACTION_OBJECT_REF_HANDLE_STEP);
            return Ok(candidate);
        }
        candidate = candidate
            .checked_add(TRANSACTION_OBJECT_REF_HANDLE_STEP)
            .unwrap_or(TRANSACTION_OBJECT_REF_HANDLE_STEP);
        ensure!(
            candidate != start,
            "transaction object ref handle space is exhausted"
        );
    }
}
```

Change `ObjectTable::transaction_ref_handle_for_object_id_avoiding` to call this helper and then install its two maps.

- [ ] **Step 4: Make local tables use reserved handles**

Remove `next_transaction_ref_handle` from `TransactionLocalObjectTable`. Split its current allocation method into lookup and association helpers:

```rust
fn transaction_ref_handle_for_object_id(&self, object_id: ObjectId) -> Option<u32> {
    self.objects_to_transaction_ref_handles
        .get(&object_id)
        .copied()
}

fn associate_transaction_ref_handle_for_object_id(
    &mut self,
    handle: u32,
    object_id: ObjectId,
) -> Result<()> {
    self.kind(object_id)?;
    ensure!(
        !self.transaction_ref_handles_to_objects.contains_key(&handle),
        "transaction-local ref handle is already associated"
    );
    self.transaction_ref_handles_to_objects
        .insert(handle, object_id);
    self.objects_to_transaction_ref_handles
        .insert(object_id, handle);
    Ok(())
}
```

Update `TransactionState::transaction_ref_handle_for_object_id_avoiding` so existing local handles are reused within one transaction, while new handles come from `ObjectTable::reserve_transaction_ref_handle_avoiding` and are then associated with the local object.

- [ ] **Step 5: Run the focused handle tests and verify GREEN**

Run:

```bash
cargo test -p wasmtime --lib transaction_local_handles --features transaction -- --nocapture
cargo test -p wasmtime --lib live_ref_bridge_distinguishes_gc_bridge_refs_from_transaction_handles --features transaction -- --nocapture
```

Expected: all selected tests PASS.

- [ ] **Step 6: Commit the handle allocation change**

```bash
git add crates/wasmtime/src/runtime/transaction/object_table.rs \
  crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "fix: reserve transaction-local handles store-wide"
```

### Task 2: Finalize Local Objects Through Reachability

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/promotion.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Replace unconditional-publication expectations with failing reachability tests**

Change `transaction_local_object_allocation_is_isolated_until_commit` so an unrooted object is absent after commit:

```rust
assert!(!state.commit_object_payloads(&mut objects).unwrap());
assert_eq!(objects.live_count(), 0);
assert_eq!(
    objects.known_object_id_for_transaction_ref_handle(handle),
    None
);
state.complete_commit().unwrap();
```

Replace `transaction_local_object_commit_remaps_local_references` with a root-reachability test containing a reachable parent/child graph and one unreachable object:

```rust
#[test]
fn transaction_local_commit_persists_only_root_reachable_graph() {
    let mut objects = ObjectTable::default();
    let mut state = TransactionState::default();

    state.begin().unwrap();
    let child = state
        .allocate_transaction_local_struct(vec![ObjectValue::I32(7)], None)
        .unwrap();
    let root = state
        .allocate_transaction_local_struct(
            vec![ObjectValue::Ref(Some(child)), ObjectValue::Ref(Some(child))],
            None,
        )
        .unwrap();
    let unreachable = state
        .allocate_transaction_local_struct(vec![ObjectValue::I32(99)], None)
        .unwrap();
    let child_handle = state
        .transaction_ref_handle_for_object_id_avoiding(&mut objects, child, |_| false)
        .unwrap();
    let root_handle = state
        .transaction_ref_handle_for_object_id_avoiding(&mut objects, root, |_| false)
        .unwrap();
    let unreachable_handle = state
        .transaction_ref_handle_for_object_id_avoiding(&mut objects, unreachable, |_| false)
        .unwrap();
    state
        .stage_global_owned(None, 0, GlobalSnapshot::GcRef(root_handle))
        .unwrap();

    state
        .promote_persistent_references_before_commit(&mut objects)
        .unwrap();
    let promoted_root = state.promoted_object_for_test(root).unwrap();
    let promoted_child = state.promoted_object_for_test(child).unwrap();
    let mut publications = Vec::new();
    assert!(
        state
            .commit_object_payloads_into(&mut objects, &mut publications)
            .unwrap()
    );

    assert_eq!(objects.live_count(), 2);
    assert!(objects.is_persistent(promoted_root).unwrap());
    assert!(objects.is_persistent(promoted_child).unwrap());
    assert_eq!(
        objects.payload(promoted_root).unwrap(),
        ObjectPayload::Struct(vec![
            ObjectValue::Ref(Some(promoted_child)),
            ObjectValue::Ref(Some(promoted_child)),
        ])
    );
    assert_eq!(
        objects.object_id_for_transaction_ref_handle(root_handle).unwrap(),
        promoted_root
    );
    assert_eq!(
        objects.object_id_for_transaction_ref_handle(child_handle).unwrap(),
        promoted_child
    );
    assert_eq!(
        objects.known_object_id_for_transaction_ref_handle(unreachable_handle),
        None
    );

    let root_delta = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    assert!(root_delta.roots.values().any(|roots| roots.contains(&promoted_root)));
    state
        .complete_commit_with_persistent_root_delta_for_test(root_delta)
        .unwrap();
}
```

Also add `ordinary_global_does_not_keep_transaction_object_alive`: create a
transactional struct in one exported `tfunc`, store its handle in an ordinary
mutable Wasm global, and read it from a second exported `tfunc`. The creating
call must succeed, the shared object count must remain zero, and the later read
must fail because an ordinary global is not a persistent root.

```rust
#[test]
fn ordinary_global_does_not_keep_transaction_object_alive() {
    let mut config = crate::Config::new();
    config.wasm_gc(true);
    let engine = crate::Engine::new(&config).unwrap();
    let module = transaction_test_module(
        &engine,
        r#"
            (module
              (type $s (tstruct (field (mut i32))))
              (global $slot (mut (ref null $s)) (ref.null $s))
              (tfunc (export "new") (result i32)
                (global.set $slot (tstruct.new $s (i32.const 7)))
                (tstruct.get $s 0 (tref.cast_read (global.get $slot))))
              (tfunc (export "read") (result i32)
                (tstruct.get $s 0 (tref.cast_read (global.get $slot)))))
        "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let create = instance
        .get_typed_func::<(), i32>(&mut store, "new")
        .unwrap();
    let read = instance
        .get_typed_func::<(), i32>(&mut store, "read")
        .unwrap();

    assert_eq!(create.call(&mut store, ()).unwrap(), 7);
    assert_eq!(store.transaction_object_table().live_count(), 0);
    assert!(read.call(&mut store, ()).is_err());
}
```

- [ ] **Step 2: Run the reachability tests and verify RED**

Run:

```bash
cargo test -p wasmtime --lib transaction_local_object --features transaction -- --nocapture
cargo test -p wasmtime --lib ordinary_global_does_not_keep_transaction_object_alive --features transaction -- --nocapture
```

Expected: the state tests report shared non-persistent objects and the
ordinary-global read succeeds unexpectedly because
`commit_transaction_local_objects` publishes every local slot.

- [ ] **Step 3: Replace publication with finalization**

Replace `commit_transaction_local_objects` with a finalizer that rebinds only promoted handles, removes local staged payloads, and drops the local table:

```rust
fn finalize_transaction_local_objects_after_promotion(
    &mut self,
    object_table: &mut ObjectTable,
) -> Result<()> {
    self.ensure_active()?;
    let local_ids = self.local_object_table.slots.keys().copied().collect::<Vec<_>>();
    let local_handles = self
        .local_object_table
        .transaction_ref_handles_to_objects
        .clone();

    for (handle, local_id) in local_handles {
        let Some(promoted) = self.promoted_objects.get(&local_id).copied() else {
            continue;
        };
        ensure!(
            object_table.is_persistent(promoted)?,
            "promoted transaction-local object is not persistent"
        );
        object_table.associate_transaction_ref_handle_for_object_id(handle, promoted)?;
    }

    for local_id in local_ids {
        self.staged_objects.remove(&local_id);
    }
    self.local_object_table = TransactionLocalObjectTable::default();
    Ok(())
}
```

In `commit_object_payloads_with`, call the finalizer before collecting updates, return `false` when no persistent/shared staged update remains, and remove `remap_object_payload_refs` plus all local-to-shared remapping code.

- [ ] **Step 4: Make promotion scans and completed lookup local-aware**

In `promote_persistent_references_before_commit_with_adapter`, skip
transaction-local `staged_objects` entries before calling
`object_table.is_persistent(object_id)`. Local staged payloads are promotion
sources, not shared persistent owners; reachable local sources are traversed from
roots by `promote_transaction_object_graph_in_attempt`.

In `persistent_object_id_for_live_bridge_after_completed_promotion`, resolve transaction handles with `self.known_object_id_for_transaction_ref_handle(object_table, gc_ref)`. If the result is transaction-local, require and return `self.promoted_objects[&object_id]`; otherwise retain the existing persistent/shared logic. This keeps root-delta construction valid both before and after finalization.

- [ ] **Step 5: Add a cycle-preservation test**

Create two local structs, stage references so `left -> right -> left`, publish `left` through a staged `tglobal`, promote, commit, and assert the two persistent payloads point to each other. Also assert `objects.live_count() == 2` so no volatile duplicates remain.

```rust
#[test]
fn transaction_local_object_promotion_preserves_cycles() {
    let mut objects = ObjectTable::default();
    let mut state = TransactionState::default();

    state.begin().unwrap();
    let left = state
        .allocate_transaction_local_struct(vec![ObjectValue::Ref(None)], None)
        .unwrap();
    let right = state
        .allocate_transaction_local_struct(vec![ObjectValue::Ref(Some(left))], None)
        .unwrap();
    state
        .stage_struct_field(&mut objects, left, 0, ObjectValue::Ref(Some(right)))
        .unwrap();
    let left_handle = state
        .transaction_ref_handle_for_object_id_avoiding(&mut objects, left, |_| false)
        .unwrap();
    state
        .stage_global_owned(None, 0, GlobalSnapshot::GcRef(left_handle))
        .unwrap();

    state
        .promote_persistent_references_before_commit(&mut objects)
        .unwrap();
    let promoted_left = state.promoted_object_for_test(left).unwrap();
    let promoted_right = state.promoted_object_for_test(right).unwrap();
    assert!(state.commit_object_payloads(&mut objects).unwrap());

    assert_eq!(objects.live_count(), 2);
    assert_eq!(
        objects.payload(promoted_left).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::Ref(Some(promoted_right))])
    );
    assert_eq!(
        objects.payload(promoted_right).unwrap(),
        ObjectPayload::Struct(vec![ObjectValue::Ref(Some(promoted_left))])
    );
    let root_delta = state
        .staged_persistent_root_delta_for_test(&objects)
        .unwrap();
    state
        .complete_commit_with_persistent_root_delta_for_test(root_delta)
        .unwrap();
}
```

- [ ] **Step 6: Run promotion and local-object tests and verify GREEN**

Run:

```bash
cargo test -p wasmtime --lib transaction_local_object --features transaction -- --nocapture
cargo test -p wasmtime --lib persistent_promotion_commit --features transaction -- --nocapture
```

Expected: all selected tests PASS, including alias and cycle preservation.

- [ ] **Step 7: Commit the reachability finalization change**

```bash
git add crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/transaction/promotion.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "fix: persist transaction objects by reachability"
```

### Task 3: Align Wasm Execution Tests With Escape-Root Semantics

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`
- Test: `crates/wasmtime/tests/transaction_persistence.rs`

- [ ] **Step 1: Re-run the ordinary-global escape test after the state fix**

Run:

```bash
cargo test -p wasmtime --lib ordinary_global_does_not_keep_transaction_object_alive --features transaction -- --nocapture
```

Expected: PASS. The creating call leaves no shared object, and the later read
rejects the expired unpromoted handle.

- [ ] **Step 2: Update constructor execution tests**

Keep all intra-transaction behavior assertions, but change post-commit expectations for unrooted constructors:

- `transaction_object_tstruct_executes_through_object_table`: expect zero shared objects and remove shared payload assertions.
- `transaction_object_tstruct_get_ref_roundtrips_into_object_operation`: expect zero shared objects.
- `transaction_object_tstruct_trap_frees_new_object_record`: expect zero shared objects after both trap and successful unrooted execution.
- Rename `transaction_object_constructors_return_transaction_ref_handles` to `transaction_object_constructors_use_local_handles_without_publishing`; retain its result assertion and expect empty shared object/handle maps afterward.
- `transaction_object_tarray_executes_through_object_table`: expect zero shared objects and remove shared payload assertions.
- `transaction_object_tarray_default_and_fixed_constructors_create_object_records`: retain returned-value assertions and expect zero shared objects.
- Leave `transaction_object_static_initializers_return_transaction_ref_handles` unchanged because static initialization is outside active transaction allocation.

- [ ] **Step 3: Convert cross-transaction retention tests to persistent roots**

In `transaction_object_existing_staged_write_rolls_back_on_tfail_and_trap`, replace the ordinary `$slot` global and `global.get/set` operations with `$slot` as `tglobal` and `tglobal.get/set`. Keep the rollback/trap assertions and additionally assert the retained object is persistent.

In `exported_tfunc_returning_tarray_ref_enters_transaction`, replace the ordinary `$slot` global with `tglobal` and use `tglobal.get/set`, because the later exported read intentionally requires the array to survive commit.

- [ ] **Step 4: Run the Wasm constructor and persistence suites**

Run:

```bash
cargo test -p wasmtime --lib transaction_object_ --features transaction -- --nocapture
cargo test -p wasmtime --test transaction_persistence --features transaction -- --nocapture
```

Expected: all selected tests PASS. Rooted graphs recover from persistent storage; unrooted constructor-only graphs leave no shared object records.

- [ ] **Step 5: Commit the Wasm behavior tests**

```bash
git add crates/wasmtime/src/runtime/transaction/tests.rs \
  crates/wasmtime/tests/transaction_persistence.rs
git commit -m "test: enforce transaction object reachability roots"
```

### Task 4: Align Documentation and Verify the Full Change

**Files:**
- Modify: `docs/superpowers/specs/2026-07-01-kotlin-txref-design.md`
- Modify: `docs/superpowers/plans/2026-07-01-kotlin-txref-boundary.md`
- Verify: all runtime files changed by Tasks 1-3

- [ ] **Step 1: Clarify promoted-handle behavior in the spec**

Replace the sentence saying all local handles are discarded with:

```text
After root publication, the runtime discards the transaction-local table and all
unpromoted handle mappings. If a local object was promoted, any handle associated
with that object may be rebound to the promoted persistent ObjectId so committed
tglobal, persistent-root, and persistent-table values remain immediately usable. The handle
survives because the object is persistently reachable, never because the handle
itself is a root.
```

- [ ] **Step 2: Correct the original TxRef implementation plan**

Change its architecture and conflict-resolution sections to say:

```text
Transactional constructors allocate into a transaction-local table. Commit
promotes only persistent-root-reachable graphs, rebinds promoted handles to
promoted persistent ObjectIds, and discards all other local slots and unpromoted
handle mappings. No local object is published into the shared volatile object
table merely because the transaction commits. Transaction handle allocation
exclusion is monotonic for an ObjectTable lifetime: unpromoted or expired
handles remain in reserved_transaction_ref_handles as tombstones until full
ObjectTable reset, while promoted handles leave that set only when installed in
the permanent handle mapping, which still prevents reuse. Neither case makes a
handle a persistence root. Preserve the universal runtime scope, the Kotlin-only
TxRef/TxCopyable boundary, the static initializer exception, and pre-LP
atomicity. Detailed execution is in
2026-07-15-transaction-local-reachability.md.
```

Remove references to local-to-shared object-id remapping and replace them with local-to-promoted handle rebinding.

- [ ] **Step 3: Run formatting and static checks**

Run:

```bash
cargo fmt --all --check
git diff --check
cargo check -p wasmtime --lib --features transaction
```

Expected: all commands exit zero. Existing unrelated transaction-feature warnings may remain, but no new warning should originate from the changed files.

- [ ] **Step 4: Run the complete focused verification set**

Run:

```bash
cargo test -p wasmtime --lib transaction_local_object --features transaction -- --nocapture
cargo test -p wasmtime --lib persistent_promotion_commit --features transaction -- --nocapture
cargo test -p wasmtime --lib transaction_object_ --features transaction -- --nocapture
cargo test -p wasmtime --test transaction_persistence --features transaction -- --nocapture
cargo test --test transaction_kotlin_toolset --features transaction -- --nocapture
```

Expected: every command exits zero with zero failed tests.

- [ ] **Step 5: Review the final diff against the invariants**

Run:

```bash
git diff --stat HEAD~3
git diff HEAD~3 -- crates/wasmtime/src/runtime/transaction/object_table.rs \
  crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/transaction/promotion.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs \
  crates/wasmtime/src/runtime/vm/libcalls.rs \
  docs/superpowers/specs/2026-07-01-kotlin-txref-design.md \
  docs/superpowers/plans/2026-07-01-kotlin-txref-boundary.md
```

Confirm there is no allocation of a non-persistent shared slot for a transaction-local object, no local handle counter reset, and no unrelated cleanup.

- [ ] **Step 6: Commit documentation corrections**

```bash
git add docs/superpowers/specs/2026-07-01-kotlin-txref-design.md \
  docs/superpowers/plans/2026-07-01-kotlin-txref-boundary.md
git commit -m "docs: make transaction persistence reachability-based"
```
