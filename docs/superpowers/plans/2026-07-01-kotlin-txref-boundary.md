# Kotlin TxRef TxCopyable Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement explicit Kotlin `TxRef` boundaries, `TxCopyable` copyability checks, universal transactional-Wasm transaction-local object allocation, and runtime live-GC promotion at all persistent-reference write boundaries.

**Architecture:** The Kotlin SDK exposes `TxRef`, `make_txn_ref`, `TxRef.get`, and `TxCopyable`. The Kotlin rewriter enforces source-level explicitness and copyability. The runtime behavior is language-neutral: ordinary transactional Wasm constructors allocate through a transaction-local object table. Commit promotes only persistent-root-reachable graphs, rebinds handles for promoted objects to their promoted persistent `ObjectId`s, discards all other local slots and unpromoted handle mappings, and never publishes transaction-local objects into the shared volatile object table merely because the transaction commits. Transaction handle allocation exclusion is monotonic for an `ObjectTable` lifetime: an unpromoted or expired handle remains in `reserved_transaction_ref_handles` as a tombstone until full `ObjectTable` reset, while a promoted handle leaves that set only when installed in the permanent handle mapping, which still prevents reuse. Neither case makes a handle a persistence root. This runtime rule is universal across frontends, while `TxRef` and `TxCopyable` remain the Kotlin source-level boundary. The static-initializer exception and pre-LP atomicity remain unchanged. Detailed runtime execution is refined in `docs/superpowers/plans/2026-07-15-transaction-local-reachability.md`.

**Tech Stack:** Rust (`wasmtime-transaction-tools`, Wasmtime runtime/libcalls, transaction runtime tests), Kotlin Multiplatform Wasm/WASI SDK examples, Wasm parser/encoder tests, existing transaction promotion machinery.

---

## Conflict Resolution Locked By This Plan

- Do not rewrite accepted ordinary constructors to `tstruct.new` or `tarray.new` just because a type is `@Persistent` or `TxCopyable`.
- Do make transaction-local allocation universal for transactional Wasm runtime constructors, not a Kotlin-only behavior. Any frontend emitting `tstruct.new` or `tarray.new*` gets the same transaction-local object table semantics.
- Do keep `TxRef` and `TxCopyable` as the Kotlin-only source-level boundary; runtime reachability, promotion, and handle semantics apply universally to transactional Wasm constructors from any frontend.
- Do not make runtime check whether a live GC ref came from `TxRef.get()`. Explicitness is compile-time-only.
- Do promote ordinary live WasmGC refs at every persistent-reference write/staging boundary, including `tstruct.set`, `tarray.set`, `tarray.fill`, `tglobal.set`, root publication, and persistent table writes.
- Do reuse `TransactionState.promoted_gc_refs`; do not add a second promotion cache.
- Do make transaction handle allocation exclusion monotonic for an `ObjectTable` lifetime: unpromoted or expired handles remain in `reserved_transaction_ref_handles` until full `ObjectTable` reset, while promoted handles leave that set only when installed in the permanent handle mapping, which still prevents reuse. Neither case makes a handle a persistence root.
- Do preserve the static-initializer exception and pre-LP atomicity; reachability-based finalization changes which `ObjectId`s survive commit, not the durable publication boundary.
- Do not treat `gcWasm.capture = "allModuleGcTypes"` as a source-level persistent boundary. Captured Kotlin runtime types support layouts and copyability only.
- Do accept Kotlinx serialization `@Serializable` metadata as copyability evidence when available, but the transaction denylist still wins.
- Do not reuse stale implementation patches that added eager `tstruct.new` expectations. Start execution from a fresh worktree based on the latest `transaction` branch.

## File Structure

