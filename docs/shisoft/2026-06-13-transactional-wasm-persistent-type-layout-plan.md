# Persistent Type Layout Metadata Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Store compact persistent type/layout metadata in region metadata space, store only object-specific metadata in persistent object headers, and rebuild the persistent object table from logs plus metadata during recovery.

**Architecture:** Wasmtime remains the semantic authority for validation, casts, subtyping, and field/array typing. The persistent runtime stores trace-oriented type layouts only: stable layout ids, type fingerprints, object-reference slots, array element trace kind, and object body sizing facts. Persistent object headers store `ObjectId`, `kind`, `type_layout_id`, length/body size, and version. Recovery first rebuilds the type layout registry from region metadata, then replays committed object publications and validates every persistent object record against a recovered layout id.

**Tech Stack:** Rust, Wasmtime runtime, existing `transaction` feature, `TMemory` block-region metadata descriptors, file-backed durable transaction log, existing transaction object heap and recovery code.

---

## Current Boundary

The implementation already has transactional objects, durable object publications, file-backed recovery, and a volatile object table. The remaining gap is that object records still use a generic `type_index` field and object tracing currently derives reference layout from decoded payload values. That is not enough for a persistent object model because recovery needs a durable, code-independent way to interpret object bodies and rebuild metadata.

The desired boundary is:

- Region metadata stores per-type trace layouts.
- Object records store a compact `type_layout_id`.
- Object table entries remain volatile and are rebuilt.
- Object data logs remain the durable source of object versions.
- Wasmtime module/type checking remains outside persistent storage.

## Invariants

- A persistent object record is valid only if its `type_layout_id` already exists durably in region metadata.
- Type metadata is persisted before any object publication that references it is durably published.
- Recovery rejects object records whose header kind disagrees with the recovered type layout kind.
- Recovery rejects duplicate committed versions for the same logical id.
- Object-table volatile indices, GC handles, refcounts, and runtime caches are never treated as persistent state.
- `ObjectId` is the persistent identity for objects and is encoded as an object-domain `GranuleId`.

## Wave 1: Rename The Durable Object Type Field

- [ ] Add a focused failing unit test in [object_heap.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/transaction/object_heap.rs) proving object record headers expose `type_layout_id`, not `type_index`.

  Test shape:

  ```rust
  #[test]
  fn object_record_header_uses_type_layout_id() {
      let record = encode_object_record(7, 11, ObjectKind::Struct as u16, 23, &[]);
      let header = TxObjectHeader::decode(record.as_slice()).unwrap();
      assert_eq!(header.type_layout_id, 23);
  }
  ```

  Expected initial result:

  ```text
  no field `type_layout_id` on type `TxObjectHeader`
  ```

- [ ] Rename `TxObjectHeader::type_index` to `type_layout_id` in [object_heap.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/transaction/object_heap.rs).

- [ ] Rename runtime fields and parameters:

  - `ObjectTableSlot::type_index` to `type_layout_id` in [transaction.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/transaction.rs).
  - `RecoveredObjectWinner::type_index` to `type_layout_id` in [recovery.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs).
  - Function parameters named `type_index` in object heap helpers to `type_layout_id`.

- [ ] Keep the on-disk `TxDataRecordHeader::type_info` field name unchanged for this wave, but map it at API boundaries as `type_layout_id`. This avoids a log-layout churn while making the runtime meaning explicit.

- [ ] Run:

  ```bash
  cargo test -p wasmtime transaction::tests::object_record_header_uses_type_layout_id
  cargo test -p wasmtime transaction::tests::object_heap
  ```

  Expected final result:

  ```text
  test result: ok
  ```

- [ ] Commit:

  ```bash
  git add crates/wasmtime/src/runtime/transaction/object_heap.rs crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs
  git commit -m "Rename persistent object type layout field"
  ```

## Wave 2: Add Persistent Type Layout Records

- [ ] Create [type_layout.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/transaction/type_layout.rs) and expose it from [transaction.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/transaction.rs).

