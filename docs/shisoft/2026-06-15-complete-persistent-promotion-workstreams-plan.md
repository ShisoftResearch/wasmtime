# Complete Persistent Promotion Workstreams Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete commit-time promotion so every value that can become reachable from persistent transactional roots is rewritten into durable `ObjectId`-based persistent object data before commit.

**Architecture:** Promotion remains a commit-time pass that runs before durable object/root publication. The pass must rewrite volatile transaction-mirrored objects, ordinary Wasmtime GC heap objects, i31 values, function references, and external references into persistent payloads whose identity is `ObjectId`. Fixed transaction log entries stay compact; promotion metadata is encoded in object data records and layout metadata, not by expanding `TxLogEntry`.

**Tech Stack:** Rust, Wasmtime runtime and GC internals, transactional `ObjectTable`, Zen-style file-backed durable log, Cranelift libcall lowering, WAT/WAST tests, Rust unit tests.

---

## Non-Negotiable Invariants

- `TxLogEntry` must remain at most 32 bytes so two entries fit in one 64-byte cache line.
- Do not add promotion-specific fields to `TxLogEntry`.
- Any extra promotion metadata belongs in object data records, object headers, type-layout records, or host-side encoder registries.
- Persistent payload references store `ObjectId`, not `VMGcRef`, raw `VMFuncRef`, raw `externref`, or process-local addresses.
- Ordinary Wasmtime GC identity is valid only inside the running process. It may be a promotion source but never a recovered persistent identity.
- A transaction that cannot encode a reachable value into a durable representation must fail before mutating live committed state.
- `ti31` is treated as a scalar persistent value encoded as `ObjectPayload::I31`, not as a Wasmtime heap object.
- `tfuncref` and `texternref` become persistent only through symbolic durable identities.

## File Map

- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`
  - Keep `TxLogEntry` sealed at half-cache-line size.
  - Add regression tests for size/alignment and encoded CRC stability.
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
  - Extend persistent object value/payload model.
  - Extend promotion maps and graph rewrite logic.
  - Add unit tests for source classification, graph promotion, and recovery.
- Create: `crates/wasmtime/src/runtime/transaction/promotion.rs`
  - Own promotion source classification and graph walking.
  - Keep source adapters separated from object table mutation.
- Create: `crates/wasmtime/src/runtime/transaction/durable_ref.rs`
  - Define durable encodings for function and external references.
  - Define the final `TRefRaw` bridge representation used by libcalls.
- Modify: `crates/wasmtime/src/runtime/transaction/object_heap.rs`
  - Encode/decode new durable payload forms in object data records.
- Modify: `crates/wasmtime/src/runtime/transaction/type_layout.rs`
  - Register and recover layouts needed to scan promoted ordinary GC structs and arrays.
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
  - Route transactional ref helper boundaries through `TRefRaw`/`ObjectId` instead of raw `u32` `VMGcRef`.
  - Wire host encoders for durable func/extern promotion.
- Modify: `crates/cranelift/src/translate/code_translator.rs`
  - Lower transactional ref operations to the final bridge ABI.
- Modify: `crates/cranelift/src/func_environ.rs`
  - Expose any additional libcall signatures required by the final ref ABI.
- Modify: `crates/test-util/src/wast.rs`
  - Keep WAST harness on real transactional syntax and file-backed recovery paths.
- Add tests under existing Wasmtime runtime unit tests and `crates/wasmtime/tests/transaction_persistence.rs`.
- Update docs:
  - `docs/shisoft/transactional-wasm-runtime-core-design.md`
  - `docs/shisoft/transactional-wasm-implementation-log.md`
  - `docs/shisoft/transactional-wasm-remaining-work-roadmap.md`

## Workstream 0: Guard the Fixed Durable Log Shape

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`

- [ ] **Step 1: Add a size regression test**

Add this test in the existing `#[cfg(test)]` module for durable log tests:

```rust
#[test]
fn tx_log_entry_fits_half_cache_line() {
    assert_eq!(core::mem::size_of::<TxLogEntry>(), 32);
    assert_eq!(core::mem::align_of::<TxLogEntry>(), 32);
}
```