- Modify `examples/transaction-kotlin/sdk/src/commonMain/kotlin/twasm/Transaction.kt`: add `TxRef`, `TxCopyable`, `make_txn_ref`, and `TxRef.get`.
- Modify `crates/transaction-tools/src/kotlin/metadata.rs`: add sidecar copyability metadata with `copyableTypes`, validate structural copyability, and keep `@Persistent` implying copyable.
- Modify `examples/transaction-kotlin/bank/twasm.kotlin.json`: add `gcWasm.capture`, `Bank.note`, and any copyable DTO metadata added to the example.
- Modify `examples/transaction-kotlin/bank/src/wasmWasiMain/kotlin/Main.kt`: demonstrate `make_txn_ref` and `TxRef.get` on a `String` note.
- Modify `crates/transaction-tools/src/kotlin/rewrite.rs`: track explicit persistent types separately from generic captured GC types, detect `TxRef.get`, enforce provenance and copyability at source-level persistent boundaries, and keep constructors ordinary.
- Modify `crates/transaction-tools/tests/kotlin_rewrite.rs`: add failing/passing rewriter tests for TxRef, TxCopyable, Serializable-as-copyable metadata, generic captured types, and constructor behavior.
- Modify `crates/wasmtime/src/runtime/vm/libcalls.rs`: add a shared persistent-slot ABI conversion helper that promotes ordinary live GC refs through the existing promotion adapter and map; route struct, array, global, table/root write paths through it.
- Modify `crates/wasmtime/src/runtime/transaction/state.rs` and `crates/wasmtime/src/runtime/transaction/object_table.rs`: add the transaction-local object table, handle resolution, monotonic per-`ObjectTable` handle exclusion, reachability-based finalization, and local-handle rebinding to promoted persistent `ObjectId`s used by all transactional Wasm constructors.
- Modify `crates/wasmtime/src/runtime/transaction/tests.rs`: add focused runtime tests for repeated live GC ref promotion through persistent write/staging boundaries.
- Modify `tests/transaction_kotlin_toolset.rs`: update Kotlin bank recovery assertions for persisted `String` note and aliasing.

## Task 0: Execution Workspace Cleanup

**Files:**
- Verify only: repository worktree state

- [ ] **Step 1: Create a fresh isolated worktree**

Run from `/home/shisoft/Code/Research/wasmtime`:

```bash
git worktree add -b kotlin-txref-txcopyable-impl /home/shisoft/Code/Research/wasmtime-kotlin-txref-txcopyable-impl transaction
```

Expected: a new worktree is created at `/home/shisoft/Code/Research/wasmtime-kotlin-txref-txcopyable-impl`.

- [ ] **Step 2: Verify stale patches are not present**

Run:

```bash
cd /home/shisoft/Code/Research/wasmtime-kotlin-txref-txcopyable-impl
git status --short
rg -n "tstruct\\.new|TStructNew|persistent_struct_has_ref_fields" crates/transaction-tools/src/kotlin/rewrite.rs crates/transaction-tools/tests/kotlin_rewrite.rs
```

Expected: `git status --short` is empty. The `rg` command may find existing unrelated transactional-constructor tests, but it must not find TxRef tests requiring `tstruct.new` or helper code that rewrites constructors solely because persistent structs have ref fields.

## Task 1: Kotlin SDK And Sidecar Metadata

**Files:**
- Modify: `examples/transaction-kotlin/sdk/src/commonMain/kotlin/twasm/Transaction.kt`
- Modify: `crates/transaction-tools/src/kotlin/metadata.rs`
- Test: `crates/transaction-tools/tests/kotlin_metadata.rs`

- [ ] **Step 1: Add the SDK API**

In `examples/transaction-kotlin/sdk/src/commonMain/kotlin/twasm/Transaction.kt`, add this API after `TxnFunc` and before `transaction`:

```kotlin
class TxRef<T> internal constructor(val value: T)

@Target(AnnotationTarget.CLASS)
@Retention(AnnotationRetention.BINARY)
annotation class TxCopyable

fun <T> make_txn_ref(value: T): TxRef<T> =
    TxRef(value)

fun <T> TxRef<T>.get(): T =
    value
```

- [ ] **Step 2: Add sidecar copyable metadata types**

In `crates/transaction-tools/src/kotlin/metadata.rs`, add `copyable_types` to `KotlinSidecar`:

```rust
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct KotlinSidecar {
    pub version: u32,
    pub module: String,
    #[serde(default)]
    pub gc_wasm: KotlinGcWasmPolicy,
    pub persistent_types: Vec<KotlinPersistentType>,
    #[serde(default)]
    pub copyable_types: Vec<KotlinCopyableType>,
    pub transaction_functions: Vec<String>,
    pub roots: Vec<KotlinRoot>,
}
```

Update `KotlinSidecar::new` to initialize `copyable_types: Vec::new()`.

Add these type definitions after `KotlinPersistentType`:

```rust
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct KotlinCopyableType {
    pub name: String,
    pub kind: KotlinPersistentKind,
    #[serde(default)]
    pub fields: Vec<KotlinField>,
    #[serde(default)]
    pub element: Option<KotlinField>,
    #[serde(default)]
    pub source: KotlinCopyableSource,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum KotlinCopyableSource {
    TxCopyable,
    Serializable,
    Builtin,
}

impl Default for KotlinCopyableSource {
    fn default() -> Self {
        Self::TxCopyable
    }
}
```

- [ ] **Step 3: Validate copyable metadata**

In `validate_kotlin_sidecar`, build two name sets:

```rust
let mut persistent_type_names = HashSet::with_capacity(sidecar.persistent_types.len());
let mut copyable_type_names = HashSet::with_capacity(sidecar.copyable_types.len());
```