- [ ] Add the durable type layout model:

  ```rust
  #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
  pub(crate) struct TypeLayoutId(u32);

  impl TypeLayoutId {
      pub(crate) const fn new(raw: u32) -> Option<Self> {
          if raw == 0 { None } else { Some(Self(raw)) }
      }

      pub(crate) const fn get(self) -> u32 {
          self.0
      }
  }

  #[derive(Clone, Copy, Debug, Eq, PartialEq)]
  #[repr(u16)]
  pub(crate) enum PersistentTypeKind {
      Struct = 1,
      Array = 2,
      I31 = 3,
      Extern = 4,
      Func = 5,
  }

  #[derive(Clone, Copy, Debug, Eq, PartialEq)]
  #[repr(u8)]
  pub(crate) enum TraceSlotKind {
      Scalar = 0,
      ObjectRef = 1,
  }

  #[derive(Clone, Copy, Debug, Eq, PartialEq)]
  pub(crate) struct StructTraceField {
      pub(crate) field_index: u32,
      pub(crate) field_offset: u32,
      pub(crate) value_size: u32,
      pub(crate) kind: TraceSlotKind,
  }

  #[derive(Clone, Debug, Eq, PartialEq)]
  pub(crate) enum PersistentTypeLayout {
      Struct {
          id: TypeLayoutId,
          fingerprint: u64,
          body_size: u32,
          fields: Vec<StructTraceField>,
      },
      Array {
          id: TypeLayoutId,
          fingerprint: u64,
          element_size: u32,
          element_kind: TraceSlotKind,
      },
      Scalar {
          id: TypeLayoutId,
          fingerprint: u64,
          kind: PersistentTypeKind,
      },
  }
  ```

- [ ] Implement a compact little-endian codec in the same module:

  - Fixed record header:
    - `magic: u32`
    - `record_len: u32`
    - `type_layout_id: u32`
    - `kind: u16`
    - `reserved: u16`
    - `fingerprint: u64`
    - `body_size_or_element_size: u32`
    - `field_count: u32`
    - `element_kind: u8`
    - `reserved2: [u8; 3]`
  - Field entry:
    - `field_index: u32`
    - `field_offset: u32`
    - `value_size: u32`
    - `kind: u8`
    - `reserved: [u8; 3]`

  Use a magic constant, not a format-version field:

  ```rust
  const TYPE_LAYOUT_RECORD_MAGIC: u32 = 0x544c_4159; // "TLAY"
  ```

- [ ] Add codec tests in [type_layout.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/transaction/type_layout.rs):

  - struct layout with two scalar fields and one object-ref field round-trips.
  - array layout with object-ref element round-trips.
  - scalar layout round-trips.
  - `TypeLayoutId::new(0)` returns `None`.
  - unknown layout kind is rejected.
  - invalid record length is rejected.

- [ ] Run:

  ```bash
  cargo test -p wasmtime transaction::type_layout
  ```

  Expected final result:

  ```text
  test result: ok
  ```

- [ ] Commit:

  ```bash
  git add crates/wasmtime/src/runtime/transaction/type_layout.rs crates/wasmtime/src/runtime/transaction.rs
  git commit -m "Add persistent type layout records"
  ```

## Wave 3: Add A Runtime Type Layout Registry

- [ ] Add `TypeLayoutRegistry` to [type_layout.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/transaction/type_layout.rs):

  ```rust
  #[derive(Clone, Debug, Default)]
  pub(crate) struct TypeLayoutRegistry {
      layouts: BTreeMap<TypeLayoutId, PersistentTypeLayout>,
      fingerprints: BTreeMap<u64, TypeLayoutId>,
  }
  ```

- [ ] Implement registry operations:

  - `insert(layout) -> Result<()>`
  - `get(id) -> Option<&PersistentTypeLayout>`
  - `contains(id) -> bool`
  - `id_for_fingerprint(fingerprint) -> Option<TypeLayoutId>`
  - `iter() -> impl Iterator<Item = &PersistentTypeLayout>`
  - `encode_all() -> Vec<u8>`
  - `decode_all(bytes) -> Result<TypeLayoutRegistry>`

