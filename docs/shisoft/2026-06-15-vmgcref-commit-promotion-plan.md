# VMGcRef Commit Promotion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Promote transaction-mirrored volatile `VMGcRef` object graphs and `ti31` scalar references into persistent object records with new `ObjectId` identities during transaction commit.

**Architecture:** Promotion runs after staged tmemory/table/global updates are flushed and before persistent object/root publications are computed. The first wave promotes transaction-mirrored objects that already have `ObjectTable` payloads from `tstruct`/`tarray` runtime helpers, and rewrites immediate `ti31` refs into scalar `ObjectPayload::I31` records; arbitrary ordinary Wasmtime GC heap objects remain rejected until a later GC-heap introspection adapter lands. The promotion pass reserves new persistent `ObjectId`s, rewrites `ObjectValue::Ref` edges to promoted or already-persistent targets, stages promoted payload records, then lets the existing durable object publication and LP path make the graph recoverable.

**Tech Stack:** Rust, Wasmtime runtime internals, transactional `ObjectTable`, `TransactionState`, file-backed transaction logs, WAT integration tests, `cargo test`.

**Status:** Ready for implementation; no tasks in this plan have been executed yet.

---

## Scope

This plan implements commit-time promotion for the objects that the
transactional object runtime already mirrors in `ObjectTable`:

- `tstruct.new`, `tstruct.new_default`, and `tstruct.set` objects
- `tarray.new`, `tarray.new_default`, `tarray.new_fixed`, data/elem-created
  transactional arrays, and `tarray.set`/bulk updates already represented in
  `ObjectTable`
- `ti31` references when they cross into persistent transactional object data
  or persistent roots; these are scalar/value records, not heap-object copies
- object graphs reachable from staged persistent roots or staged persistent
  object payloads

This plan does not introspect arbitrary ordinary Wasmtime GC heap objects that
were not mirrored into `ObjectTable`. Those refs must keep failing with:

```text
volatile GC reference promotion into persistent object graph is not implemented yet
```

This plan also does not add durable block/chunk reuse or `ObjectId` reuse.
It does not promote `tfuncref`, `texternref`, or `exnref` payloads yet. Those
must use symbolic durable identities, such as namespace plus function id/name
for `tfuncref` and host-provided durable handles for `texternref`, rather than
persisting raw runtime pointers.

## File Map

- Modify: `crates/wasmtime/src/runtime/transaction.rs`
  - Publish and recover `ObjectKind::I31` as a durable object granule.
  - Add persistent reservation/fill helpers to `ObjectTable`.
  - Add active-transaction promotion maps to `TransactionWorkspace`.
  - Add recursive graph promotion and payload ref rewriting.
  - Decode raw i31 immediates into `ObjectPayload::I31` before the function-ref
    fallback in the transactional object ABI.
  - Replace the promotion-boundary-only root/payload conversion with
    promotion-aware conversion.
  - Add focused unit tests.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`
  - Add the `TI31` packed object granule domain.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
  - Accept `TI31` object-data records when the object header kind is `I31`.
- Modify: `crates/cranelift/src/func_environ.rs`
  - Allow i31 reference fields/elements in transactional object ABI lowering.
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
  - Run promotion before `commit_object_payloads_into`, root-delta creation,
    persistent GC delta creation, and durable object publication.
- Modify: `crates/wasmtime/tests/transaction_persistence.rs`
  - Add a file-backed WAT test proving a `tstruct.new` root is promoted,
    committed, recovered, and rebuilt as a persistent object.
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
  - Record that the first promotion wave covers transaction-mirrored objects.
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
  - Record implementation status and verification.

## Task 1: Durable TI31 Object Domain And ABI

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/cranelift/src/func_environ.rs`
- Test: `cargo test -p wasmtime --lib ti31_persistent_object_domain -- --format terse`
- Test: `cargo test -p wasmtime --lib transaction_i31_reference_fields -- --format terse`

- [ ] **Step 1: Write failing durable-domain tests**

In `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`, add:

```rust
#[test]
fn pack_ti31_object_granule_id_round_trips() {
    let logical_id = pack_object_granule_id(PackedGranuleDomain::TI31, 31).unwrap();
    assert_eq!(logical_id >> 60, PackedGranuleDomain::TI31 as u64);

    let (domain, object_id) = unpack_object_granule_id(logical_id).unwrap();
    assert_eq!(domain, PackedGranuleDomain::TI31);
    assert_eq!(object_id, 31);
}
```

In `crates/wasmtime/src/runtime/transaction.rs`, add a nested test module:

```rust
mod ti31_persistent_object_domain {
    use super::*;

    #[test]
    fn i31_pending_publication_uses_ti31_domain() {
        let mut objects = ObjectTable::default();
        let object = objects.allocate_payload(ObjectPayload::I31(-7)).unwrap();

        let pub_ = objects.object_pending_publication(object).unwrap();
        let (domain, object_id) =
            crate::runtime::vm::unpack_object_granule_id(pub_.logical_id).unwrap();

        assert_eq!(domain, crate::runtime::vm::PackedGranuleDomain::TI31);
        assert_eq!(object_id, object.object_index);
        assert_eq!(pub_.kind, ObjectKind::I31 as u16);
    }
}
```

- [ ] **Step 2: Verify red**

Run:

```bash
cargo test -p wasmtime --lib ti31_persistent_object_domain -- --format terse
```

Expected: compile failure because `PackedGranuleDomain::TI31` is not defined
and `ObjectKind::I31` is not publishable as an object granule.

- [ ] **Step 3: Add the durable `TI31` object domain**

In `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`, extend the
packed domain enum and helpers:

```rust
pub(crate) enum PackedGranuleDomain {
    TMemory = 1,
    TMemorySize = 2,
    TGlobal = 3,
    TTable = 4,
    TTableSize = 5,
    TStruct = 6,
    TArray = 7,
    TI31 = 8,
}
```

Update `packed_granule_domain`:

```rust
8 => PackedGranuleDomain::TI31,
```

Update both `pack_object_granule_id` and `unpack_object_granule_id` object
domain checks:

```rust
matches!(
    domain,
    PackedGranuleDomain::TStruct | PackedGranuleDomain::TArray | PackedGranuleDomain::TI31
)
```

In `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`, update
`object_domain_matches_object_kind`:

```rust
PackedGranuleDomain::TI31 => object_kind == ObjectKind::I31 as u16,
```

In `crates/wasmtime/src/runtime/transaction.rs`, update
`object_pending_publication`:

```rust
let domain = match object_kind_from_u16(header.kind)? {
    ObjectKind::Struct => crate::runtime::vm::PackedGranuleDomain::TStruct,
    ObjectKind::Array => crate::runtime::vm::PackedGranuleDomain::TArray,
    ObjectKind::I31 => crate::runtime::vm::PackedGranuleDomain::TI31,
    other => bail!("object kind {other:?} is not a persistent object granule"),
};
```

Keep `ObjectKind::Extern` and `ObjectKind::Func` rejected here.

- [ ] **Step 4: Add i31 raw-ref ABI conversion tests**

Replace the current test named
`module_compilation_rejects_transaction_i31_reference_fields_for_now` in
`crates/wasmtime/src/runtime/transaction.rs` with:

```rust
#[test]
fn transaction_i31_reference_fields_round_trip_as_scalars() {
    let mut config = crate::Config::new();
    config.wasm_gc(true);
    let engine = crate::Engine::new(&config).unwrap();
    let module = transaction_test_module(
        &engine,
        r#"
        (module
          (type $s (tstruct (field (mut (ref null i31)))))
          (tfunc (export "x") (result i32)
            (local $s (tref $s))
            (local.set $s (tstruct.new $s (tref.ti31 (i32.const 13))))
            (i31.get_s (tstruct.get $s 0 (tref.cast_read (local.get $s))))))
        "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let x = instance.get_typed_func::<(), i32>(&mut store, "x").unwrap();

    assert_eq!(x.call(&mut store, ()).unwrap(), 13);
}
```

- [ ] **Step 5: Verify red**

Run:

```bash
cargo test -p wasmtime --lib transaction_i31_reference_fields -- --format terse
```

Expected: compile or translation failure with the current
`transactional i31 reference ObjectId values are not implemented yet`
diagnostic.

- [ ] **Step 6: Allow i31 references in lowering and decode them before func refs**

In `crates/cranelift/src/func_environ.rs`, update
`transaction_object_ref_type_supported`:

```rust
fn transaction_object_ref_type_supported(ref_ty: WasmRefType) -> bool {
    ref_ty.heap_type == WasmHeapType::I31
        || ref_ty.is_vmgcref_type_and_not_i31()
        || matches!(
            ref_ty.heap_type.top(),
            WasmHeapTopType::Func
        )
}
```

In the ABI-to-Wasm decode path, handle `i31` before switching on
`ref_ty.heap_type.top()`:

```rust
if ref_ty.heap_type == WasmHeapType::I31 {
    builder.ins().ireduce(I32, low)
} else {
    match ref_ty.heap_type.top() {
        WasmHeapTopType::Func if self.pointer_type() == I64 => low,
        WasmHeapTopType::Func => builder.ins().ireduce(I32, low),
        WasmHeapTopType::Any | WasmHeapTopType::Extern | WasmHeapTopType::Exn => {
            builder.ins().ireduce(I32, low)
        }
        WasmHeapTopType::Cont => unreachable!(),
    }
}
```

In `crates/wasmtime/src/runtime/transaction.rs`, add i31 interning maps to
`ObjectTable`:

```rust
i31_ref_to_object: BTreeMap<u32, ObjectId>,
object_to_i31_ref: BTreeMap<ObjectId, u32>,
```

Initialize both maps in `ObjectTable::default`.

Add helpers to `ObjectTable`:

```rust
fn is_raw_i31_ref(raw_ref: u64) -> bool {
    raw_ref <= u64::from(u32::MAX) && (raw_ref & 1) == 1
}

fn decode_raw_i31_ref(raw_ref: u64) -> Result<i32> {
    ensure!(Self::is_raw_i31_ref(raw_ref), "raw ref is not an i31 immediate");
    Ok((raw_ref as u32 as i32) >> 1)
}

fn encode_raw_i31_ref(value: i32) -> u64 {
    u64::from(((value as u32) << 1) | 1)
}

fn object_id_for_raw_i31_ref(&mut self, raw_ref: u64) -> Result<ObjectId> {
    let raw_ref = u32::try_from(raw_ref).context("raw i31 ref does not fit u32")?;
    if let Some(object_id) = self.i31_ref_to_object.get(&raw_ref).copied()
        && self.live_slot(object_id).is_ok()
    {
        return Ok(object_id);
    }
    self.i31_ref_to_object.remove(&raw_ref);

    let value = Self::decode_raw_i31_ref(u64::from(raw_ref))?;
    let object_id = self.allocate_payload(ObjectPayload::I31(value))?;
    self.i31_ref_to_object.insert(raw_ref, object_id);
    self.object_to_i31_ref.insert(object_id, raw_ref);
    Ok(object_id)
}
```

Update `raw_ref_for_object_id` so `ObjectKind::I31` returns the encoded i31
immediate instead of requiring a GC-ref association:

```rust
if let Some(raw_ref) = self.object_to_i31_ref.get(&object_id).copied() {
    return Ok(u64::from(raw_ref));
}
if let ObjectPayload::I31(value) = self.payload(object_id)? {
    return Ok(Self::encode_raw_i31_ref(value));
}
```