Persistent type names are inserted into `persistent_type_names`. Copyable type names are inserted into `copyable_type_names`; reject duplicates and reject a `copyableTypes` entry whose name is already in `persistentTypes` because `@Persistent` already implies copyable:

```rust
if persistent_type_names.contains(copyable_type.name.as_str()) {
    bail!(
        "copyable type {} duplicates persistent type; @Persistent already implies TxCopyable",
        copyable_type.name
    );
}
```

For copyable field validation, the allowed named ref targets are:

```rust
let mut copyable_ref_targets = persistent_type_names.clone();
copyable_ref_targets.extend(copyable_type_names.iter().copied());
```

Call `validate_field` for every copyable struct field and array element with `copyable_ref_targets`.

- [ ] **Step 4: Add metadata parsing tests**

Add or update tests to cover this JSON:

```json
{
  "version": 1,
  "module": "copyable-fixture",
  "persistentTypes": [
    {
      "name": "Account",
      "kind": "struct",
      "fields": [
        { "name": "balance", "kind": "i64", "nullable": false }
      ]
    }
  ],
  "copyableTypes": [
    {
      "name": "TransferNote",
      "kind": "struct",
      "source": "serializable",
      "fields": [
        { "name": "text", "kind": "ref", "type": "kotlin.String", "nullable": false },
        { "name": "source", "kind": "ref", "type": "Account", "nullable": false }
      ]
    }
  ],
  "gcWasm": { "capture": "allModuleGcTypes", "denyTypes": [] },
  "transactionFunctions": ["publish"],
  "roots": []
}
```

Expected parse result: one persistent type, one copyable type, copyable source `Serializable`.

Add a rejection test where `copyableTypes` contains `"Account"` while `persistentTypes` also contains `"Account"`. Expected error substring:

```text
@Persistent already implies TxCopyable
```

- [ ] **Step 5: Run metadata-focused tests**

Run:

```bash
cargo test -p wasmtime-transaction-tools kotlin_metadata -- --nocapture
```

Expected: metadata tests PASS.

- [ ] **Step 6: Commit SDK and metadata**

Run:

```bash
git add examples/transaction-kotlin/sdk/src/commonMain/kotlin/twasm/Transaction.kt \
  crates/transaction-tools/src/kotlin/metadata.rs \
  crates/transaction-tools/tests/kotlin_metadata.rs
git commit -m "feat: add Kotlin TxCopyable metadata"
```

Expected: commit succeeds and contains only SDK, metadata, and metadata-test changes.

## Task 2: Runtime Persistent-Slot Promotion

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add runtime tests for repeated live-GC promotion**

In `crates/wasmtime/src/runtime/transaction/tests.rs`, inside `mod persistent_promotion_commit`, add a test using `FakeOrdinaryGcPromotionAdapter`:

```rust
#[test]
fn persistent_write_promotion_reuses_promoted_gc_ref() {
    clear_current_thread_transaction_for_test();
    let mut objects = ObjectTable::default();
    let mut adapter = FakeOrdinaryGcPromotionAdapter::default().with_source(
        0x950,
        OrdinaryGcPromotionSource::Struct {
            type_layout_id: type_layout::TypeLayoutId::DEFAULT_STRUCT,
            fields: vec![OrdinaryGcPromotionValue::I64(950)],
        },
    );
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(950));

    let first = state
        .promote_gc_ref_for_live_transaction_ref_with_adapter(&mut objects, 0x950, &mut adapter)
        .unwrap()
        .unwrap();
    let second = state
        .promote_gc_ref_for_live_transaction_ref_with_adapter(&mut objects, 0x950, &mut adapter)
        .unwrap()
        .unwrap();

    assert_eq!(first, second);
    assert_eq!(state.promoted_gc_ref_for_test(0x950), Some(first));
    assert!(objects.is_persistent(first).unwrap());
    assert_eq!(
        state.staged_object_payload_for_test(first).unwrap(),
        &ObjectPayload::Struct(vec![ObjectValue::I64(950)])
    );
}
```

This test proves the cache behavior used by all persistent write boundaries.

- [ ] **Step 2: Add a persistent-slot ABI helper**

In `crates/wasmtime/src/runtime/vm/libcalls.rs`, add this helper next to `object_value_from_transaction_abi`:

```rust
fn object_value_from_persistent_slot_abi(
    store: &mut StoreOpaque,
    abi: ObjectValueAbi,
) -> Result<ObjectValue> {
    let (tag, low, high) = abi.as_parts();
    if tag != OBJECT_VALUE_ABI_TAG_REF {
        return abi.to_object_value();
    }

    let (engine, gc_store, durable_refs, state, object_table) =
        store.transaction_promotion_context_mut();

    if high != OBJECT_VALUE_ABI_LIVE_REF_KIND_GC {
        return live_ref_value_from_raw(durable_refs, object_table, high, low);
    }

    if low == 0 {
        return Ok(ObjectValue::Ref(None));
    }
    if ObjectTable::is_raw_i31_ref(low) {
        return Ok(ObjectValue::I31(ObjectTable::decode_raw_i31_ref(low)?));
    }

    let gc_ref = u32::try_from(low).context("live GC reference does not fit u32")?;
    if let Some(value) = live_object_ref_value_from_ambiguous_raw(
        durable_refs,
        object_table,
        gc_ref,
        false,
    )? {
        return Ok(value);
    }

    let promoted = {
        let mut adapter = StoreBackedOrdinaryGcPromotionAdapter::new(engine, gc_store, durable_refs);
        state.promote_gc_ref_for_live_transaction_ref_with_adapter(
            object_table,
            gc_ref,
            &mut adapter,
        )?
    };

    promoted
        .map(|object_id| ObjectValue::Ref(Some(object_id)))
        .with_context(|| {
            format!("ordinary live GC reference {gc_ref:#x} did not promote to a persistent object")
        })
}
```

This helper owns the persistent-slot normalization policy. Do not duplicate
ordinary live-GC promotion logic in individual `tstruct`, `tarray`, `tglobal`, or
`ttable` call sites.

- [ ] **Step 3: Route `tstruct.set` through the helper**

In `transaction_tstruct_set_impl`, replace:

```rust
let store = store.store_opaque_mut();
let (durable_refs, state, object_table) =
    store.transaction_durable_refs_state_and_object_table_mut();
let object_id = object_table.object_id_for_transaction_ref_handle(gc_ref)?;
let value = object_value_from_transaction_abi(durable_refs, object_table, abi)?;
state.stage_struct_field(object_table, object_id, field, value)
```

with:

```rust
let store = store.store_opaque_mut();
let value = object_value_from_persistent_slot_abi(store, abi)?;
let (_, _, _, state, object_table) = store.transaction_promotion_context_mut();
let object_id = object_table.object_id_for_transaction_ref_handle(gc_ref)?;
state.stage_struct_field(object_table, object_id, field, value)
```

- [ ] **Step 4: Route array write boundaries through the helper**

Apply the same replacement pattern to:

- `transaction_tarray_set_impl`
- `transaction_tarray_fill_impl`

Use `object_value_from_persistent_slot_abi` for the element value before calling `stage_array_element` or `fill_array_range`.

- [ ] **Step 5: Route ref-typed globals through the helper**

In `transaction_tglobal_set_impl`, after `ensure_global_snapshot_type(snapshot, wasm_ty)?`, add a ref-typed branch:

```rust
if let GlobalSnapshot::GcRef(gc_ref) = snapshot {
    let store = store.store_opaque_mut();
    let abi = ObjectValueAbi::from_live_parts(
        OBJECT_VALUE_ABI_TAG_REF,
        u64::from(gc_ref),
        OBJECT_VALUE_ABI_LIVE_REF_KIND_GC,
    )?;
    let value = object_value_from_persistent_slot_abi(store, abi)?;
    let (_, _, durable_refs, state, object_table) = store.transaction_promotion_context_mut();
    let promoted_snapshot = match value {
        ObjectValue::Ref(Some(object_id)) => {
            let handle = object_table.transaction_ref_handle_for_object_id_avoiding(object_id, |raw| {
                live_ref_raw_is_registered(&*durable_refs, raw)
            })?;
            GlobalSnapshot::GcRef(handle)
        }
        ObjectValue::Ref(None) => GlobalSnapshot::GcRef(0),
        ObjectValue::I31(value) => {
            GlobalSnapshot::GcRef(ObjectTable::encode_raw_i31_ref(value) as u32)
        }
        _ => bail!("persistent global ref promotion produced non-ref value"),
    };
    state.stage_global_owned(Some(instance), global_index.as_u32(), promoted_snapshot)?;
    return Ok(());
}
```

- [ ] **Step 6: Route table/root publication paths through the helper**

Add this helper near `table_element_snapshot_from_raw`:

```rust
fn normalize_persistent_table_snapshot(
    store: &mut StoreOpaque,
    snapshot: TableElementSnapshot,
) -> Result<TableElementSnapshot> {
    let TableElementSnapshot::GcRef(gc_ref) = snapshot else {
        return Ok(snapshot);
    };
    let abi = ObjectValueAbi::from_live_parts(
        OBJECT_VALUE_ABI_TAG_REF,
        u64::from(gc_ref),
        OBJECT_VALUE_ABI_LIVE_REF_KIND_GC,
    )?;
    let value = object_value_from_persistent_slot_abi(store, abi)?;
    let (_, _, durable_refs, _state, object_table) = store.transaction_promotion_context_mut();
    match value {
        ObjectValue::Ref(Some(object_id)) => {
            let handle = object_table.transaction_ref_handle_for_object_id_avoiding(object_id, |raw| {
                live_ref_raw_is_registered(&*durable_refs, raw)
            })?;
            Ok(TableElementSnapshot::GcRef(handle))
        }
        ObjectValue::Ref(None) => Ok(TableElementSnapshot::GcRef(0)),
        ObjectValue::I31(value) => {
            Ok(TableElementSnapshot::GcRef(ObjectTable::encode_raw_i31_ref(value) as u32))
        }
        _ => bail!("persistent table ref promotion produced non-ref value"),
    }
}
```

In `transaction_ttable_set_impl`, after `let snapshot = { ... };`, replace the direct staging call with:

```rust
let snapshot = normalize_persistent_table_snapshot(store.store_opaque_mut(), snapshot)?;
store
    .store_opaque_mut()
    .transaction_state_mut()
    .stage_table_element_owned(Some(instance), table, index, snapshot)?;
```

In `transaction_ttable_grow_impl`, replace:

```rust
let init = table_element_snapshot_from_raw(element_type, init)?;
```

with:

```rust
let init = {
    let init = table_element_snapshot_from_raw(element_type, init)?;
    normalize_persistent_table_snapshot(store.store_opaque_mut(), init)?
};
```

Keep pure state methods such as `stage_table_element_owned` unchanged; VM/libcall
boundaries must store normalized snapshots.

- [ ] **Step 7: Route root marker publication through normalized globals**

The Kotlin root lowering emits `tglobal.set`, so root publication is covered by
`transaction_tglobal_set_impl`. Add this regression assertion to the runtime
tests in Step 1 if it is not already covered by the Kotlin toolset test:

```rust
#[test]
fn global_write_promotion_reuses_promoted_gc_ref() {
    clear_current_thread_transaction_for_test();
    let mut objects = ObjectTable::default();
    let mut adapter = FakeOrdinaryGcPromotionAdapter::default().with_source(
        0x951,
        OrdinaryGcPromotionSource::Struct {
            type_layout_id: type_layout::TypeLayoutId::DEFAULT_STRUCT,
            fields: vec![OrdinaryGcPromotionValue::I64(951)],
        },
    );
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(951));

    let promoted = state
        .promote_gc_ref_for_live_transaction_ref_with_adapter(&mut objects, 0x951, &mut adapter)
        .unwrap()
        .unwrap();
    state.stage_global(0, GlobalSnapshot::GcRef(0x951)).unwrap();
    state
        .promote_persistent_references_before_commit_with_adapter(&mut objects, &mut adapter)
        .unwrap();

    assert_eq!(state.promoted_gc_ref_for_test(0x951), Some(promoted));
}
```

- [ ] **Step 8: Run runtime promotion tests**

Run:

```bash
cargo test -p wasmtime --lib persistent_write_promotion_reuses_promoted_gc_ref --features transaction -- --nocapture
cargo test -p wasmtime --lib global_write_promotion_reuses_promoted_gc_ref --features transaction -- --nocapture
cargo test -p wasmtime --lib persistent_promotion_commit --features transaction -- --nocapture
```

Expected: all commands PASS.

- [ ] **Step 9: Commit runtime promotion changes**

Run:

```bash
git add crates/wasmtime/src/runtime/vm/libcalls.rs crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "feat: promote live refs at persistent write boundaries"
```

Expected: commit succeeds with only runtime promotion files staged.

## Task 3: Rewriter TxRef And Copyability Tests

**Files:**
- Modify: `crates/transaction-tools/tests/kotlin_rewrite.rs`

- [ ] **Step 1: Add test helpers**

Near existing Kotlin sidecar helpers, add:

```rust
fn nullable_ref_field(name: &str, target: &str) -> KotlinField {
    ref_field(name, target, true)
}

fn copyable_struct(name: &str, fields: Vec<KotlinField>) -> KotlinCopyableType {
    KotlinCopyableType {
        name: name.into(),
        kind: KotlinPersistentKind::Struct,
        fields,
        element: None,
        source: KotlinCopyableSource::TxCopyable,
    }
}
```

Import `KotlinCopyableSource` and `KotlinCopyableType` from `wasmtime_transaction_tools::kotlin_metadata`.

- [ ] **Step 2: Add accepted TxRef string test**

Add:

```rust
#[test]
fn rewrite_accepts_txref_get_into_persistent_field_without_rewriting_constructor() {
    let input = wat::parse_str(
        r#"
        (module
          (type $String (struct (field (mut i32))))
          (type $TxRefString (struct (field (mut (ref null $String)))))
          (type $Bank (struct (field (mut (ref null $String)))))
          (func $"twasm.TxRef.get"
                (param $txref (ref $TxRefString))
                (result (ref null $String))
            local.get $txref
            struct.get $TxRefString 0)
          (func (export "publish")
                (param $txref (ref $TxRefString))
                (result (ref null $Bank))
            local.get $txref
            call $"twasm.TxRef.get"
            struct.new $Bank))
        "#,
    )
    .unwrap();
    let mut sidecar = sidecar(
        vec![struct_type_with_fields(
            "Bank",
            vec![nullable_ref_field("note", "kotlin.String")],
        )],
        &["publish"],
        vec![],
    );
    sidecar.gc_wasm.capture = KotlinGcWasmCapture::AllModuleGcTypes;

    let (output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    let printed = wasmprinter::print_bytes(&output).unwrap();

    assert_valid_module(&output);
    assert_eq!(report.rewritten_object_ops, 0, "{report:?}");
    assert!(printed.contains("struct.new"), "{printed}");
    assert!(!printed.contains("tstruct.new"), "{printed}");
}
```

Expected behavior: the rewriter accepts the explicit boundary but does not force eager transactional construction.

- [ ] **Step 3: Add direct outside ref rejection test**

Add:

```rust
#[test]
fn rewrite_rejects_direct_ordinary_ref_into_persistent_field() {
    let input = wat::parse_str(
        r#"
        (module
          (type $String (struct (field (mut i32))))
          (type $Bank (struct (field (mut (ref null $String)))))
          (func (export "publish") (result (ref null $Bank))
            i32.const 7
            struct.new $String
            struct.new $Bank))
        "#,
    )
    .unwrap();
    let mut sidecar = sidecar(
        vec![struct_type_with_fields(
            "Bank",
            vec![nullable_ref_field("note", "kotlin.String")],
        )],
        &["publish"],
        vec![],
    );
    sidecar.gc_wasm.capture = KotlinGcWasmCapture::AllModuleGcTypes;

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(
        err.contains("ordinary WasmGC reference enters persistent field Bank.note without make_txn_ref"),
        "{err}"
    );
}
```

- [ ] **Step 4: Add copyable DTO acceptance test**

Add a test where `Bank.note` points to `TransferNote`, and `TransferNote` has `text: kotlin.String` and `source: Account` fields. Build the sidecar with:

```rust
sidecar.copyable_types = vec![copyable_struct(
    "TransferNote",
    vec![
        nullable_ref_field("text", "kotlin.String"),
        nullable_ref_field("source", "Account"),
    ],
)];
sidecar.gc_wasm.capture = KotlinGcWasmCapture::AllModuleGcTypes;
```

Expected: `rewrite_kotlin_module` accepts `TxRef.get()` for `TransferNote` into `Bank.note`.

- [ ] **Step 5: Add denied type rejection test**

Add a test where `TransferNote` has a field with type `"kotlin.io.File"` and the sidecar contains:

```json
"gcWasm": {
  "capture": "allModuleGcTypes",
  "denyTypes": ["kotlin.io.File"]
}
```

Expected error substring:

```text
copyable type TransferNote field file references denied type kotlin.io.File
```

- [ ] **Step 6: Run new rewriter tests and verify failures before implementation**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite txref -- --nocapture
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite copyable -- --nocapture
```

Expected before Task 4: at least the new TxRef/copyable tests FAIL because rewriter provenance and copyability enforcement are not implemented.

- [ ] **Step 7: Commit failing rewriter tests**

Run:

```bash
git add crates/transaction-tools/tests/kotlin_rewrite.rs
git commit -m "test: cover Kotlin TxRef copyability boundaries"
```

Expected: commit succeeds with only rewriter tests staged.

## Task 4: Rewriter Provenance And Copyability Enforcement

**Files:**
- Modify: `crates/transaction-tools/src/kotlin/rewrite.rs`
- Test: `crates/transaction-tools/tests/kotlin_rewrite.rs`

- [ ] **Step 1: Add explicit persistent/copyable type sets**

Extend `GcTypeInfo` with:

```rust
explicit_persistent: PersistentTypeIndices,
copyable: PersistentTypeIndices,
copyable_type_indices: BTreeMap<String, u32>,
```

Populate `explicit_persistent` only from `sidecar.persistent_types`. Populate `copyable` from:

- all explicit persistent types,
- all `sidecar.copyable_types`,
- recognized built-ins such as `kotlin.String` when captured by `gcWasm.capture = "allModuleGcTypes"`.

Keep existing `persistent` as the runtime-layout set that includes captured module GC types.

- [ ] **Step 2: Add provenance types**

Near `FunctionSignature`, add:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefProvenance {
    NonRef,
    NullRef,
    OrdinaryRef,
    PersistentRef,
    TxnRefAllowed,
    TxnLocalCopyable,
    Parameter(u32),
    UnknownRef,
}
```