- [ ] Enforce registry consistency:

  - Duplicate `TypeLayoutId` with identical record is accepted as idempotent.
  - Duplicate `TypeLayoutId` with different record is rejected.
  - Duplicate fingerprint mapped to a different layout id is rejected.
  - Layout id zero is rejected.

- [ ] Add registry to `ObjectTable` in [transaction.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/transaction.rs):

  ```rust
  type_layouts: TypeLayoutRegistry,
  ```

- [ ] Add `ObjectTable` helper methods:

  ```rust
  pub(crate) fn register_type_layout(&mut self, layout: PersistentTypeLayout) -> Result<()>;
  pub(crate) fn type_layouts(&self) -> &TypeLayoutRegistry;
  pub(crate) fn require_type_layout(&self, id: TypeLayoutId) -> Result<&PersistentTypeLayout>;
  ```

- [ ] Keep existing object creation APIs working by registering built-in scalar layouts for `I31`, `Extern`, and `Func` during `ObjectTable::new`. Struct and array object creation paths that currently pass `0` must be changed to pass an explicit nonzero layout id.

- [ ] Add tests:

  - registry accepts idempotent duplicate.
  - registry rejects conflicting duplicate id.
  - object allocation with missing struct layout id returns an error.
  - object allocation with registered struct layout id succeeds.

- [ ] Run:

  ```bash
  cargo test -p wasmtime transaction::type_layout
  cargo test -p wasmtime transaction::tests::object_table
  ```

  Expected final result:

  ```text
  test result: ok
  ```

- [ ] Commit:

  ```bash
  git add crates/wasmtime/src/runtime/transaction/type_layout.rs crates/wasmtime/src/runtime/transaction.rs
  git commit -m "Track persistent type layouts in object table"
  ```

## Wave 4: Persist Type Layouts In Region Metadata Space

- [ ] Add [metadata.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/vm/memory/tmemory/metadata.rs) and expose it from [tmemory.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/vm/memory/tmemory.rs).

- [ ] Use the existing metadata descriptor model in [block_region.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs). Add one descriptor kind for persistent type layouts:

  ```rust
  pub(crate) const TYPE_LAYOUT_META_KIND: u64 = 0x5459_5045_4c41_594f; // "TYPELAYO"
  ```

- [ ] Reserve metadata blocks during file-backed region initialization:

  - Block 0: `RegionHeader`.
  - Block 1: metadata descriptor table.
  - Block 2: append-only type layout metadata payload.
  - Log and data block allocation starts after reserved metadata blocks.

- [ ] Change `validate_region_header` in [block_region.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs):

  - Accept exactly one type-layout descriptor.
  - Reject descriptors whose offset/count overlap block 0, log blocks, or data blocks.
  - Reject descriptor records whose `kind` is unknown.

- [ ] Add metadata APIs on `FileBackedMemoryBlockRegion`:

  ```rust
  pub(crate) fn load_type_layout_metadata(&self) -> Result<TypeLayoutRegistry>;
  pub(crate) fn append_type_layout_metadata(&mut self, layout: &PersistentTypeLayout) -> Result<()>;
  ```

  The append path writes the encoded layout record, flushes the touched file range, and leaves layout metadata independent from transaction log LP bits.

- [ ] Add in-memory equivalents for tests on `VMemoryBlockRegion`, using the same codec and region metadata shape.

- [ ] Add tests in [block_region.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs) or [metadata.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/vm/memory/tmemory/metadata.rs):

  - new file-backed region contains metadata descriptor table.
  - reopen loads an empty type-layout registry.
  - append one layout, reopen, recover same layout.
  - append two layouts, reopen, recover both.
  - corrupt layout metadata record is rejected during metadata load.

- [ ] Run:

  ```bash
  cargo test -p wasmtime tmemory::metadata
  cargo test -p wasmtime tmemory::block_region
  ```

  Expected final result:

  ```text
  test result: ok
  ```

