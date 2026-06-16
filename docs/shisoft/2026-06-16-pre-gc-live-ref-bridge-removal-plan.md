# Pre-GC Live Ref Bridge Removal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove live `VMGcRef`/`VMFuncRef` identity from persistent object paths while preserving explicit live transaction ABI compatibility seams.

**Architecture:** Persistent object records already encode durable `ObjectValue`s, with `ObjectId` graph edges, inline `ti31`, and durable func/extern identities. This plan hardens the boundaries so persistent publication, recovery, tracing, roots, and object payloads never consult live Wasmtime refs. Live raw refs remain only at libcall/lowering boundaries and commit-time promotion inputs until the future `TRefType` ABI split.

**Tech Stack:** Rust, Wasmtime runtime/libcalls, Cranelift transaction lowering, file-backed transaction persistence tests, WAST transaction test harness.

---

## File Map

- `crates/wasmtime/src/runtime/transaction.rs`
  - Owns `ObjectId`, `ObjectValue`, `ObjectValueAbi`, `ObjectTable`, persistent root selection, object publication, recovery rebuild, and most tests.
  - This is where persistent-vs-live helper names and assertions should be tightened.
- `crates/wasmtime/src/runtime/transaction/promotion.rs`
  - Owns commit-time promotion from ordinary Wasmtime GC values into persistent `ObjectId` records.
  - This remains the allowed `VMGcRef` input boundary.
- `crates/wasmtime/src/runtime/transaction/durable_ref.rs`
  - Owns durable function/external identity registry and explicitly tagged WAST live fallbacks.
  - This should keep live fallback paths tagged as compatibility, not persistent recovery support.
- `crates/wasmtime/src/runtime/transaction/object_heap.rs`
  - Owns durable object record encoding, decoding, and reference tracing.
  - This should remain durable-only: `ObjectValue::Ref(ObjectId)`, durable func/extern leaves, inline `ti31`.
- `crates/wasmtime/src/runtime/vm/libcalls.rs`
  - Owns live helper ABI conversion for transaction object instructions.
  - This is where raw `VMGcRef`/`VMFuncRef` values can still enter as live operands, but persistent conversion must produce durable values before publication.
- `crates/cranelift/src/translate/code_translator.rs`
  - Owns current transaction lowering comments and helper signatures.
  - Keep unchanged unless a verification failure identifies a lowering comment
    or helper route that still describes live refs as persistent identity.
- `docs/shisoft/2026-06-16-pre-gc-object-model-freeze-design.md`
  - Source design for this plan.
- `docs/shisoft/transactional-wasm-implementation-log.md`
  - Add a short status note after implementation.

## Non-Goals

- Do not implement `TRefType` yet.
- Do not remove ordinary Wasmtime `VMGcRef` or `VMFuncRef` use outside transaction persistent paths.
- Do not remove live raw-ref ABI compatibility at libcall/lowering boundaries if WAST still depends on it.
- Do not implement mixed object-data block compaction, line-level Immix reuse, clean-abort undo retirement, or `ObjectId` reuse.
- Do not modify WAST test cases.

## Task 1: Add Persistent ABI Guard Tests

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`

- [ ] **Step 1: Add tests showing durable object ABI does not require side maps**

Add tests near the existing `ObjectValueAbi`/object-table tests:

```rust
#[test]
fn persistent_object_value_abi_ref_uses_object_id_not_gc_ref_side_map() -> Result<()> {
    let object_id = ObjectId { object_index: 41 };
    let abi = ObjectValueAbi::from_object_value(&ObjectValue::Ref(Some(object_id)))?;

    assert_eq!(
        abi.as_parts(),
        (
            OBJECT_VALUE_ABI_TAG_REF,
            PersistentObjectRefRaw::from_optional_object_id(Some(object_id))?.as_raw(),
            0
        )
    );
    assert_eq!(abi.to_object_value()?, ObjectValue::Ref(Some(object_id)));
    Ok(())
}