Update `free` so it removes i31 interning entries:

```rust
if let Some(raw_ref) = self.object_to_i31_ref.remove(&object_id) {
    if self.i31_ref_to_object.get(&raw_ref) == Some(&object_id) {
        self.i31_ref_to_object.remove(&raw_ref);
    }
}
```

Update `object_id_for_raw_ref_or_func` so i31 immediates are handled before
the GC-ref lookup and function-ref fallback:

```rust
if Self::is_raw_i31_ref(raw_ref) {
    return self.object_id_for_raw_i31_ref(raw_ref);
}
```

- [ ] **Step 7: Verify green**

Run:

```bash
cargo test -p wasmtime --lib ti31_persistent_object_domain -- --format terse
cargo test -p wasmtime --lib transaction_i31_reference_fields -- --format terse
```

Expected: durable-domain tests and i31 reference field round-trip tests pass.

- [ ] **Step 8: Commit**

Run:

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/libcalls.rs crates/cranelift/src/func_environ.rs
git commit -m "Add durable ti31 object records"
```

## Task 2: Persistent Reservation And Fill Primitives

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_promotion_reservation -- --format terse`

- [ ] **Step 1: Write failing tests**

Add a nested module under `#[cfg(test)] mod tests`:

```rust
mod persistent_promotion_reservation {
    use super::*;

    #[test]
    fn promotion_reserves_new_persistent_object_id_without_reusing_source() {
        let mut objects = ObjectTable::default();
        let source = objects
            .allocate_struct_for_gc_ref(0x710, vec![ObjectValue::I32(7)])
            .unwrap();

        let promoted = objects
            .reserve_persistent_object_id_for_promotion(
                ObjectKind::Struct,
                TypeLayoutId::DEFAULT_STRUCT,
            )
            .unwrap();

        assert_ne!(promoted, source);
        assert!(!objects.is_persistent(source).unwrap());
        assert!(objects.is_persistent(promoted).unwrap());
        assert_eq!(
            objects.payload(promoted).unwrap(),
            ObjectPayload::Struct(Vec::new())
        );
        assert_eq!(objects.known_object_id_for_gc_ref(0x710), Some(source));
    }

    #[test]
    fn promotion_fill_preserves_payload_and_type_layout() {
        let mut objects = ObjectTable::default();
        let promoted = objects
            .reserve_persistent_object_id_for_promotion(
                ObjectKind::Array,
                TypeLayoutId::DEFAULT_ARRAY,
            )
            .unwrap();

        objects
            .fill_reserved_persistent_object_for_promotion(
                promoted,
                ObjectPayload::Array(vec![ObjectValue::I32(1), ObjectValue::I32(2)]),
            )
            .unwrap();

        assert_eq!(
            objects.payload(promoted).unwrap(),
            ObjectPayload::Array(vec![ObjectValue::I32(1), ObjectValue::I32(2)])
        );
        assert_eq!(
            objects.live_slot(promoted).unwrap().type_layout_id,
            TypeLayoutId::DEFAULT_ARRAY.get()
        );
    }
}
```

- [ ] **Step 2: Verify red**

Run:

```bash
cargo test -p wasmtime --lib persistent_promotion_reservation -- --format terse
```

Expected: compile failure because
`reserve_persistent_object_id_for_promotion` and
`fill_reserved_persistent_object_for_promotion` do not exist.

- [ ] **Step 3: Add reservation helpers**

Add these methods to `impl ObjectTable`:

```rust
fn reserve_persistent_object_id_for_promotion(
    &mut self,
    kind: ObjectKind,
    type_layout_id: TypeLayoutId,
) -> Result<ObjectId> {
    self.allocate_payload_with_type_layout_id(
        ObjectPayload::default_for_kind(kind),
        type_layout_id,
        true,
    )
}

fn fill_reserved_persistent_object_for_promotion(
    &mut self,
    object_id: ObjectId,
    payload: ObjectPayload,
) -> Result<()> {
    ensure!(
        self.is_persistent(object_id)?,
        "promotion target object must be persistent"
    );
    ensure!(
        self.kind(object_id)? == payload.kind(),
        "promotion target object kind does not match promoted payload"
    );
    self.validate_persistent_payload_refs(&payload)?;
    self.update_payload(object_id, payload)
}
```

Do not associate the promoted target with the source `VMGcRef`; the raw
`VMGcRef` remains a volatile runtime wrapper. Persistent identity is the new
`ObjectId`.

- [ ] **Step 4: Verify green**

Run:

```bash
cargo test -p wasmtime --lib persistent_promotion_reservation -- --format terse
```

Expected: both tests pass.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs
git commit -m "Add persistent promotion reservation slots"
```

## Task 3: Transaction-Local Promotion Map

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_promotion_graph -- --format terse`

- [ ] **Step 1: Write failing tests**

Add tests in a nested module named `persistent_promotion_graph`:

```rust
mod persistent_promotion_graph {
    use super::*;

    #[test]
    fn promotion_maps_volatile_struct_to_new_persistent_object_id() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let source = objects
            .allocate_struct_for_gc_ref(0x720, vec![ObjectValue::I32(7)])
            .unwrap();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(720));

        let promoted = state
            .promote_transaction_object_graph_for_test(&mut objects, source)
            .unwrap();

        assert_ne!(promoted, source);
        assert!(!objects.is_persistent(source).unwrap());
        assert!(objects.is_persistent(promoted).unwrap());
        assert_eq!(
            state.promoted_object_for_test(source),
            Some(promoted)
        );
        assert_eq!(
            state.staged_object_payload_for_test(promoted).unwrap(),
            &ObjectPayload::Struct(vec![ObjectValue::I32(7)])
        );
    }

    #[test]
    fn promotion_rewrites_child_refs_to_promoted_targets() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let child = objects
            .allocate_struct_for_gc_ref(0x721, vec![ObjectValue::I32(1)])
            .unwrap();
        let root = objects
            .allocate_struct_for_gc_ref(0x722, vec![ObjectValue::Ref(Some(child))])
            .unwrap();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(721));

        let promoted_root = state
            .promote_transaction_object_graph_for_test(&mut objects, root)
            .unwrap();
        let promoted_child = state.promoted_object_for_test(child).unwrap();

        assert_ne!(promoted_root, root);
        assert_ne!(promoted_child, child);
        assert_eq!(
            state.staged_object_payload_for_test(promoted_root).unwrap(),
            &ObjectPayload::Struct(vec![ObjectValue::Ref(Some(promoted_child))])
        );
        assert_eq!(
            state.staged_object_payload_for_test(promoted_child).unwrap(),
            &ObjectPayload::Struct(vec![ObjectValue::I32(1)])
        );
    }

    #[test]
    fn promotion_preserves_cycles_with_reserved_object_ids() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let left = objects
            .allocate_struct_for_gc_ref(0x723, vec![ObjectValue::Ref(None)])
            .unwrap();
        let right = objects
            .allocate_struct_for_gc_ref(0x724, vec![ObjectValue::Ref(Some(left))])
            .unwrap();
        objects
            .update_payload(left, ObjectPayload::Struct(vec![ObjectValue::Ref(Some(right))]))
            .unwrap();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(723));

        let promoted_left = state
            .promote_transaction_object_graph_for_test(&mut objects, left)
            .unwrap();
        let promoted_right = state.promoted_object_for_test(right).unwrap();

        assert_eq!(
            state.staged_object_payload_for_test(promoted_left).unwrap(),
            &ObjectPayload::Struct(vec![ObjectValue::Ref(Some(promoted_right))])
        );
        assert_eq!(
            state.staged_object_payload_for_test(promoted_right).unwrap(),
            &ObjectPayload::Struct(vec![ObjectValue::Ref(Some(promoted_left))])
        );
    }

    #[test]
    fn promotion_promotes_i31_scalar_to_persistent_object_id() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let source = objects.allocate_payload(ObjectPayload::I31(-17)).unwrap();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(724));

        let promoted = state
            .promote_transaction_object_graph_for_test(&mut objects, source)
            .unwrap();

        assert_ne!(promoted, source);
        assert!(objects.is_persistent(promoted).unwrap());
        assert_eq!(
            state.staged_object_payload_for_test(promoted).unwrap(),
            &ObjectPayload::I31(-17)
        );
    }
}
```

- [ ] **Step 2: Verify red**

Run:

```bash
cargo test -p wasmtime --lib persistent_promotion_graph -- --format terse
```

Expected: compile failure for missing promotion-map helpers.

- [ ] **Step 3: Add promotion state to the active workspace**

Add to `TransactionWorkspace` and the active state fields:

```rust
promoted_objects: BTreeMap<ObjectId, ObjectId>,
promoted_i31_refs: BTreeMap<u32, ObjectId>,
```

Update `take_workspace` so the map suspends with the active transaction:

```rust
fn take_workspace(&mut self) -> TransactionWorkspace {
    TransactionWorkspace {
        staged_globals: mem::take(&mut self.staged_globals),
        staged_granules: mem::take(&mut self.staged_granules),
        staged_memory_sizes: mem::take(&mut self.staged_memory_sizes),
        staged_table_sizes: mem::take(&mut self.staged_table_sizes),
        staged_table_elements: mem::take(&mut self.staged_table_elements),
        original_table_elements: mem::take(&mut self.original_table_elements),
        staged_objects: mem::take(&mut self.staged_objects),
        promoted_objects: mem::take(&mut self.promoted_objects),
        promoted_i31_refs: mem::take(&mut self.promoted_i31_refs),
        allocated_objects: mem::take(&mut self.allocated_objects),
        read_granules: mem::take(&mut self.read_granules),
        write_granules: mem::take(&mut self.write_granules),
        scratch: mem::take(&mut self.scratch),
        pending_memory_store: self.pending_memory_store.take(),
    }
}
```

Update `install_workspace` so resume restores the map and
`clear_active` clears it through the existing default workspace install:

```rust
fn install_workspace(&mut self, workspace: TransactionWorkspace) {
    self.staged_globals = workspace.staged_globals;
    self.staged_granules = workspace.staged_granules;
    self.staged_memory_sizes = workspace.staged_memory_sizes;
    self.staged_table_sizes = workspace.staged_table_sizes;
    self.staged_table_elements = workspace.staged_table_elements;
    self.original_table_elements = workspace.original_table_elements;
    self.staged_objects = workspace.staged_objects;
    self.promoted_objects = workspace.promoted_objects;
    self.promoted_i31_refs = workspace.promoted_i31_refs;
    self.allocated_objects = workspace.allocated_objects;
    self.read_granules = workspace.read_granules;
    self.write_granules = workspace.write_granules;
    self.scratch = workspace.scratch;
    self.pending_memory_store = workspace.pending_memory_store;
}
```

`TransactionWorkspace` derives `Default`, so adding the field is enough for
commit, abort, and fail cleanup because all three paths call `clear_active` or
`abort_allocated_objects`, which reaches `clear_active`.

- [ ] **Step 4: Add graph promotion helpers**

Add these methods to `impl TransactionState`:

```rust
fn promote_transaction_object_graph(
    &mut self,
    object_table: &mut ObjectTable,
    source: ObjectId,
) -> Result<ObjectId> {
    self.ensure_active()?;
    if object_table.is_persistent(source)? {
        return Ok(source);
    }
    if let Some(promoted) = self.promoted_objects.get(&source).copied() {
        return Ok(promoted);
    }

    let kind = object_table.kind(source)?;
    ensure!(
        matches!(kind, ObjectKind::Struct | ObjectKind::Array | ObjectKind::I31),
        "transactional promotion currently supports struct, array, and i31 object payloads"
    );
    let type_layout_id = TypeLayoutId::new(object_table.live_slot(source)?.type_layout_id)
        .context("promoted object source layout id cannot be zero")?;
    let promoted = object_table.reserve_persistent_object_id_for_promotion(kind, type_layout_id)?;
    self.record_allocated_object(promoted)?;
    self.promoted_objects.insert(source, promoted);

    let source_payload = self
        .staged_objects
        .get(&source)
        .cloned()
        .unwrap_or(object_table.payload(source)?);
    let promoted_payload = self.rewrite_payload_refs_for_promotion(object_table, source_payload)?;
    object_table.validate_persistent_payload_refs(&promoted_payload)?;
    self.staged_objects.insert(promoted, promoted_payload);
    Ok(promoted)
}

fn rewrite_payload_refs_for_promotion(
    &mut self,
    object_table: &mut ObjectTable,
    payload: ObjectPayload,
) -> Result<ObjectPayload> {
    match payload {
        ObjectPayload::Struct(fields) => fields
            .into_iter()
            .map(|value| self.rewrite_value_ref_for_promotion(object_table, value))
            .collect::<Result<Vec<_>>>()
            .map(ObjectPayload::Struct),
        ObjectPayload::Array(elements) => elements
            .into_iter()
            .map(|value| self.rewrite_value_ref_for_promotion(object_table, value))
            .collect::<Result<Vec<_>>>()
            .map(ObjectPayload::Array),
        ObjectPayload::I31(_) => Ok(payload),
        ObjectPayload::Extern(_) | ObjectPayload::Func(_) => {
            bail!("transactional promotion requires symbolic durable identity for function and external references")
        }
    }
}

fn rewrite_value_ref_for_promotion(
    &mut self,
    object_table: &mut ObjectTable,
    value: ObjectValue,
) -> Result<ObjectValue> {
    let ObjectValue::Ref(Some(object_id)) = value else {
        return Ok(value);
    };
    if object_table.is_persistent(object_id)? {
        return Ok(ObjectValue::Ref(Some(object_id)));
    }
    Ok(ObjectValue::Ref(Some(
        self.promote_transaction_object_graph(object_table, object_id)?,
    )))
}

fn promote_raw_i31_ref(
    &mut self,
    object_table: &mut ObjectTable,
    raw_ref: u32,
) -> Result<ObjectId> {
    self.ensure_active()?;
    if let Some(promoted) = self.promoted_i31_refs.get(&raw_ref).copied() {
        return Ok(promoted);
    }
    let source = object_table.object_id_for_raw_i31_ref(u64::from(raw_ref))?;
    let promoted = self.promote_transaction_object_graph(object_table, source)?;
    self.promoted_i31_refs.insert(raw_ref, promoted);
    Ok(promoted)
}
```

Add `#[cfg(test)]` wrappers:

```rust
fn promote_transaction_object_graph_for_test(
    &mut self,
    object_table: &mut ObjectTable,
    source: ObjectId,
) -> Result<ObjectId>;

fn promoted_object_for_test(&self, source: ObjectId) -> Option<ObjectId>;

fn promoted_i31_ref_for_test(&self, raw_ref: u32) -> Option<ObjectId>;

fn staged_object_payload_for_test(&self, object_id: ObjectId) -> Option<&ObjectPayload>;
```

- [ ] **Step 5: Verify green**

Run:

```bash
cargo test -p wasmtime --lib persistent_promotion_graph -- --format terse
```

Expected: all graph tests pass.

- [ ] **Step 6: Commit**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs
git commit -m "Add transaction object promotion graph"
```

## Task 4: Promote Staged Persistent Root And Payload References Before Commit

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_promotion_commit -- --format terse`

- [ ] **Step 1: Write failing tests**

Add a nested module named `persistent_promotion_commit`:

```rust
mod persistent_promotion_commit {
    use super::*;

    #[test]
    fn root_delta_promotes_known_non_persistent_gc_ref() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let source = objects
            .allocate_struct_for_gc_ref(0x730, vec![ObjectValue::I32(30)])
            .unwrap();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(730));
        state.stage_global(0, GlobalSnapshot::GcRef(0x730)).unwrap();

        state
            .promote_persistent_references_before_commit(&mut objects)
            .unwrap();
        let delta = state.staged_persistent_root_delta(&objects).unwrap();
        let promoted = state.promoted_object_for_test(source).unwrap();

        assert_eq!(
            delta.roots
                .get(&PersistentRootKey::Global {
                    instance: None,
                    global_index: 0
                })
                .cloned()
                .unwrap(),
            object_set([promoted])
        );
    }

    #[test]
    fn root_delta_promotes_i31_gc_ref_immediate() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let raw_i31 = ObjectTable::encode_raw_i31_ref(19) as u32;
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(732));
        state.stage_global(0, GlobalSnapshot::GcRef(raw_i31)).unwrap();

        state
            .promote_persistent_references_before_commit(&mut objects)
            .unwrap();
        let delta = state.staged_persistent_root_delta(&objects).unwrap();
        let promoted = state.promoted_i31_ref_for_test(raw_i31).unwrap();

        assert_eq!(
            delta.roots
                .get(&PersistentRootKey::Global {
                    instance: None,
                    global_index: 0
                })
                .cloned()
                .unwrap(),
            object_set([promoted])
        );
        assert_eq!(
            state.staged_object_payload_for_test(promoted).unwrap(),
            &ObjectPayload::I31(19)
        );
    }

    #[test]
    fn persistent_owner_payload_ref_promotes_child_before_publication() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let child = objects
            .allocate_struct_for_gc_ref(0x731, vec![ObjectValue::I32(31)])
            .unwrap();
        let owner = objects
            .allocate_persistent_struct_for_gc_ref(0x732, vec![ObjectValue::Ref(None)])
            .unwrap();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(731));
        state.acquire_object_write(&objects, owner).unwrap();
        state
            .stage_struct_field(&objects, owner, 0, ObjectValue::Ref(Some(child)))
            .unwrap();

        state
            .promote_persistent_references_before_commit(&mut objects)
            .unwrap();
        let promoted_child = state.promoted_object_for_test(child).unwrap();

        assert_eq!(
            state.staged_object_payload_for_test(owner).unwrap(),
            &ObjectPayload::Struct(vec![ObjectValue::Ref(Some(promoted_child))])
        );
        assert_eq!(
            state.staged_object_payload_for_test(promoted_child).unwrap(),
            &ObjectPayload::Struct(vec![ObjectValue::I32(31)])
        );
    }
}
```