- [ ] Commit:

  ```bash
  git add crates/wasmtime/src/runtime/vm/memory/tmemory.rs crates/wasmtime/src/runtime/vm/memory/tmemory/metadata.rs crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
  git commit -m "Persist type layouts in region metadata"
  ```

## Wave 5: Enforce Type Layout Publication Ordering

- [ ] Extend the durable log backend boundary in [persist.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/transaction/persist.rs):

  ```rust
  pub(crate) trait TxDurableLogBackend {
      fn ensure_type_layout(&mut self, layout: &PersistentTypeLayout) -> Result<()>;
      fn has_type_layout(&self, id: TypeLayoutId) -> Result<bool>;
      // existing log/data methods remain unchanged
  }
  ```

- [ ] Implement `ensure_type_layout` for:

  - in-memory durable log backend.
  - file-backed durable log backend, delegating to the region metadata APIs.

- [ ] Rename `PendingPublication::type_info` to `type_layout_id` in [persist.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/transaction/persist.rs). Encode it into `TxDataRecordHeader::type_info` at the final wire boundary.

- [ ] Update `publish_object_publications_before_commit`:

  - Collect required type layouts from the object table registry.
  - Call `ensure_type_layout` before writing the object publication data record.
  - Return an error if any publication references an unknown layout id.
  - Preserve current data-record-first, log-entry-second, LP-last commit ordering.

- [ ] Add tests:

  - publication referencing an unknown layout id fails before any object log entry is written.
  - known layout metadata is persisted before object publication data.
  - repeated publication for an already persisted layout does not append a duplicate layout record.

- [ ] Run:

  ```bash
  cargo test -p wasmtime transaction::persist
  cargo test -p wasmtime transaction::tests::object_publication
  ```

  Expected final result:

  ```text
  test result: ok
  ```

- [ ] Commit:

  ```bash
  git add crates/wasmtime/src/runtime/transaction/persist.rs crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
  git commit -m "Persist type layouts before object publications"
  ```

## Wave 6: Recover Object Table From Layout Metadata Plus Logs

- [ ] Extend [recovery.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs):

  ```rust
  pub(crate) struct RecoveredRegion {
      pub(crate) object_winners: Vec<RecoveredObjectWinner>,
      pub(crate) root_object_ids: Vec<u64>,
      pub(crate) type_layouts: TypeLayoutRegistry,
      // existing fields remain
  }
  ```

- [ ] Recovery sequence:

  1. Load type-layout metadata from region metadata space.
  2. Scan committed transaction log entries.
  3. Replay object publication winners.
  4. Decode each object record header.
  5. Verify `type_layout_id` exists.
  6. Verify header `kind` agrees with the recovered layout kind.
  7. Store winner with `type_layout_id`.

- [ ] Update `ObjectTable::rebuild_from_recovered_object_winners`:

  - Accept recovered `TypeLayoutRegistry`.
  - Install the registry before installing recovered slots.
  - Rebuild slots with `type_layout_id`.
  - Keep volatile GC handles and function handles empty unless explicitly reconstructed by runtime APIs.

- [ ] Add recovery tests:

  - recovery rejects object records whose layout id is missing.
  - recovery rejects kind/layout mismatch.
  - recovery accepts a struct object with a matching struct layout.
  - recovery accepts an array object with a matching array layout.
  - recovered object table contains slots keyed by persistent `ObjectId`.

- [ ] Run:

  ```bash
  cargo test -p wasmtime tmemory::recovery
  cargo test -p wasmtime transaction::tests::object_table_rebuild
  ```

  Expected final result:

  ```text
  test result: ok
  ```

- [ ] Commit:

  ```bash
  git add crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs crates/wasmtime/src/runtime/transaction.rs
  git commit -m "Recover object table with persistent type layouts"
  ```

## Wave 7: Use Layout Metadata For Durable Reference Tracing