#[test]
fn persistent_object_value_abi_func_and_extern_use_durable_identities() -> Result<()> {
    let func = DurableFuncIdentity {
        module_fingerprint: 0x1234,
        function_index: 7,
        type_layout_id: TypeLayoutId::BUILTIN_FUNC,
    };
    let extern_ = DurableExternIdentity {
        namespace: 9,
        handle: 0xabc,
        type_layout_id: TypeLayoutId::BUILTIN_EXTERN,
    };

    assert_eq!(
        ObjectValueAbi::from_object_value(&ObjectValue::FuncRef(func))?.to_object_value()?,
        ObjectValue::FuncRef(func)
    );
    assert_eq!(
        ObjectValueAbi::from_object_value(&ObjectValue::ExternRef(extern_))?.to_object_value()?,
        ObjectValue::ExternRef(extern_)
    );
    Ok(())
}
```

- [ ] **Step 2: Run the new tests and confirm the baseline**

Run:

```bash
CARGO_TARGET_DIR=/tmp/wasmtime-pre-gc-freeze-target \
  CARGO_INCREMENTAL=0 \
  cargo test -p wasmtime persistent_object_value_abi_ --lib
```

Expected: pass.

## Task 2: Split Durable Ref ABI From Live Helper ABI

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`

- [ ] **Step 1: Add explicit durable helper names**

In `ObjectTable`, keep the existing behavior but introduce names that make the boundary explicit:

```rust
pub(crate) fn persistent_ref_abi_for_object_id(&self, object_id: ObjectId) -> Result<(u64, u64)> {
    self.live_slot(object_id)?;
    ensure!(
        self.is_persistent(object_id)?,
        "persistent ref ABI requires a persistent object: {object_id:?}"
    );
    Ok((
        PersistentObjectRefRaw::from_optional_object_id(Some(object_id))?.as_raw(),
        OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT,
    ))
}

pub(crate) fn live_bridge_ref_abi_for_object_id(&self, object_id: ObjectId) -> Result<(u64, u64)> {
    self.live_slot(object_id)?;
    if let Some(gc_ref) = self.object_to_gc_ref.get(&object_id).copied() {
        return Ok((u64::from(gc_ref), OBJECT_VALUE_ABI_LIVE_REF_KIND_GC));
    }
    self.persistent_ref_abi_for_object_id(object_id)
}
```

Remove `live_ref_abi_for_object_id` after its call sites are updated. The final
code should have only the explicit durable and live-bridge helper names.

- [ ] **Step 2: Force persistent object-value conversion to use the durable helper**

In `crates/wasmtime/src/runtime/vm/libcalls.rs`, split ABI conversion into two
functions:

```rust
fn durable_transaction_abi_from_object_value(
    value: &ObjectValue,
) -> Result<ObjectValueAbi> {
    ObjectValueAbi::from_object_value(value)
}
```

and:

```rust
fn live_transaction_abi_from_object_value(
    durable_refs: &DurableReferenceRegistry,
    object_table: &ObjectTable,
    value: &ObjectValue,
) -> Result<ObjectValueAbi> {
    if let ObjectValue::I31(value) = value {
        return ObjectValueAbi::from_live_parts(
            OBJECT_VALUE_ABI_TAG_REF,
            ObjectTable::encode_raw_i31_ref(*value),
            OBJECT_VALUE_ABI_LIVE_REF_KIND_I31,
        );
    }
    let (raw, kind) = match value {
        ObjectValue::Ref(Some(object_id)) => {
            object_table.live_bridge_ref_abi_for_object_id(*object_id)?
        }
        ObjectValue::Ref(None) => (0, OBJECT_VALUE_ABI_LIVE_REF_KIND_UNTYPED),
        ObjectValue::FuncRef(identity) => {
            let vm_func_ref_addr = durable_refs.resolve_func_identity(*identity).with_context(|| {
                format!(
                    "durable function identity is not registered in this store: module={:#x} function={} layout={}",
                    identity.module_fingerprint,
                    identity.function_index,
                    identity.type_layout_id.get()
                )
            })?;
            (
                u64::try_from(vm_func_ref_addr)
                    .context("durable function reference address does not fit u64")?,
                OBJECT_VALUE_ABI_LIVE_REF_KIND_FUNC,
            )
        }
        ObjectValue::ExternRef(identity) => (
            u64::from(durable_refs.resolve_extern_identity(*identity).with_context(|| {
                format!(
                    "durable external identity is not registered in this store: namespace={} handle={:#x} layout={}",
                    identity.namespace,
                    identity.handle,
                    identity.type_layout_id.get()
                )
            })?),
            OBJECT_VALUE_ABI_LIVE_REF_KIND_EXTERN,
        ),
        _ => return ObjectValueAbi::from_object_value(value),
    };
    ObjectValueAbi::from_live_parts(OBJECT_VALUE_ABI_TAG_REF, raw, kind)
}
```