Add:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefCapabilityRequirement {
    PersistentReceiver,
    PersistentValue,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct FunctionRefRequirements {
    params: BTreeMap<u32, RefCapabilityRequirement>,
}

impl RefProvenance {
    fn can_enter_persistent_value(self) -> bool {
        matches!(
            self,
            RefProvenance::NullRef
                | RefProvenance::PersistentRef
                | RefProvenance::TxnRefAllowed
                | RefProvenance::TxnLocalCopyable
        )
    }

    fn can_be_persistent_receiver(self) -> bool {
        matches!(self, RefProvenance::PersistentRef | RefProvenance::NullRef)
    }
}
```

- [ ] **Step 3: Detect `TxRef.get`**

Add:

```rust
fn txref_get_function_indices(input: &[u8]) -> Result<BTreeSet<u32>> {
    function_indices_by_name(input, |name| {
        name == "twasm.TxRef.get"
            || name.ends_with(".TxRef.get")
            || name.ends_with("TxRef.<get-value>")
            || (name.contains("TxRef") && name.ends_with(".get"))
    })
}
```

- [ ] **Step 4: Add copyability helpers**

Add:

```rust
fn is_copyable_type(gc_type_info: &GcTypeInfo, type_index: u32) -> bool {
    gc_type_info.copyable.structs.contains(&type_index)
        || gc_type_info.copyable.arrays.contains(&type_index)
}

fn is_explicit_persistent_type(gc_type_info: &GcTypeInfo, type_index: u32) -> bool {
    gc_type_info.explicit_persistent.structs.contains(&type_index)
        || gc_type_info.explicit_persistent.arrays.contains(&type_index)
}
```

Use explicit persistent sets for source-level boundary checks. Use runtime `persistent` sets only for deciding whether existing object ops need transactional lowering.

- [ ] **Step 5: Validate source-level boundaries**

At explicit persistent boundaries, reject ref values unless provenance is `PersistentRef`, `TxnRefAllowed`, `TxnLocalCopyable`, or `NullRef`, and the value type is copyable.

Boundary sites:

- explicit persistent struct fields in `struct.new` and `struct.set`,
- explicit persistent array elements in `array.new`, `array.set`, `array.fill`,
- explicit persistent roots from root lowering,
- calls into helper functions whose inferred parameter requirements place a value into one of those boundaries.

Do not validate every edge inside generic captured runtime support types.

- [ ] **Step 6: Keep constructors ordinary**

Ensure this test still passes:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite rewrite_keeps_persistent_constructors_ordinary_until_promotion_boundary -- --nocapture
```

Expected: PASS. The output contains `struct.new`/`array.new` and does not contain `tstruct.new`/`tarray.new` solely because a type is persistent/copyable.

- [ ] **Step 7: Run rewriter tests**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite txref -- --nocapture
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite copyable -- --nocapture
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite -- --nocapture
```

Expected: all commands PASS.

- [ ] **Step 8: Commit rewriter implementation**

Run:

```bash
git add crates/transaction-tools/src/kotlin/rewrite.rs crates/transaction-tools/tests/kotlin_rewrite.rs
git commit -m "feat: enforce Kotlin TxRef copyability"
```

Expected: commit succeeds with only rewriter source and tests staged.

## Task 5: Kotlin Bank Example And Runtime Toolset Test

**Files:**
- Modify: `examples/transaction-kotlin/bank/src/wasmWasiMain/kotlin/Main.kt`
- Modify: `examples/transaction-kotlin/bank/twasm.kotlin.json`
- Modify: `tests/transaction_kotlin_toolset.rs`

- [ ] **Step 1: Update bank example source**

Use this Kotlin shape:

```kotlin
import twasm.Persistent
import twasm.TxnFunc
import twasm.get
import twasm.make_txn_ref
import twasm.root
import twasm.setRoot
import twasm.transaction

@Persistent
class Account(var balance: Long)

@Persistent
class Bank(
    var alice: Account,
    var bob: Account,
    var note: String
)

@TxnFunc
fun transfer(bank: Bank, fromAlice: Boolean, amount: Long, note: String) {
    if (fromAlice) {
        bank.alice.balance -= amount
        bank.bob.balance += amount
    } else {
        bank.bob.balance -= amount
        bank.alice.balance += amount
    }
    bank.note = note
}

@TxnFunc
fun installFreshBank(alice: Long, bob: Long, note: String) {
    val bank = Bank(Account(alice), Account(bob), note)
    setRoot("bank", bank)
}

fun main() {
    val note = make_txn_ref("this is a note")
    transaction {
        installFreshBank(1_000, 200, note.get())
        val bank = root<Bank>("bank")
        transfer(bank, true, 100, note.get())
    }
}
```

- [ ] **Step 2: Update bank sidecar**

Use this JSON:

```json
{
  "version": 1,
  "module": "transaction-kotlin-bank",
  "gcWasm": {
    "capture": "allModuleGcTypes",
    "denyTypes": [
      "kotlin.coroutines.*",
      "kotlin.Throwable",
      "kotlin.io.File"
    ]
  },
  "persistentTypes": [
    {
      "name": "Account",
      "kind": "struct",
      "fields": [
        { "name": "balance", "kind": "i64", "nullable": false }
      ]
    },
    {
      "name": "Bank",
      "kind": "struct",
      "fields": [
        { "name": "alice", "kind": "ref", "type": "Account", "nullable": true },
        { "name": "bob", "kind": "ref", "type": "Account", "nullable": true },
        { "name": "note", "kind": "ref", "type": "kotlin.String", "nullable": true }
      ]
    }
  ],
  "copyableTypes": [],
  "transactionFunctions": [
    "transfer",
    "installFreshBank"
  ],
  "roots": [
    { "name": "bank", "type": "Bank", "nullable": false }
  ]
}
```

- [ ] **Step 3: Update recovery assertions**

In `kotlin_rewritten_bank_executes_and_recovers_file_backed_roots`, read three `Bank` fields:

```rust
let alice_ref = recovered_object_ref_field(&root_winner.record_bytes, 3, 0)?
    .context("recovered Bank alice field was null")?;
let bob_ref = recovered_object_ref_field(&root_winner.record_bytes, 3, 1)?
    .context("recovered Bank bob field was null")?;
let note_ref = recovered_object_ref_field(&root_winner.record_bytes, 3, 2)?
    .context("recovered Bank note field was null")?;
```

After the existing `alice_ref != bob_ref` assertion, add:

```rust
ensure!(
    recovered
        .object_winners
        .iter()
        .any(|winner| winner.object_id == note_ref),
    "recovered Bank note ObjectId did not have a persistent record"
);
```

- [ ] **Step 4: Build Kotlin bank example**

Run:

```bash
cd examples/transaction-kotlin
gradle :bank:compileDevelopmentExecutableKotlinWasmWasi
```

Expected: `BUILD SUCCESSFUL`.

- [ ] **Step 5: Run Kotlin runtime test**

Run from repo root:

```bash
cargo test --test transaction_kotlin_toolset kotlin_rewritten_bank_executes_and_recovers_file_backed_roots --features transaction -- --nocapture
```

Expected: PASS, or SKIP only if the existing test prints the established Gradle-unavailable skip message.

- [ ] **Step 6: Commit bank updates**

Run:

```bash
git add examples/transaction-kotlin/bank/src/wasmWasiMain/kotlin/Main.kt \
  examples/transaction-kotlin/bank/twasm.kotlin.json \
  tests/transaction_kotlin_toolset.rs
git commit -m "test: verify Kotlin TxRef bank note recovery"
```

Expected: commit succeeds with only bank example and toolset-test changes staged.

## Task 6: Final Verification

**Files:**
- Verify all files changed by Tasks 1-5

- [ ] **Step 1: Format Rust**

Run:

```bash
cargo fmt --all
```

Expected: exits successfully.

- [ ] **Step 2: Run transaction-tools tests**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite -- --nocapture
cargo check -p wasmtime-transaction-tools
```

Expected: both commands PASS.

- [ ] **Step 3: Run runtime promotion tests**

Run:

```bash
cargo test -p wasmtime --lib persistent_promotion_commit --features transaction -- --nocapture
cargo test -p wasmtime --lib transaction_local_object --features transaction -- --nocapture
```

Expected: both PASS.

- [ ] **Step 4: Run Kotlin toolset tests**

Run:

```bash
cargo test --test transaction_kotlin_toolset --features transaction -- --nocapture
```

Expected: PASS, with existing Gradle skips allowed only when Gradle is unavailable.

- [ ] **Step 5: Check final diff**

Run:

```bash
git status --short
git log --oneline -6
```

Expected: working tree is clean except for intentionally uncommitted artifacts, and the recent commits are the task commits from this plan.