- [ ] Replace payload-derived tracing in [object_heap.rs](/home/shisoft/Code/Research/wasmtime/crates/wasmtime/src/runtime/transaction/object_heap.rs) with layout-guided tracing for persistent objects.

- [ ] Implement:

  ```rust
  pub(crate) fn trace_object_refs_with_layout(
      layout: &PersistentTypeLayout,
      payload: &[u8],
      out: &mut Vec<ObjectId>,
  ) -> Result<()>;
  ```

- [ ] Struct tracing:

  - For each `StructTraceField` with `ObjectRef`, read the object-ref ABI value at `field_offset`.
  - Ignore scalar fields.
  - Reject field offsets that exceed payload length.

- [ ] Array tracing:

  - If `element_kind` is `Scalar`, no object refs are produced.
  - If `element_kind` is `ObjectRef`, iterate by `element_size`.
  - Reject payload lengths that are not divisible by `element_size`.

- [ ] Keep the existing payload-derived tracing helpers only for tests that exercise object payload encoding without a persistent registry. Mark those helpers `#[cfg(test)]`.

- [ ] Add tests:

  - layout-guided struct tracing finds object refs.
  - layout-guided struct tracing ignores scalar slots.
  - layout-guided array tracing finds object refs.
  - bad field offset is rejected.
  - bad array payload length is rejected.

- [ ] Run:

  ```bash
  cargo test -p wasmtime transaction::object_heap::trace
  cargo test -p wasmtime transaction::tests::object_recovery_reference_graph
  ```

  Expected final result:

  ```text
  test result: ok
  ```

- [ ] Commit:

  ```bash
  git add crates/wasmtime/src/runtime/transaction/object_heap.rs crates/wasmtime/src/runtime/transaction/type_layout.rs crates/wasmtime/src/runtime/transaction.rs
  git commit -m "Trace persistent object references from type layouts"
  ```

## Wave 8: Connect Wasmtime Type Information To Layout Ids

- [ ] Add a translation/runtime mapping layer that converts Wasmtime GC type information into `PersistentTypeLayout` records. Place the first implementation near the existing transaction runtime boundary, not in persistent storage code.

- [ ] Required mapping behavior:

  - Struct field value types become struct trace fields.
  - Reference-typed struct fields become `TraceSlotKind::ObjectRef`.
  - Non-reference fields become `TraceSlotKind::Scalar`.
  - Array element reference types become `TraceSlotKind::ObjectRef`.
  - Array element non-reference types become `TraceSlotKind::Scalar`.
  - Fingerprints are deterministic over Wasmtime type shape and transactional object kind.

- [ ] Add an internal helper and a small adapter type:

  ```rust
  pub(crate) struct WasmtimePersistentFieldLayout {
      pub(crate) field_index: u32,
      pub(crate) field_offset: u32,
      pub(crate) value_size: u32,
      pub(crate) is_object_ref: bool,
  }

  pub(crate) fn persistent_layout_for_wasmtime_struct_type(
      type_index: u32,
      body_size: u32,
      fields: &[WasmtimePersistentFieldLayout],
  ) -> PersistentTypeLayout;

  pub(crate) fn persistent_layout_for_wasmtime_array_type(
      type_index: u32,
      element_size: u32,
      element_is_object_ref: bool,
  ) -> PersistentTypeLayout;
  ```

  Use the concrete Wasmtime type descriptors available at the implementation site. The persistent module must not own Wasm validation or cast semantics.

- [ ] Wire struct/array allocation paths so persistent object creation receives a nonzero `TypeLayoutId`.

- [ ] Tests:

  - two identical Wasmtime struct shapes produce the same fingerprint and layout id.
  - different field reference maps produce different fingerprints.
  - persistent struct allocation registers layout before publication.
  - persistent array allocation registers layout before publication.

- [ ] Run:

  ```bash
  cargo test -p wasmtime transaction::tests::wasmtime_type_layout_mapping
  cargo test -p wasmtime transaction::tests::persistent_object_publication
  ```

  Expected final result:

  ```text
  test result: ok
  ```