Use `durable_transaction_abi_from_object_value` for object-field persistence.
Use `live_transaction_abi_from_object_value` only for returning values to Wasm
helper boundaries.

- [ ] **Step 3: Add a regression test for associated GC refs**

Add a test in `transaction.rs`:

```rust
#[test]
fn persistent_ref_abi_ignores_associated_live_gc_ref() -> Result<()> {
    let mut objects = ObjectTable::default();
    let object_id = objects.allocate_persistent_struct_for_gc_ref(
        0x710,
        vec![ObjectValue::I32(1)],
    )?;

    assert_eq!(objects.known_object_id_for_gc_ref(0x710), Some(object_id));
    assert_eq!(
        objects.persistent_ref_abi_for_object_id(object_id)?,
        (
            PersistentObjectRefRaw::from_optional_object_id(Some(object_id))?.as_raw(),
            OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT,
        )
    );
    assert_eq!(
        objects.live_bridge_ref_abi_for_object_id(object_id)?,
        (0x710, OBJECT_VALUE_ABI_LIVE_REF_KIND_GC)
    );
    Ok(())
}
```

- [ ] **Step 4: Run focused tests**

Run:

```bash
CARGO_TARGET_DIR=/tmp/wasmtime-pre-gc-freeze-target \
  CARGO_INCREMENTAL=0 \
  cargo test -p wasmtime persistent_ref_abi_ --lib
```

Expected: pass.

## Task 3: Rename Side Maps As Live-Bridge State

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: tests in the same file that directly inspect side maps.

- [ ] **Step 1: Rename fields without changing behavior**

Rename:

```rust
gc_ref_to_object
object_to_gc_ref
```

to:

```rust
live_gc_ref_to_object
object_to_live_gc_ref
```

Update all call sites in `ObjectTable` and tests.

- [ ] **Step 2: Rename helper methods to mark live-bridge use**

Rename helpers:

```rust
associate_gc_ref -> associate_live_gc_ref_for_transaction_bridge
object_id_for_gc_ref -> object_id_for_live_gc_ref_bridge
known_object_id_for_gc_ref -> known_object_id_for_live_gc_ref_bridge
known_persistent_object_id_for_gc_ref -> known_persistent_object_id_for_live_gc_ref_bridge
gc_ref_for_object_id -> live_gc_ref_for_object_id_bridge
```

Do not keep compatibility wrappers for the old helper names. Every remaining
helper name must say `live_bridge` when it consults the side maps. Add this
comment above the side-map fields:

```rust
// SHISOFT-TWASM-MOCK: live transaction ref bridge; persistent object records
// must use ObjectId and must not call this helper.
```

- [ ] **Step 3: Update direct tests**

Replace direct assertions such as:

```rust
assert!(recovered.rebuilt.gc_ref_to_object.is_empty());
assert!(recovered.rebuilt.object_to_gc_ref.is_empty());
```

with:

```rust
assert!(recovered.rebuilt.live_gc_ref_to_object.is_empty());
assert!(recovered.rebuilt.object_to_live_gc_ref.is_empty());
```

- [ ] **Step 4: Run transaction crate tests that exercise object table mappings**

Run:

```bash
CARGO_TARGET_DIR=/tmp/wasmtime-pre-gc-freeze-target \
  CARGO_INCREMENTAL=0 \
  cargo test -p wasmtime transaction_object --lib
```

Expected: pass.