- [ ] **Step 2: Run the test and verify the current shape is locked**

Run:

```bash
cargo test -p wasmtime --lib tx_log_entry_fits_half_cache_line -- --format terse
```

Expected: PASS.

- [ ] **Step 3: Add a comment next to `TxLogEntry`**

Add this comment immediately above `pub(crate) struct TxLogEntry`:

```rust
// Keep this entry exactly one half cache line. Promotion metadata must live in
// object data records or type-layout records, not in the fixed transaction log
// entry, so two entries can share one 64-byte cache line.
```

- [ ] **Step 4: Commit the guard**

Run:

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs
git commit -m "Guard transactional log entry size"
```

## Workstream 1: Durable Reference Value Model

**Files:**
- Create: `crates/wasmtime/src/runtime/transaction/durable_ref.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/object_heap.rs`

- [ ] **Step 1: Add failing tests for durable reference values**

Add tests in `crates/wasmtime/src/runtime/transaction.rs`:

```rust
#[test]
fn durable_func_identity_round_trips_through_object_payload() {
    let identity = DurableFuncIdentity {
        module_fingerprint: 0x10_20_30_40_50_60_70_80,
        function_index: 7,
        type_layout_id: TypeLayoutId::from_raw_for_test(3),
    };
    let payload = ObjectPayload::Func(identity);
    let encoded = encode_object_record_for_test(
        ObjectId { object_index: 9 },
        1,
        ObjectKind::Func as u16,
        default_type_layout_id_for_kind(ObjectKind::Func),
        &payload,
    )
    .unwrap();
    let decoded = TxObjectHeader::decode_record(&encoded).unwrap();
    assert_eq!(decoded.payload, payload);
}