- [ ] Commit:

  ```bash
  git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/type_layout.rs
  git commit -m "Map Wasmtime object types to persistent layouts"
  ```

## Wave 9: File-Backed End-To-End Recovery Tests

- [ ] Add file-backed tests that create a temporary persistent region, register layouts, commit object transactions, reopen the region, recover, and read back the object table.

- [ ] Test cases:

  - persistent struct with only scalar fields.
  - persistent struct containing a persistent object reference.
  - persistent array of scalars.
  - persistent array of object references.
  - transaction mixing tmemory undo records and object publications.
  - missing layout metadata causes recovery rejection.
  - object table volatile metadata is absent after recovery and rebuilt by `ObjectId`.

- [ ] Tests should assert:

  - committed object data is read from recovered object records.
  - `ObjectId` identity survives recovery.
  - `type_layout_id` survives recovery.
  - tmemory recovery still applies undo-log semantics.
  - object publications still apply redo-log semantics.

- [ ] Run:

  ```bash
  cargo test -p wasmtime transaction::tests::file_backed_object_layout_recovery
  cargo test -p wasmtime transaction::tests::file_backed_mixed_tmemory_object_recovery
  ```

  Expected final result:

  ```text
  test result: ok
  ```

- [ ] Commit:

  ```bash
  git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs
  git commit -m "Test file-backed persistent object layout recovery"
  ```

## Wave 10: Documentation And Progress Tracking

- [ ] Update [transactional-wasm-runtime-core-design.md](/home/shisoft/Code/Research/wasmtime/docs/shisoft/transactional-wasm-runtime-core-design.md):

  - Add the region metadata/type layout section.
  - Clarify that type metadata is trace metadata, not Wasm semantic metadata.
  - Clarify object headers carry object-specific metadata only.
  - Clarify object table volatility and recovery rebuild.
  - Clarify type layout persistence-before-publication ordering.

- [ ] Update [transactional-wasm-implementation-log.md](/home/shisoft/Code/Research/wasmtime/docs/shisoft/transactional-wasm-implementation-log.md):

  - Record the implemented layout registry.
  - Record the file-backed metadata persistence status.
  - Record passing recovery tests.
  - Record any remaining GC integration boundaries.

- [ ] Update [transactional-wasm-all-wast-completion-roadmap.md](/home/shisoft/Code/Research/wasmtime/docs/shisoft/transactional-wasm-all-wast-completion-roadmap.md) only if object metadata work changes WAST status.

- [ ] Run final verification:

  ```bash
  cargo test -p wasmtime transaction
  cargo test -p wasmtime tmemory
  cargo test -p wasmtime --test all
  ```

  Expected final result:

  ```text
  test result: ok
  ```

- [ ] Commit:

  ```bash
  git add docs/shisoft/transactional-wasm-runtime-core-design.md docs/shisoft/transactional-wasm-implementation-log.md docs/shisoft/transactional-wasm-all-wast-completion-roadmap.md
  git commit -m "Document persistent type layout metadata"
  ```

## Completion Criteria

- All persistent object records use `type_layout_id` terminology in runtime code.
- The persistent type layout registry is durably stored in region metadata space.
- Object publication fails before durable publication if its layout id is unknown.
- Recovery rebuilds the persistent object table from type metadata plus committed object publication logs.
- Durable object reference tracing uses recovered type layouts.
- File-backed recovery tests cover struct, array, object references, and mixed tmemory/object transactions.
- No persistent object-table volatile indices, GC handles, or refcounts are written as durable state.

## Rollback Plan

- Each wave has an isolated commit.
- If metadata-region reservation breaks existing file-backed tmemory tests, revert Wave 4 and keep Waves 1-3 because they are runtime-only naming and registry work.
- If Wasmtime type mapping is blocked by unavailable runtime descriptors, keep Waves 1-7 and make persistent object allocation require explicit `TypeLayoutId` at the transaction runtime boundary until the descriptor path is exposed.