## Task 4: Harden Persistent Publication And Recovery Against Live Bridges

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/object_heap.rs`

- [ ] **Step 1: Add publication assertion test**

Add a test that builds a persistent object with an associated live GC ref and another object referring to it, publishes the referring object, clears live maps, and verifies the encoded payload still traces by `ObjectId`:

```rust
#[test]
fn persistent_publication_traces_object_id_after_live_bridge_maps_are_cleared() -> Result<()> {
    let mut objects = ObjectTable::default();
    let child = objects.allocate_persistent_struct_for_gc_ref(
        0x801,
        vec![ObjectValue::I32(10)],
    )?;
    let parent = objects.allocate_persistent_struct_for_gc_ref(
        0x802,
        vec![ObjectValue::Ref(Some(child))],
    )?;

    let publication = objects.pending_publication_for_test(parent)?;
    objects.live_gc_ref_to_object.clear();
    objects.object_to_live_gc_ref.clear();

    let refs = objects.trace_object_ids(parent)?;
    assert_eq!(refs, vec![child]);
    let (domain, object_id) = crate::runtime::vm::unpack_object_granule_id(publication.logical_id)?;
    assert_eq!(domain, crate::runtime::vm::PackedGranuleDomain::TStruct);
    assert_eq!(object_id, parent.object_index);
    Ok(())
}
```

- [ ] **Step 2: Ensure `object_pending_publication` validates durable refs only**

Keep this invariant:

```rust
self.validate_persistent_payload_refs(self.heap.payload(handle)?)?;
```

Extend `validate_persistent_payload_refs` so it also rejects any future `ObjectValue` variant that could carry live refs. The current accepted leaf set is:

```rust
ObjectValue::I31(_)
| ObjectValue::I32(_)
| ObjectValue::I64(_)
| ObjectValue::F32(_)
| ObjectValue::F64(_)
| ObjectValue::V128(_)
| ObjectValue::Ref(None)
| ObjectValue::FuncRef(_)
| ObjectValue::ExternRef(_)
```

For `ObjectValue::Ref(Some(object_id))`, require `slot.persistent == true`.

- [ ] **Step 3: Run publication/recovery tests**

Run:

```bash
CARGO_TARGET_DIR=/tmp/wasmtime-pre-gc-freeze-target \
  CARGO_INCREMENTAL=0 \
  cargo test -p wasmtime persistent_publication_traces_object_id_after_live_bridge_maps_are_cleared --lib
```

Expected: pass.

## Task 5: Keep Promotion As The Only VMGcRef-To-ObjectId Persistent Boundary

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/promotion.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`

- [ ] **Step 1: Add comments at promotion entry points**

Above `persistent_object_id_for_gc_ref_after_promotion_with_adapter` and `persistent_object_id_for_gc_ref_after_completed_promotion`, add:

```rust
// This is the allowed bridge from an ordinary live Wasmtime GC object into the
// persistent object graph. It must copy the source into object storage and
// return an ObjectId. Persistent records after this point must not depend on
// the original VMGcRef.
```

- [ ] **Step 2: Replace persistent-path calls to live object lookup**

Search:

```bash
rg -n "known_object_id_for_gc_ref|object_id_for_gc_ref|gc_ref_for_object_id|live_ref_abi_for_object_id" \
  crates/wasmtime/src/runtime/transaction.rs \
  crates/wasmtime/src/runtime/transaction \
  crates/wasmtime/src/runtime/vm/libcalls.rs
```

For each hit:

- If it is promotion code, keep it and rename to the live-bridge helper.
- If it is live helper return conversion, use the live-bridge helper.
- If it is persistent publication, recovery, tracing, durable roots, or object payload encoding, replace it with `ObjectId`/`PersistentObjectRefRaw` logic.

- [ ] **Step 3: Add a negative test for volatile GC refs in durable publication**

Add a test:

```rust
#[test]
fn persistent_payload_rejects_unpromoted_volatile_ref() -> Result<()> {
    let mut objects = ObjectTable::default();
    let volatile = objects.allocate_struct_for_gc_ref(0x901, vec![ObjectValue::I32(1)])?;
    let persistent = objects.allocate_persistent_struct_for_gc_ref(
        0x902,
        vec![ObjectValue::Ref(Some(volatile))],
    )?;

    let err = objects.pending_publication_for_test(persistent).unwrap_err();
    assert!(err.to_string().contains(VOLATILE_GC_REF_PROMOTION_UNIMPLEMENTED));
    Ok(())
}
```