#[test]
fn durable_extern_identity_round_trips_through_object_payload() {
    let identity = DurableExternIdentity {
        namespace: 4,
        handle: 0xabc,
        type_layout_id: TypeLayoutId::from_raw_for_test(8),
    };
    let payload = ObjectPayload::Extern(identity);
    let encoded = encode_object_record_for_test(
        ObjectId { object_index: 10 },
        1,
        ObjectKind::Extern as u16,
        default_type_layout_id_for_kind(ObjectKind::Extern),
        &payload,
    )
    .unwrap();
    let decoded = TxObjectHeader::decode_record(&encoded).unwrap();
    assert_eq!(decoded.payload, payload);
}
```

Expected initial result: compile failure because `DurableFuncIdentity`, `DurableExternIdentity`, and the new payload shapes do not exist.

- [ ] **Step 2: Create durable identity types**

Create `crates/wasmtime/src/runtime/transaction/durable_ref.rs`:

```rust
use super::type_layout::TypeLayoutId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DurableFuncIdentity {
    pub(crate) module_fingerprint: u64,
    pub(crate) function_index: u32,
    pub(crate) type_layout_id: TypeLayoutId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DurableExternIdentity {
    pub(crate) namespace: u32,
    pub(crate) handle: u64,
    pub(crate) type_layout_id: TypeLayoutId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DurableRefValue {
    Null,
    Object(super::ObjectId),
    I31(i32),
    Func(DurableFuncIdentity),
    Extern(DurableExternIdentity),
}
```

- [ ] **Step 3: Wire the module and payload enums**

In `crates/wasmtime/src/runtime/transaction.rs`, add:

```rust
#[path = "transaction/durable_ref.rs"]
mod durable_ref;
pub(crate) use durable_ref::{DurableExternIdentity, DurableFuncIdentity, DurableRefValue};
```

Change `ObjectPayload` variants:

```rust
pub(crate) enum ObjectPayload {
    Struct(Vec<ObjectValue>),
    Array(Vec<ObjectValue>),
    I31(i32),
    Extern(DurableExternIdentity),
    Func(DurableFuncIdentity),
}
```

Update `ObjectPayload::default_for_kind` to use canonical zero identities:

```rust
ObjectKind::Extern => ObjectPayload::Extern(DurableExternIdentity {
    namespace: 0,
    handle: 0,
    type_layout_id: default_type_layout_id_for_kind(ObjectKind::Extern),
}),
ObjectKind::Func => ObjectPayload::Func(DurableFuncIdentity {
    module_fingerprint: 0,
    function_index: 0,
    type_layout_id: default_type_layout_id_for_kind(ObjectKind::Func),
}),
```

- [ ] **Step 4: Encode/decode durable func and extern payloads**

In `object_heap.rs`, encode `ObjectPayload::Func` and `ObjectPayload::Extern` as fixed-width payload bodies:

```rust
// Func body: module_fingerprint:u64, function_index:u32, type_layout_id:u32.
// Extern body: namespace:u32, handle:u64, type_layout_id:u32.
```

Use little-endian fields and keep the object header kind as the discriminator.

- [ ] **Step 5: Run targeted tests**

Run:

```bash
cargo test -p wasmtime --lib durable_func_identity_round_trips_through_object_payload durable_extern_identity_round_trips_through_object_payload -- --format terse
```

Expected: PASS.

- [ ] **Step 6: Commit durable reference payload model**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/durable_ref.rs crates/wasmtime/src/runtime/transaction/object_heap.rs
git commit -m "Add durable transactional reference payloads"
```

## Workstream 2: Ordinary Wasmtime GC Promotion Adapter

**Files:**
- Create: `crates/wasmtime/src/runtime/transaction/promotion.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Inspect during implementation: `crates/wasmtime/src/runtime/vm/gc/gc_ref.rs`, `crates/wasmtime/src/runtime/vm/gc/enabled/drc.rs`, `crates/wasmtime/src/runtime/vm/gc/enabled/copying.rs`, `crates/wasmtime/src/runtime/vm/gc/enabled/structref.rs`, `crates/wasmtime/src/runtime/vm/gc/enabled/arrayref.rs`

- [ ] **Step 1: Add adapter-facing unit tests with a fake GC source**

Add tests in `transaction.rs` that build a fake promotion adapter with:

```rust
enum FakeGcObject {
    Struct(Vec<DurableRefValue>),
    Array(Vec<DurableRefValue>),
}
```

The test should create a volatile source graph `root -> child`, promote `root`, and assert:

```rust
assert_ne!(promoted_root, promoted_child);
assert_eq!(
    tx.staged_object_payload_for_test(promoted_root),
    Some(&ObjectPayload::Struct(vec![ObjectValue::Ref(Some(promoted_child))]))
);
assert_eq!(
    tx.staged_object_payload_for_test(promoted_child),
    Some(&ObjectPayload::Struct(vec![ObjectValue::I32(42)]))
);
```

Expected initial result: compile failure because the promotion adapter does not exist.

- [ ] **Step 2: Define the promotion adapter interface**

Create `promotion.rs`:

```rust
use super::{
    DurableExternIdentity, DurableFuncIdentity, DurableRefValue, ObjectId, ObjectPayload,
    ObjectValue, TypeLayoutId,
};
use crate::prelude::*;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct VolatileGcSourceId(pub(crate) u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GcPromotionSnapshot {
    Struct {
        type_layout_id: TypeLayoutId,
        fields: Vec<DurableRefValue>,
    },
    Array {
        type_layout_id: TypeLayoutId,
        elements: Vec<DurableRefValue>,
    },
    I31(i32),
    Func(DurableFuncIdentity),
    Extern(DurableExternIdentity),
}

pub(crate) trait GcPromotionAdapter {
    fn snapshot(&mut self, source: VolatileGcSourceId) -> Result<GcPromotionSnapshot>;
}
```

- [ ] **Step 3: Add a no-adapter failure path**

In `TransactionState`, route unknown raw `VMGcRef` values through a helper:

```rust
fn promote_unknown_volatile_ref_without_adapter(&self, raw_ref: u64) -> Result<ObjectId> {
    bail!(
        "volatile GC reference {raw_ref:#x} cannot be promoted without a Wasmtime GC promotion adapter"
    )
}
```

This preserves the current fail-before-commit behavior until the real adapter is wired.

- [ ] **Step 4: Implement adapter-backed graph promotion**

Add a method:

```rust
fn promote_gc_source_with_adapter<A: GcPromotionAdapter>(
    &mut self,
    object_table: &mut ObjectTable,
    adapter: &mut A,
    source: VolatileGcSourceId,
) -> Result<ObjectId>
```

The method must:

1. Check a `BTreeMap<VolatileGcSourceId, ObjectId>` cache to preserve sharing and break cycles.
2. Reserve a new persistent `ObjectId` before recursively rewriting children.
3. Convert `DurableRefValue::Object(id)` to `ObjectValue::Ref(Some(id))`.
4. Convert `DurableRefValue::I31(value)` by allocating or reusing an `ObjectPayload::I31(value)` object only when an object identity is required by the field representation.
5. Convert `DurableRefValue::Func(identity)` and `DurableRefValue::Extern(identity)` through Workstreams 4 and 5 helpers.
6. Stage the completed object payload with the source layout id.

- [ ] **Step 5: Add the real Wasmtime adapter safety note as code comments**

In `promotion.rs`, add a short comment above the Wasmtime adapter implementation boundary:

```rust
// The real Wasmtime adapter must snapshot through GcHeap APIs and layout
// metadata, never by persisting VMGcRef raw words. VMGcRef remains a volatile
// heap index and can change after restart or GC movement.
```

- [ ] **Step 6: Run adapter unit tests**

Run:

```bash
cargo test -p wasmtime --lib promotion_adapter -- --format terse
```

Expected: PASS for fake-adapter graph promotion and PASS for no-adapter rejection.

- [ ] **Step 7: Commit adapter foundation**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/promotion.rs
git commit -m "Add GC promotion adapter foundation"
```

## Workstream 3: Promote Ordinary Struct and Array GC Objects

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/promotion.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`

- [ ] **Step 1: Add failing tests for ordinary GC struct/array promotion**

Add tests that use the fake adapter from Workstream 2:

```rust
#[test]
fn ordinary_gc_struct_promotes_to_persistent_object_id() {
    let mut objects = ObjectTable::new();
    let mut tx = TransactionState::new_for_test(TransactionId(1));
    let mut adapter = FakePromotionAdapter::new()
        .with_struct(0x100, vec![DurableRefValue::I31(11)]);
    let promoted = tx
        .promote_gc_source_with_adapter_for_test(
            &mut objects,
            &mut adapter,
            VolatileGcSourceId(0x100),
        )
        .unwrap();
    assert!(objects.is_persistent(promoted).unwrap());
    assert_eq!(
        tx.staged_object_payload_for_test(promoted),
        Some(&ObjectPayload::Struct(vec![ObjectValue::Ref(Some(
            tx.promoted_i31_value_for_test(11).unwrap()
        ))]))
    );
}

#[test]
fn ordinary_gc_array_preserves_shared_child_identity() {
    let mut objects = ObjectTable::new();
    let mut tx = TransactionState::new_for_test(TransactionId(1));
    let mut adapter = FakePromotionAdapter::new()
        .with_struct(0x200, vec![DurableRefValue::I31(5)])
        .with_array(
            0x201,
            vec![
                DurableRefValue::Object(ObjectId { object_index: 200 }),
                DurableRefValue::Object(ObjectId { object_index: 200 }),
            ],
        );
    let promoted = tx
        .promote_gc_source_with_adapter_for_test(
            &mut objects,
            &mut adapter,
            VolatileGcSourceId(0x201),
        )
        .unwrap();
    let ObjectPayload::Array(elements) = tx.staged_object_payload_for_test(promoted).unwrap() else {
        panic!("expected array payload");
    };
    assert_eq!(elements[0], elements[1]);
}
```

Expected initial result: compile failure until the fake adapter and helper names are implemented.

- [ ] **Step 2: Wire adapter-backed promotion into commit**

In `transaction_commit_impl` in `crates/wasmtime/src/runtime/vm/libcalls.rs`, keep the order:

```rust
state.promote_persistent_references_before_commit(object_table)?;
state.validate_persistent_references_before_commit(object_table)?;
```

Extend `promote_persistent_references_before_commit` to call the adapter-aware path once Wasmtime store context can provide the adapter. Until then, keep a deterministic error for unknown ordinary GC refs.

- [ ] **Step 3: Add layout registration for promoted snapshots**

When promoting `GcPromotionSnapshot::Struct` or `GcPromotionSnapshot::Array`, register or validate the `type_layout_id` in `TypeLayoutRegistry` before staging the payload. If metadata is missing, return:

```text
transactional persistent object promotion requires registered type layout
```

- [ ] **Step 4: Run graph promotion tests**

Run:

```bash
cargo test -p wasmtime --lib ordinary_gc_struct_promotes ordinary_gc_array_preserves -- --format terse
```

Expected: PASS.

- [ ] **Step 5: Commit ordinary GC promotion**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/promotion.rs crates/wasmtime/src/runtime/vm/libcalls.rs
git commit -m "Promote ordinary GC object snapshots"
```

## Workstream 4: Durable `tfuncref` Promotion

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/durable_ref.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`

- [ ] **Step 1: Add tests for function identity stability**

Add tests:

```rust
#[test]
fn durable_func_identity_does_not_store_raw_pointer() {
    let identity = DurableFuncIdentity::new_for_test(0x1234, 9, TypeLayoutId::from_raw_for_test(2));
    assert_ne!(identity.module_fingerprint, 0);
    assert_eq!(identity.function_index, 9);
    assert_eq!(identity.type_layout_id, TypeLayoutId::from_raw_for_test(2));
}

#[test]
fn durable_func_identity_reuses_existing_object() {
    let mut objects = ObjectTable::new();
    let identity = DurableFuncIdentity::new_for_test(0x1234, 9, TypeLayoutId::from_raw_for_test(2));
    let first = objects.object_id_for_durable_func(identity).unwrap();
    let second = objects.object_id_for_durable_func(identity).unwrap();
    assert_eq!(first, second);
    assert_eq!(objects.payload(first).unwrap(), ObjectPayload::Func(identity));
}
```

Expected initial result: compile failure because `new_for_test` and `object_id_for_durable_func` do not exist.

- [ ] **Step 2: Implement function identity construction**

Add to `DurableFuncIdentity`:

```rust
impl DurableFuncIdentity {
    #[cfg(test)]
    pub(crate) fn new_for_test(
        module_fingerprint: u64,
        function_index: u32,
        type_layout_id: TypeLayoutId,
    ) -> Self {
        Self {
            module_fingerprint,
            function_index,
            type_layout_id,
        }
    }
}
```

Production construction uses module fingerprint plus function index. Export names may be recorded for diagnostics, but the durable identity must not depend on raw pointers.

- [ ] **Step 3: Replace raw `Func(u64)` allocation helpers**

Replace `ObjectTable::object_id_for_raw_ref_or_func` function-ref allocation with:

```rust
pub(crate) fn object_id_for_durable_func(
    &mut self,
    identity: DurableFuncIdentity,
) -> Result<ObjectId>
```

Use a map keyed by `DurableFuncIdentity`, not `u64`.

- [ ] **Step 4: Wire libcall conversion**

At libcall boundaries that currently pass raw `FuncRef(usize)` or raw `func_ref_to_object`, convert to `DurableFuncIdentity` before object-table lookup. If the runtime cannot compute module fingerprint and function index, fail with:

```text
transactional function reference promotion requires durable function identity
```

- [ ] **Step 5: Run function promotion tests**

Run:

```bash
cargo test -p wasmtime --lib durable_func_identity -- --format terse
```

Expected: PASS.

- [ ] **Step 6: Commit durable function references**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/durable_ref.rs crates/wasmtime/src/runtime/vm/libcalls.rs
git commit -m "Promote function refs with durable identity"
```

## Workstream 5: Durable `texternref` Promotion

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/durable_ref.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`

- [ ] **Step 1: Add host encoder tests**

Add tests:

```rust
#[test]
fn externref_promotion_requires_encoder() {
    let mut objects = ObjectTable::new();
    let err = objects
        .object_id_for_durable_extern_without_encoder_for_test(0xfeed)
        .unwrap_err()
        .to_string();
    assert!(err.contains("transactional external reference promotion requires a durable extern encoder"));
}

#[test]
fn externref_encoder_reuses_object_id_for_same_identity() {
    let mut objects = ObjectTable::new();
    let identity = DurableExternIdentity {
        namespace: 1,
        handle: 0xfeed,
        type_layout_id: TypeLayoutId::from_raw_for_test(4),
    };
    let first = objects.object_id_for_durable_extern(identity).unwrap();
    let second = objects.object_id_for_durable_extern(identity).unwrap();
    assert_eq!(first, second);
    assert_eq!(objects.payload(first).unwrap(), ObjectPayload::Extern(identity));
}
```

Expected initial result: compile failure until helper APIs exist.

- [ ] **Step 2: Define host encoder trait**

Add in `durable_ref.rs`:

```rust
pub(crate) trait PersistentExternRefEncoder {
    fn encode_externref(&mut self, raw_externref: u64) -> Result<DurableExternIdentity>;
}
```

This trait takes the current scaffold raw word only as a lookup key. The returned `DurableExternIdentity` is the only value allowed into persistent object payloads.

- [ ] **Step 3: Implement object-table lookup by durable extern identity**

Add:

```rust
pub(crate) fn object_id_for_durable_extern(
    &mut self,
    identity: DurableExternIdentity,
) -> Result<ObjectId>
```

Use a map keyed by `DurableExternIdentity` and allocate `ObjectPayload::Extern(identity)` on first use.

- [ ] **Step 4: Wire commit-time failure without an encoder**

Any `texternref` promotion path without an installed encoder returns:

```text
transactional external reference promotion requires a durable extern encoder
```

- [ ] **Step 5: Run external reference tests**

Run:

```bash
cargo test -p wasmtime --lib externref_encoder -- --format terse
```

Expected: PASS.

- [ ] **Step 6: Commit durable external references**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/durable_ref.rs crates/wasmtime/src/runtime/vm/libcalls.rs
git commit -m "Promote externrefs with host durable identity"
```

## Workstream 6: Final `ObjectId`-Carrying Transactional Ref ABI

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/durable_ref.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/cranelift/src/translate/code_translator.rs`
- Modify: `crates/cranelift/src/func_environ.rs`

- [ ] **Step 1: Add ABI encoding tests**

Add tests:

```rust
#[test]
fn tref_raw_encodes_persistent_object_id() {
    let raw = TRefRaw::from_object_id(Some(ObjectId { object_index: 41 })).unwrap();
    assert_eq!(raw.decode_object_id().unwrap(), Some(ObjectId { object_index: 41 }));
}

#[test]
fn tref_raw_rejects_committing_volatile_ref_word() {
    let raw = TRefRaw::from_volatile_gc_ref_for_test(0x1234);
    let err = raw.decode_committable_ref().unwrap_err().to_string();
    assert!(err.contains("volatile transactional ref must be promoted before commit"));
}
```

Expected initial result: compile failure because `TRefRaw` does not exist.

- [ ] **Step 2: Define the bridge representation**

Add to `durable_ref.rs`:

```rust
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TRefRaw(u64);

impl TRefRaw {
    const TAG_NULL: u64 = 0;
    const TAG_OBJECT: u64 = 1;
    const TAG_VOLATILE_GC: u64 = 2;
    const TAG_I31: u64 = 3;
    const TAG_FUNC: u64 = 4;
    const TAG_EXTERN: u64 = 5;
    const TAG_SHIFT: u64 = 60;
    const PAYLOAD_MASK: u64 = (1u64 << 60) - 1;

    pub(crate) fn from_object_id(object_id: Option<ObjectId>) -> Result<Self> {
        Ok(match object_id {
            Some(object_id) => {
                ensure!(
                    object_id.object_index <= Self::PAYLOAD_MASK,
                    "ObjectId does not fit TRefRaw payload"
                );
                Self((Self::TAG_OBJECT << Self::TAG_SHIFT) | object_id.object_index)
            }
            None => Self(Self::TAG_NULL << Self::TAG_SHIFT),
        })
    }
}
```

- [ ] **Step 3: Replace raw `u32` GC-ref paths in transactional helpers**

Update libcall helper boundaries so transactional ref values use `u64` `TRefRaw` ABI. Keep localized compatibility wrappers only where tests still construct old raw values.

- [ ] **Step 4: Update Cranelift lowering**

In transactional ref lowering, make ref-producing helpers return `i64`/`u64` `TRefRaw` and ref-consuming helpers accept that representation. Preserve Wasmtime normal GC refs outside transactional helpers.

- [ ] **Step 5: Run transactional ref ABI tests**

Run:

```bash
cargo test -p wasmtime --lib tref_raw -- --format terse
cargo test -p cranelift-wasm --lib transaction -- --format terse
```

Expected: PASS.

- [ ] **Step 6: Commit final transactional ref ABI**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/durable_ref.rs crates/wasmtime/src/runtime/vm/libcalls.rs crates/cranelift/src/translate/code_translator.rs crates/cranelift/src/func_environ.rs
git commit -m "Route transactional refs through ObjectId ABI"
```

## Workstream 7: Recovery and Runtime Rehydration

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/object_heap.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/type_layout.rs`
- Modify: `crates/wasmtime/tests/transaction_persistence.rs`

- [ ] **Step 1: Add recovery tests for promoted ref kinds**

Add file-backed tests that:

1. Commit a struct containing a promoted i31, promoted function identity, promoted external identity, and promoted child object.
2. Drop the engine/store.
3. Reopen the file-backed region.
4. Rebuild object table from durable logs.
5. Assert that all recovered payloads use `ObjectId` or durable identity values only.

Expected initial result: failure until object record decoding and recovery index rebuild understand all durable payloads.

- [ ] **Step 2: Ensure recovery rebuilds durable identity indexes**

During object-table recovery, rebuild:

```rust
durable_func_to_object: BTreeMap<DurableFuncIdentity, ObjectId>
durable_extern_to_object: BTreeMap<DurableExternIdentity, ObjectId>
```

Do not rebuild `VMGcRef -> ObjectId` as a persistent fact. That map is runtime-only and may be populated lazily when a recovered object is exposed back to Wasmtime.

- [ ] **Step 3: Validate recovered type layouts before tracing**

Before persistent GC mark or recovered root exposure, verify every struct/array `type_layout_id` has a registered layout. Missing layout metadata returns:

```text
recovered persistent object requires registered type layout
```

- [ ] **Step 4: Run recovery tests**

Run:

```bash
cargo test -p wasmtime --test transaction_persistence promoted_references_recover -- --format terse
```

Expected: PASS.

- [ ] **Step 5: Commit recovery rehydration**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_heap.rs crates/wasmtime/src/runtime/transaction/type_layout.rs crates/wasmtime/tests/transaction_persistence.rs
git commit -m "Recover promoted persistent reference identities"
```

## Workstream 8: End-to-End Wasm and WAST Coverage

**Files:**
- Modify: `crates/test-util/src/wast.rs`
- Modify: `crates/wasmtime/tests/transaction_persistence.rs`
- Add focused WAT fixtures only if the existing fixture layout requires separate files.

- [ ] **Step 1: Add real Wasm tests for mixed promotion**

Add an end-to-end test that runs a Wasm module which:

1. Creates transactional struct/array values.
2. Stores them through a persistent transactional root.
3. Stores i31, function ref, and external ref values reachable from that root.
4. Commits.
5. Reopens file-backed storage.
6. Reads the recovered root through real Wasmtime calls.

- [ ] **Step 2: Add abort tests**

Add a test where promotion reserves object ids, then a later conflict aborts the transaction. Assert:

```rust
assert!(objects.payload(promoted_object).is_err());
assert!(
    recovered_objects
        .payload(promoted_object)
        .is_err(),
    "aborted promoted object must not be recoverable"
);
```

- [ ] **Step 3: Add corruption/boundary tests**

Add tests for:

- Unknown ordinary GC ref without adapter.
- Externref without encoder.
- Missing recovered type layout.
- `TxLogEntry` size remains 32 bytes after all promotion changes.

- [ ] **Step 4: Run the full transactional test matrix**

Run:

```bash
cargo fmt --check
git diff --check
cargo test -p wasmtime --lib transaction -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
cargo test -p wasmtime-wast --test all -- transaction --format terse
```

Expected: PASS.

- [ ] **Step 5: Commit E2E promotion tests**

Run:

```bash
git add crates/test-util/src/wast.rs crates/wasmtime/tests/transaction_persistence.rs
git commit -m "Test complete persistent reference promotion"
```

## Workstream 9: Remove Temporary Promotion Scaffolding

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
- Modify: `docs/shisoft/transactional-wasm-remaining-work-roadmap.md`

- [ ] **Step 1: Search for stale mock markers**

Run:

```bash
rg -n "SHISOFT-TWASM-MOCK|VOLATILE_GC_REF_PROMOTION_UNIMPLEMENTED|raw VMGcRef|raw `VMGcRef`|Func\\(u64\\)|Extern\\(u64\\)" crates/wasmtime/src/runtime docs/shisoft
```

Expected: the search identifies only comments that are still accurate or code that will be removed in this task.

- [ ] **Step 2: Remove committed-payload raw ref scaffolding**

Remove or narrow:

- `gc_ref_to_object` as a persistent recovery concept.
- `func_ref_to_object` keyed by raw pointer.
- comments saying staged/persistent refs are raw Wasmtime reference words.

Keep runtime-only bridge maps if needed for live Wasmtime exposure, but rename them to include `volatile` in the field name.

- [ ] **Step 3: Update docs**

Document:

- Promotion is complete at commit boundaries.
- Persistent identity is `ObjectId`.
- `VMGcRef` is only a volatile source or live bridge.
- Function refs use module fingerprint plus function index.
- Extern refs require a host durable encoder.
- `TxLogEntry` remains exactly 32 bytes.

- [ ] **Step 4: Run final verification**

Run:

```bash
cargo fmt --check
git diff --check
cargo test -p wasmtime --lib transaction -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
```

Expected: PASS.

- [ ] **Step 5: Commit cleanup and docs**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/libcalls.rs docs/shisoft/transactional-wasm-runtime-core-design.md docs/shisoft/transactional-wasm-implementation-log.md docs/shisoft/transactional-wasm-remaining-work-roadmap.md
git commit -m "Document complete persistent promotion model"
```

## Subagent Work Allocation

- Subagent A: Workstream 0 and Workstream 1. Keep `TxLogEntry` fixed and add durable payload types.
- Subagent B: Workstream 2 and Workstream 3. Build promotion adapter foundation and ordinary GC snapshot promotion.
- Subagent C: Workstream 4 and Workstream 5. Implement durable `tfuncref` and `texternref` identities.
- Subagent D: Workstream 6. Migrate transactional ref ABI to `TRefRaw` and update Cranelift lowering.
- Subagent E: Workstream 7 and Workstream 8. Recovery and end-to-end file-backed Wasm/WAST tests.
- Main agent: Review every subagent patch before commit, run the full verification matrix, and update docs in Workstream 9.

## Success Criteria

- `TxLogEntry` is exactly 32 bytes and aligned to 32 bytes.
- Committed persistent object payloads never contain raw `VMGcRef`, raw `VMFuncRef`, raw `externref`, or process-local addresses.
- Transaction-mirrored objects, ordinary Wasmtime GC structs/arrays, i31, function refs, and external refs either promote to durable identities or fail before commit.
- Recovered file-backed storage rebuilds object payloads, durable identity indexes, roots, and type layouts needed by persistent GC.
- Mixed transactions containing tmemory undo records and promoted object redo records recover correctly.
- The full transactional unit and file-backed persistence suites pass.