- [ ] **Step 2: Verify red**

Run:

```bash
cargo test -p wasmtime --lib persistent_promotion_commit -- --format terse
```

Expected: compile failure because
`promote_persistent_references_before_commit` does not exist, or runtime
failure from the existing promotion boundary.

- [ ] **Step 3: Add promotion-aware root conversion**

Replace the free helper path for staged roots with methods on
`TransactionState`:

```rust
fn persistent_object_id_for_gc_ref_after_promotion(
    &mut self,
    object_table: &mut ObjectTable,
    gc_ref: u32,
) -> Result<Option<ObjectId>> {
    if gc_ref == 0 {
        return Ok(None);
    }
    if ObjectTable::is_raw_i31_ref(u64::from(gc_ref)) {
        return Ok(Some(self.promote_raw_i31_ref(object_table, gc_ref)?));
    }
    let Some(object_id) = object_table.known_object_id_for_gc_ref(gc_ref) else {
        bail!(VOLATILE_GC_REF_PROMOTION_UNIMPLEMENTED);
    };
    if object_table.is_persistent(object_id)? {
        return Ok(Some(object_id));
    }
    Ok(Some(
        self.promote_transaction_object_graph(object_table, object_id)?,
    ))
}
```

Add a non-mutating lookup used after promotion has already completed:

```rust
fn persistent_object_id_for_gc_ref_after_completed_promotion(
    &self,
    object_table: &ObjectTable,
    gc_ref: u32,
) -> Result<Option<ObjectId>> {
    if gc_ref == 0 {
        return Ok(None);
    }
    if ObjectTable::is_raw_i31_ref(u64::from(gc_ref)) {
        return self
            .promoted_i31_refs
            .get(&gc_ref)
            .copied()
            .map(Some)
            .context(VOLATILE_GC_REF_PROMOTION_UNIMPLEMENTED);
    }
    let Some(object_id) = object_table.known_object_id_for_gc_ref(gc_ref) else {
        bail!(VOLATILE_GC_REF_PROMOTION_UNIMPLEMENTED);
    };
    if object_table.is_persistent(object_id)? {
        return Ok(Some(object_id));
    }
    self.promoted_objects
        .get(&object_id)
        .copied()
        .map(Some)
        .context(VOLATILE_GC_REF_PROMOTION_UNIMPLEMENTED)
}
```

Update `staged_persistent_root_delta` to use the completed-promotion lookup
for `GlobalSnapshot::GcRef` and `TableElementSnapshot::GcRef`.

- [ ] **Step 4: Add the pre-commit promotion pass**

Add:

```rust
pub(crate) fn promote_persistent_references_before_commit(
    &mut self,
    object_table: &mut ObjectTable,
) -> Result<bool> {
    self.ensure_active()?;
    let mut changed = false;

    for value in self.staged_globals.values().copied().collect::<Vec<_>>() {
        if let GlobalSnapshot::GcRef(gc_ref) = value {
            changed |= self
                .persistent_object_id_for_gc_ref_after_promotion(object_table, gc_ref)?
                .is_some();
        }
    }

    for value in self.staged_table_elements.values().copied().collect::<Vec<_>>() {
        if let TableElementSnapshot::GcRef(gc_ref) = value {
            changed |= self
                .persistent_object_id_for_gc_ref_after_promotion(object_table, gc_ref)?
                .is_some();
        }
    }

    let staged = self
        .staged_objects
        .iter()
        .map(|(&id, payload)| (id, payload.clone()))
        .collect::<Vec<_>>();
    for (object_id, payload) in staged {
        if object_table.is_persistent(object_id)? {
            let rewritten = self.rewrite_payload_refs_for_promotion(object_table, payload)?;
            if self.staged_objects.get(&object_id) != Some(&rewritten) {
                self.staged_objects.insert(object_id, rewritten);
                changed = true;
            }
        }
    }

    Ok(changed)
}
```

Keep the existing hard error for unknown raw `VMGcRef` values; only known
non-persistent objects are promotable in this wave.

- [ ] **Step 5: Verify green**

Run:

```bash
cargo test -p wasmtime --lib persistent_promotion_commit -- --format terse
cargo test -p wasmtime --lib persistent_ref_promotion_boundary -- --format terse
```

Expected: promotion commit tests pass, and unknown raw refs remain rejected.

- [ ] **Step 6: Commit**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs
git commit -m "Promote staged persistent object refs before commit"
```

## Task 5: Wire Promotion Into The Real Commit Path

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_promotion_commit_path -- --format terse`

- [ ] **Step 1: Write failing tests**

Add a focused unit test in `transaction.rs` that exercises the same helper
sequence as `transaction_commit_impl`:

```rust
#[test]
fn persistent_promotion_commit_path_publishes_promoted_root_object() {
    let (source, recovered_region, object_winners) =
        commit_file_backed_publications_for_test(733, |objects, state| {
            let volatile = objects
                .allocate_struct_for_gc_ref(0x733, vec![ObjectValue::I32(33)])
                .unwrap();
            state.stage_global(0, GlobalSnapshot::GcRef(0x733))?;
            Ok(volatile)
        })
        .unwrap();

    assert!(!object_winners.is_empty());
    assert_eq!(recovered_region.root_object_ids.len(), 1);
    assert_eq!(
        recovered_region.root_object_ids[0],
        object_winners[0].object_id
    );
    assert_ne!(recovered_region.root_object_ids[0], source.object_index);
}
```

Then update `commit_file_backed_publications_for_test` so the test exercises
the promotion step in the same place as real commit:

```rust
state.promote_persistent_references_before_commit(objects)?;
state.commit_object_payloads_into(objects, &mut object_publications)?;
```

- [ ] **Step 2: Verify red**

Run:

```bash
cargo test -p wasmtime --lib persistent_promotion_commit_path -- --format terse
```

Expected: failure because `commit_file_backed_publications_for_test` and real
commit do not run promotion yet.

- [ ] **Step 3: Wire `transaction_commit_impl`**

In `crates/wasmtime/src/runtime/vm/libcalls.rs`, update the object/root
publication section to:

```rust
let (object_publications, root_delta, persistent_gc_delta) = {
    let store = store.store_opaque_mut();
    let (state, object_table) = store.transaction_state_and_object_table_mut();
    state.promote_persistent_references_before_commit(object_table)?;
    let mut object_publications = Vec::new();
    state.commit_object_payloads_into(object_table, &mut object_publications)?;
    let root_delta = state.staged_persistent_root_delta(&*object_table)?;
    let root_publications = state.persistent_root_publications(&root_delta)?;
    let persistent_gc_delta =
        state.persistent_gc_commit_delta(&*object_table, &object_publications)?;
    object_publications.extend(root_publications);
    (object_publications, root_delta, persistent_gc_delta)
};
```

Use the same ordering in any test helper that emulates the commit sequence.

- [ ] **Step 4: Verify green**

Run:

```bash
cargo test -p wasmtime --lib persistent_promotion_commit_path -- --format terse
cargo test -p wasmtime --lib persistent_root_commit -- --format terse
cargo test -p wasmtime --lib persistent_gc_commit -- --format terse
```

Expected: all tests pass.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/libcalls.rs
git commit -m "Wire promotion into transaction commit"
```

## Task 6: Abort And Failure Cleanup

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_promotion_abort -- --format terse`

- [ ] **Step 1: Write failing tests**

Add a nested module named `persistent_promotion_abort`:

```rust
mod persistent_promotion_abort {
    use super::*;

    #[test]
    fn abort_frees_promoted_objects_and_clears_promotion_map() {
        clear_current_thread_transaction_for_test();
        let mut objects = ObjectTable::default();
        let source = objects
            .allocate_struct_for_gc_ref(0x740, vec![ObjectValue::I32(40)])
            .unwrap();
        let initial_live = objects.live_count();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(740));

        let promoted = state
            .promote_transaction_object_graph_for_test(&mut objects, source)
            .unwrap();
        assert!(objects.is_persistent(promoted).unwrap());

        state.abort_allocated_objects(&mut objects).unwrap();

        assert_eq!(objects.live_count(), initial_live);
        assert!(objects.live_slot(promoted).is_err());
        assert_eq!(state.promoted_object_for_test(source), None);
    }
}
```

- [ ] **Step 2: Verify red**

Run:

```bash
cargo test -p wasmtime --lib persistent_promotion_abort -- --format terse
```

Expected: fail if promoted target objects are not recorded in
`allocated_objects` or the promotion map is not workspace-local.

- [ ] **Step 3: Fix cleanup semantics**

Ensure `promote_transaction_object_graph` calls:

```rust
self.record_allocated_object(promoted)?;
```

Ensure `promoted_objects` is stored in the transaction workspace and is reset by
`clear_active`, `abort_allocated_objects`, `fail_allocated_objects`, and
`complete_commit` through the existing workspace install/reset flow.

- [ ] **Step 4: Verify green**

Run:

```bash
cargo test -p wasmtime --lib persistent_promotion_abort -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
```

Expected: abort tests pass and the transaction unit suite remains green.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs
git commit -m "Clean up promoted objects on abort"
```

## Task 7: File-Backed WAT End-To-End Promotion And Recovery

**Files:**
- Modify: `crates/wasmtime/tests/transaction_persistence.rs`
- Test: `cargo test -p wasmtime --test transaction_persistence real_tfunc_promotes_tstruct_root_and_recovers -- --format terse`

- [ ] **Step 1: Write failing integration test**

Add a test that reuses the existing `transaction_root_engine`,
`call_publish_root`, and public recovery helper in
`crates/wasmtime/tests/transaction_persistence.rs`:

```rust
#[test]
fn real_tfunc_promotes_tstruct_root_and_recovers() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("promote-root.tmemory");
    let tx_log_path = dir.path().join("promote-root.txlog");
    let engine = transaction_root_engine()?;
    let module = Module::new(
        &engine,
        wat::parse_str(
            r#"
            (module
              (type $s (tstruct (field (mut i32))))
              (tglobal $root (mut (ref null $s)) (ref.null $s))
              (tfunc (export "publish")
                (tglobal.set $root (tstruct.new $s (i32.const 77)))))
            "#,
        )?,
    )?;

    call_publish_root(&engine, &module, &tmemory_path, &tx_log_path, true)?;
    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;

    assert_eq!(recovered.root_object_ids.len(), 1);
    assert_eq!(recovered.object_winners.len(), 1);
    assert_eq!(
        recovered.root_object_ids[0],
        recovered.object_winners[0].object_id
    );

    Ok(())
}
```

- [ ] **Step 2: Verify red**

Run:

```bash
cargo test -p wasmtime --test transaction_persistence real_tfunc_promotes_tstruct_root_and_recovers -- --format terse
```

Expected before wiring: the transaction fails with the promotion-required
diagnostic or recovery has no object winner for the root.

- [ ] **Step 3: Verify green**

Run:

```bash
cargo test -p wasmtime --test transaction_persistence real_tfunc_promotes_tstruct_root_and_recovers -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
```

Expected: the new WAT test and all existing file-backed persistence tests pass.

- [ ] **Step 4: Commit**

Run:

```bash
git add crates/wasmtime/tests/transaction_persistence.rs
git commit -m "Test file-backed tstruct root promotion"
```

## Task 8: Documentation And Status Update

**Files:**
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
- Test: `rg -n "commit-time promotion|transaction-mirrored|VMGcRef" docs/shisoft/transactional-wasm-runtime-core-design.md docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Update the core design doc**