- [ ] **Step 4: Run promotion and durable publication tests**

Run:

```bash
CARGO_TARGET_DIR=/tmp/wasmtime-pre-gc-freeze-target \
  CARGO_INCREMENTAL=0 \
  cargo test -p wasmtime promotion --lib
```

Then:

```bash
CARGO_TARGET_DIR=/tmp/wasmtime-pre-gc-freeze-target \
  CARGO_INCREMENTAL=0 \
  cargo test -p wasmtime persistent_payload_rejects_unpromoted_volatile_ref --lib
```

Expected: pass.

## Task 6: Keep Durable Func/Extern Leaves Out Of Object Table

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/durable_ref.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`

- [ ] **Step 1: Add tests that func/extern refs are not object-table granules**

Add tests:

```rust
#[test]
fn durable_func_and_extern_values_are_not_object_payloads() {
    assert!(ObjectPayload::default_for_kind(ObjectKind::Func).is_err());
    assert!(ObjectPayload::default_for_kind(ObjectKind::Extern).is_err());
    assert!(ObjectPayload::default_for_kind(ObjectKind::I31).is_err());
}

#[test]
fn object_value_abi_func_extern_does_not_need_live_registry() -> Result<()> {
    let func = DurableFuncIdentity {
        module_fingerprint: 0xfeed,
        function_index: 3,
        type_layout_id: TypeLayoutId::BUILTIN_FUNC,
    };
    let extern_ = DurableExternIdentity {
        namespace: 4,
        handle: 0xbeef,
        type_layout_id: TypeLayoutId::BUILTIN_EXTERN,
    };

    assert_eq!(
        ObjectValueAbi::from_object_value(&ObjectValue::FuncRef(func))?.to_object_value()?,
        ObjectValue::FuncRef(func)
    );
    assert_eq!(
        ObjectValueAbi::from_object_value(&ObjectValue::ExternRef(extern_))?.to_object_value()?,
        ObjectValue::ExternRef(extern_)
    );
    Ok(())
}
```

- [ ] **Step 2: Ensure live fallback functions stay explicitly tagged**

In `durable_ref.rs`, keep both live fallback comments containing `SHISOFT-TWASM-MOCK`. Do not remove the opt-in guard:

```rust
ensure!(
    self.allow_live_wast_reference_fallbacks,
    "ordinary GC promotion cannot encode function reference without registered durable function identity"
);
```

- [ ] **Step 3: Run durable ref tests**

Run:

```bash
CARGO_TARGET_DIR=/tmp/wasmtime-pre-gc-freeze-target \
  CARGO_INCREMENTAL=0 \
  cargo test -p wasmtime durable_ref --lib
```

Expected: pass.

## Task 7: Update Documentation And Mock Boundary Inventory

**Files:**
- Modify: `docs/shisoft/2026-06-16-pre-gc-object-model-freeze-design.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
- Modify: `docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md`

- [ ] **Step 1: Update freeze design status**

Append a short "Implementation Status" section:

```markdown
## Implementation Status

- Persistent object records encode graph edges as `ObjectId`.
- Live Wasmtime `VMGcRef` mappings are named as transaction live-bridge state.
- Persistent publication and recovery do not require live GC/function pointer
  identity.
- Remaining live fallbacks are explicit WAST compatibility seams or future
  `TRefType` ABI work.
```

- [ ] **Step 2: Update the implementation log**

Add an entry:

```markdown
### 2026-06-16: Pre-GC Object Model Freeze

- Split durable persistent object reference encoding from live transaction
  helper ABI conversion.
- Renamed live `VMGcRef` side maps as bridge-only state.
- Kept promotion as the only allowed path from live Wasmtime GC objects into
  persistent `ObjectId` records.
- Confirmed existing coarse block/chunk reclamation remains unchanged:
  whole-dead object chunks and committed linear-undo chunks can be retired and
  reused with block generations.
```

- [ ] **Step 3: Update roadmap checkboxes**

In the stabilization roadmap, mark the pre-GC freeze and live bridge cleanup
items complete. Leave unchecked:

- full `TRefType` ABI split;
- mixed object-data compaction;
- clean-abort undo reclamation;
- line-level Immix reuse;
- `ObjectId` reuse.

## Task 8: Verification Gate

**Files:**
- No code edits unless failures require fixes.

- [ ] **Step 1: Run formatting**

Run:

```bash
cargo fmt --check
```

Expected: pass.

- [ ] **Step 2: Run focused transaction tests**

Run:

```bash
CARGO_TARGET_DIR=/tmp/wasmtime-pre-gc-freeze-target \
  CARGO_INCREMENTAL=0 \
  cargo test -p wasmtime transaction --lib
```

Expected: pass.

- [ ] **Step 3: Run WAST transaction tests**

Run:

```bash
CARGO_TARGET_DIR=/tmp/wasmtime-pre-gc-freeze-target \
  CARGO_INCREMENTAL=0 \
  WASMTIME_TEST_TRANSACTION_WAST=1 \
  CARGO_BUILD_JOBS=2 \
  python3 ./ci/run-tests.py --locked --exclude=wasi-preview1-component-adapter -- --format terse
```

Expected: pass with zero ignored WAST cases.

- [ ] **Step 4: Run adapter build with transaction wasm-tools first in PATH**

Run:

```bash
PATH=/home/shisoft/Code/Research/wasm-tools-transaction/target/debug:$PATH \
  ./ci/build-wasi-preview1-component-adapter.sh
```

Expected: pass.

- [ ] **Step 5: Check mock inventory**

Run:

```bash
rg -n "SHISOFT-TWASM-MOCK|VMGcRef|VMFuncRef|gc_ref_to_object|object_to_gc_ref|live_gc_ref_to_object|object_to_live_gc_ref" \
  crates/wasmtime/src/runtime/transaction.rs \
  crates/wasmtime/src/runtime/transaction \
  crates/wasmtime/src/runtime/vm/libcalls.rs \
  crates/cranelift/src/translate/code_translator.rs
```

Expected:

- no `gc_ref_to_object` or `object_to_gc_ref` names remain;
- remaining `VMGcRef`/`VMFuncRef` hits are ordinary Wasmtime live ABI,
  promotion source, or explicitly tagged compatibility fallback;
- no persistent publication/recovery/tracing path uses live ref identity.

- [ ] **Step 6: Commit**

Run:

```bash
git status --short
git add \
  crates/wasmtime/src/runtime/transaction.rs \
  crates/wasmtime/src/runtime/transaction/promotion.rs \
  crates/wasmtime/src/runtime/transaction/durable_ref.rs \
  crates/wasmtime/src/runtime/transaction/object_heap.rs \
  crates/wasmtime/src/runtime/vm/libcalls.rs \
  crates/cranelift/src/translate/code_translator.rs \
  docs/shisoft/2026-06-16-pre-gc-object-model-freeze-design.md \
  docs/shisoft/transactional-wasm-implementation-log.md \
  docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md
git commit -m "Freeze persistent object reference boundaries before GC"
```

Do not add any `Co-authored-by` trailer.

## Self-Review

- Spec coverage: The plan covers live bridge removal from persistent paths,
  explicit promotion-only `VMGcRef` ingress, durable func/extern leaves,
  recovery invariants, and mock inventory review. Existing coarse block/chunk
  reclamation is acknowledged but not reimplemented.
- Placeholder scan: No `TBD` or unconstrained "handle later" instructions are
  used. Future work is listed as non-goals with explicit names.
- Type consistency: The plan uses existing names from the codebase:
  `ObjectId`, `ObjectValue`, `ObjectValueAbi`, `PersistentObjectRefRaw`,
  `DurableFuncIdentity`, `DurableExternIdentity`, `TypeLayoutId`,
  `ObjectTable`, and `VOLATILE_GC_REF_PROMOTION_UNIMPLEMENTED`.