In `docs/shisoft/transactional-wasm-runtime-core-design.md`, update the
promotion section to say:

```markdown
The first implemented promotion wave promotes transaction-mirrored objects:
objects that already have `ObjectTable` payloads because they were created or
mutated through transactional `tstruct`/`tarray` helpers. Promotion assigns a
new persistent `ObjectId`; the volatile source `ObjectId` remains a runtime
wrapper and is not the durable identity. Arbitrary ordinary Wasmtime GC heap
objects that are not mirrored into `ObjectTable` remain rejected until the
Wasmtime-GC introspection adapter is implemented.
```

- [ ] **Step 2: Update the implementation log**

Add a new current-status section:

```markdown
## Current Status: Wave 11 VMGcRef Commit Promotion

Date: 2026-06-15

Implemented status:

- Commit-time promotion now converts transaction-mirrored volatile `VMGcRef`
  object graphs into new persistent `ObjectId` records before durable root and
  object publications are computed.
- Promotion reserves persistent targets before rewriting payloads, so cyclic
  graphs are representable.
- Promoted object records publish through the existing object-data redo log and
  root records publish through the existing root publication path before LP.
- Abort/fail paths discard uncommitted promoted objects through the transaction
  allocated-object cleanup path.

Still deferred:

- Promotion of arbitrary ordinary Wasmtime GC heap objects not mirrored in
  `ObjectTable`.
- Durable block/chunk retirement and persistent block reuse.
```

- [ ] **Step 3: Verify docs contain the status**

Run:

```bash
rg -n "Wave 11 VMGcRef Commit Promotion|transaction-mirrored|new persistent `ObjectId`" docs/shisoft/transactional-wasm-runtime-core-design.md docs/shisoft/transactional-wasm-implementation-log.md
```

Expected: each phrase has at least one match.

- [ ] **Step 4: Commit**

Run:

```bash
git add docs/shisoft/transactional-wasm-runtime-core-design.md docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Document VMGcRef commit promotion"
```

## Completion Criteria

- A non-null staged persistent root that points at a transaction-mirrored
  volatile `VMGcRef` commits successfully by promoting that object to a new
  persistent `ObjectId`.
- A staged persistent object payload that points at a volatile transaction
  object commits successfully after rewriting that edge to the promoted
  persistent `ObjectId`.
- A `ti31` reference crossing into persistent object data or a persistent root
  becomes a persistent `ObjectPayload::I31` record and recovers as object data,
  not as a raw runtime pointer.
- Cyclic transaction object graphs promote without infinite recursion because
  all targets are reserved before payload fill.
- Promoted object records and root records are published before LP and are
  recovered from file-backed storage.
- Unknown raw `VMGcRef`s not mirrored in `ObjectTable` still fail with the
  explicit promotion-required diagnostic.
- Abort/fail discards uncommitted promoted target objects.

## Final Verification

Run:

```bash
cargo fmt --check
cargo test -p wasmtime --lib ti31_persistent_object_domain -- --format terse
cargo test -p wasmtime --lib transaction_i31_reference_fields -- --format terse
cargo test -p wasmtime --lib persistent_promotion_reservation -- --format terse
cargo test -p wasmtime --lib persistent_promotion_graph -- --format terse
cargo test -p wasmtime --lib persistent_promotion_commit -- --format terse
cargo test -p wasmtime --lib persistent_promotion_commit_path -- --format terse
cargo test -p wasmtime --lib persistent_promotion_abort -- --format terse
cargo test -p wasmtime --lib persistent_ref_promotion_boundary -- --format terse
cargo test -p wasmtime --lib persistent_root_commit -- --format terse
cargo test -p wasmtime --lib persistent_gc_commit -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
```

Expected: every command exits successfully.

## Deferred After This Plan

- Introspect arbitrary ordinary Wasmtime GC heap objects by reading
  `VMGcHeader::ty`, resolving `VMSharedTypeIndex` back to module/runtime layout
  metadata, and copying fields/elements from `GcStore` through `VMStructRef` and
  `VMArrayRef`.
- Promote `tfuncref`, `texternref`, `exnref`, and host-only references by
  rewriting raw runtime pointers into symbolic durable identities. For
  `tfuncref`, use module namespace plus function index/name; for `texternref`,
  require a host-provided durable handle or serializer/deserializer contract.
- Replace remaining volatile `VMGcRef -> ObjectId` ABI scaffolding with a final
  `ObjectId`-carrying persistent `tref` representation.
- Add durable block/chunk retirement metadata and safe persistent block reuse.
