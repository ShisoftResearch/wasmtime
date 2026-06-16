# Pre-GC Debt Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:subagent-driven-development` to implement this plan task by
> task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finish the remaining object-model debts that block persistent GC work:
remove live `VMGcRef` identity from t-prefixed object helper boundaries, keep
durable function/external references strict outside WAST fallbacks, and close the
object layout/root recovery tests and documentation.

**Architecture:** Until a true `TRefType` stack representation exists,
t-prefixed object refs remain 32-bit Wasm values, but those values become
transaction-owned object ref handles. The handle table maps live handles to
full-width `ObjectId` values and is rebuilt or regenerated after recovery; it is
not persistent identity. Durable records continue to store `ObjectId` through
`PersistentObjectRefRaw`.

**Tech Stack:** Wasmtime runtime/libcalls, Cranelift lowering, builtin libcall
signatures, transaction durable ref registry, file-backed recovery tests,
transaction WAST/Rust SDK gates.

## Completion Status

Status: complete for the pre-GC handle-based transactional object ABI.

Implemented:

- transaction-owned `u32` object ref handles for live t-prefixed helper
  boundaries;
- durable `ObjectId`/`PersistentObjectRefRaw` encoding for persistent object
  records;
- t-prefixed struct/array constructors returning transaction handles instead
  of ordinary `VMGcRef`s;
- t-prefixed object accessors, permission casts, and static initializers
  resolving through the object table;
- volatile runtime type metadata for transaction handles so `tref.cast`,
  `tref.test`, and branch-on-cast can avoid ordinary GC heap header reads;
- strict durable `tfuncref`/`texternref` payload reintegration through the
  durable reference registry;
- WAST-only transaction-ref sentinel conversion at Wasm/host boundaries, gated
  by `enable_live_wast_reference_fallbacks_for_test`.

Verified on 2026-06-16:

```text
cargo fmt --check
ok

git diff --check
ok

CARGO_TARGET_DIR=/tmp/wasmtime-pre-gc-debt-target CARGO_INCREMENTAL=0 cargo test -p wasmtime transaction --lib -- --format terse
test result: ok. 413 passed; 0 failed

CARGO_TARGET_DIR=/tmp/wasmtime-pre-gc-debt-target CARGO_INCREMENTAL=0 cargo test -p wasmtime --test transaction_persistence -- --format terse
test result: ok. 25 passed; 0 failed

CARGO_TARGET_DIR=/tmp/wasmtime-pre-gc-debt-target CARGO_INCREMENTAL=0 WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_BUILD_JOBS=2 python3 ./ci/run-tests.py --locked --exclude=wasi-preview1-component-adapter -- --format terse
exit code 0, including transaction WAST: 3641 passed; 0 failed

PATH=/home/shisoft/Code/Research/wasm-tools-transaction/target/debug:$PATH ./ci/build-wasi-preview1-component-adapter.sh
exit code 0
```

---

## File Responsibilities

- `crates/wasmtime/src/runtime/transaction.rs`: owns `ObjectId`,
  `PersistentObjectRefRaw`, transaction object ref handle allocation/resolution,
  object permissions, recovery/root tests, and object layout tests.
- `crates/wasmtime/src/runtime/vm/libcalls.rs`: converts live helper ABI values
  to/from `ObjectValue`, allocates transaction object refs, and enforces strict
  durable func/extern handling.
- `crates/environ/src/builtin.rs`: declares builtin signatures for t-prefixed
  object constructors and helpers.
- `crates/cranelift/src/func_environ.rs`: lowers t-prefixed object constructors,
  accessors, field payloads, and static initializers to the new helper ABI.
- `crates/cranelift/src/translate/code_translator.rs`: keeps transaction object
  operator comments and routing consistent.
- `docs/shisoft/2026-06-16-pre-gc-object-model-freeze-design.md`,
  `docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md`,
  and `docs/shisoft/transactional-wasm-implementation-log.md`: document the
  completed pre-GC shape and remaining non-blocking `TRefType` future work.

---

## Task 1: Transaction Object Ref Handle Runtime

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`

- [x] Add a failing unit test proving a recovered persistent `ObjectId` larger
      than `u32::MAX` can receive a live transaction object handle without
      narrowing the durable `ObjectId`.

Run:

```sh
cargo test -p wasmtime transaction_object_ref_handle_supports_full_width_object_id --lib
```

Expected: fail because no transaction object handle API exists yet.

- [x] Add `TransactionObjectRefRaw(u32)` with `0` as null, plus
      `ObjectTable` maps:
      `transaction_ref_handles_to_objects: BTreeMap<u32, ObjectId>`,
      `objects_to_transaction_ref_handles: BTreeMap<ObjectId, u32>`, and
      `next_transaction_ref_handle: u32`.

- [x] Implement:
      `transaction_ref_handle_for_object_id(&mut self, ObjectId) -> Result<u32>`,
      `object_id_for_transaction_ref_handle(u32) -> Result<ObjectId>`, and
      `known_persistent_object_id_for_transaction_ref_handle(u32)
      -> Result<Option<ObjectId>>`.
      These APIs must not inspect `VMGcRef` side maps.

- [x] Keep `PersistentObjectRefRaw` as the durable full-width encoding. Do not
      change object record/root storage.

- [x] Run the new test and existing focused object ABI tests.

```sh
cargo test -p wasmtime transaction_object_ref_handle_supports_full_width_object_id --lib
cargo test -p wasmtime persistent_object_value_abi_ref_uses_object_id_not_gc_ref_side_map --lib
cargo test -p wasmtime persistent_ref_abi --lib
```

Expected: all pass.

---

## Task 2: Live ObjectValueAbi Uses Transaction Handles, Not GC Refs

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`

- [x] Add a failing unit test showing
      `live_transaction_abi_from_object_value` returns
      `OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT` with a transaction
      handle that roundtrips to a full-width `ObjectId` and does not depend on
      `object_to_live_bridge_gc_ref`.

Run:

```sh
cargo test -p wasmtime object_value_abi_roundtrips_transaction_handle_for_large_object_id --lib
```

Expected: fail because live conversion still uses the old persistent raw bridge.

- [x] Change `live_transaction_abi_from_object_value` to take
      `&mut ObjectTable` and encode `ObjectValue::Ref(Some(_))` with
      `ObjectTable::transaction_ref_handle_for_object_id`.

- [x] Change explicit persistent live-ref decoding to resolve the low word as a
      transaction object ref handle. Keep `PersistentObjectRefRaw` for durable
      `ObjectValueAbi::from_object_value` only.

- [x] Retain WAST compatibility paths for `OBJECT_VALUE_ABI_LIVE_REF_KIND_GC`,
      `FUNC`, `EXTERN`, and `UNTYPED`, but ensure those paths are described as
      live fallback or promotion input.

- [x] Run:

```sh
cargo test -p wasmtime object_value_abi_roundtrips_transaction_handle_for_large_object_id --lib
cargo test -p wasmtime object_value_abi_roundtrips_recovered_persistent_object_ref_without_gc_ref --lib
cargo test -p wasmtime live_ref_bridge --lib
```

Expected: all pass after test updates.

---

## Task 3: T-Prefixed Constructors Return Transaction Handles

**Files:**
- Modify: `crates/environ/src/builtin.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/cranelift/src/func_environ.rs`
- Modify: `crates/cranelift/src/translate/code_translator.rs`

- [x] Add a failing runtime/lowering test proving `tstruct.new` and
      `tarray.new` execute without creating a live `VMGcRef` association and
      their returned refs can still be used by t-prefixed get/set/len helpers.

Run:

```sh
cargo test -p wasmtime transaction_object_constructors_return_transaction_ref_handles --lib
```

Expected: fail because constructors still associate Wasmtime GC refs.

- [x] Change t-prefixed object constructor builtins to return `u32`
      transaction ref handles and remove the `gc_ref` argument from constructor
      runtime paths where the constructor itself creates the transactional
      object.

- [x] In Cranelift lowering, stop calling ordinary `translate_struct_new` /
      `translate_array_new` for `TStruct*` / `TArray*` constructors. The
      t-prefixed constructor result is the transaction ref handle returned by
      the builtin. Continue to use ordinary constructors only for ordinary
      non-transactional `struct.*` / `array.*` operators.

- [x] Keep static module-initializer constructors persistent and return a
      transaction ref handle for subsequent static initializer uses.

- [x] Update t-prefixed set/get/fill/copy/init/len helpers to resolve receiver
      values through `object_id_for_transaction_ref_handle`.

- [x] Run:

```sh
cargo test -p wasmtime transaction_object_constructors_return_transaction_ref_handles --lib
cargo test -p wasmtime transaction_object_tstruct_executes_through_object_table --lib
cargo test -p wasmtime transaction_object_tarray_executes_through_object_table --lib
cargo test -p wasmtime transaction --lib
```

Expected: all pass.

---

## Task 4: TRef Permission Helpers Use Transaction Handles

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`

- [x] Add failing tests showing `tref.cast_read` and `tref.cast_write` grant
      permissions for transaction object ref handles and ignore ordinary live
      GC refs unless they are explicit WAST compatibility values.

Run:

```sh
cargo test -p wasmtime tref_cast_acquires_object_permission_for_transaction_handles --lib
```

Expected: fail because permission helpers still route through GC-ref naming.

- [x] Rename permission helpers from `*_for_gc_ref` to
      `*_for_transaction_ref_handle` and resolve through
      `known_persistent_object_id_for_transaction_ref_handle`.

- [x] Keep a clearly tagged WAST fallback helper only where proposal tests pass
      ordinary live refs through the existing compatibility path.

- [x] Run:

```sh
cargo test -p wasmtime tref_cast_acquires_object_permission_for_transaction_handles --lib
cargo test -p wasmtime tref_cast --lib
cargo test -p wasmtime model_permissions --lib
```

Expected: all pass.

---

## Task 5: Durable Function And External Reference Reintegration

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/durable_ref.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Test: transaction persistence tests in `crates/wasmtime/src/runtime/transaction.rs`

- [x] Add failing tests for file-backed reopen with durable `tfuncref` and
      `texternref` payload leaves. The strict path must preserve identities in
      object payloads and must not allocate object-table entries for them.

Run:

```sh
cargo test -p wasmtime durable_func_extern_refs_survive_object_payload_recovery --lib
```

Expected: fail for missing reopen reintegration or missing test API.

- [x] Ensure `DurableFuncIdentity` and `DurableExternIdentity` are sufficient
      durable tuple values in object payload records. Do not persist live
      `VMFuncRef` pointers or raw `VMGcRef` extern handles.

- [x] Add reopen-time registry hooks that can register host-provided durable
      func/extern mappings before decoded payloads are resolved to live helper
      values.

- [x] Keep `enable_live_wast_reference_fallbacks_for_test` as the only
      store-local fallback and document it as non-restart-stable.

- [x] Run:

```sh
cargo test -p wasmtime durable_ref --lib
cargo test -p wasmtime durable_func_extern_refs_survive_object_payload_recovery --lib
cargo test -p wasmtime transaction_persistence --lib
```

Expected: all pass.

---

## Task 6: Object Layout And Root Recovery Closure

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `docs/shisoft/2026-06-16-pre-gc-object-model-freeze-design.md`
- Modify: `docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [x] Add tests for reachable cycles, nested reachable objects, unreachable
      committed objects, root replacement, table element roots, and mixed
      object/linear-memory transactions after reopen.

Run:

```sh
cargo test -p wasmtime pre_gc_object_recovery_closure --lib
```

Expected: fail until the tests are implemented and any missing closure behavior
is fixed.

- [x] Ensure recovery rebuilds object table entries from committed root reachability
      and committed object winners only. Volatile-only object indices must not
      be recovered.

- [x] Update docs to mark Waves 1, 3 strict durable identity, 4, and 5 complete
      only for the pre-GC handle-based ABI. Leave true `TRefType` as future
      work, not a pre-GC blocker.

- [x] Run:

```sh
cargo test -p wasmtime pre_gc_object_recovery_closure --lib
cargo test -p wasmtime transaction --lib
```

Expected: all pass.

---

## Task 7: Final Verification And Commit

**Files:**
- All files touched above.

- [x] Run formatting and whitespace checks.

```sh
cargo fmt --check
git diff --check
```

- [x] Run focused and broad transaction verification.

```sh
CARGO_TARGET_DIR=/tmp/wasmtime-pre-gc-debt-target CARGO_INCREMENTAL=0 cargo test -p wasmtime transaction --lib
CARGO_TARGET_DIR=/tmp/wasmtime-pre-gc-debt-target CARGO_INCREMENTAL=0 WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_BUILD_JOBS=2 python3 ./ci/run-tests.py --locked --exclude=wasi-preview1-component-adapter -- --format terse
PATH=/home/shisoft/Code/Research/wasm-tools-transaction/target/debug:$PATH ./ci/build-wasi-preview1-component-adapter.sh
```

- [x] Run mock-boundary inventory.

```sh
rg -n "SHISOFT-TWASM-MOCK|VMGcRef|VMFuncRef|live_bridge|transaction_ref_handle|PersistentObjectRefRaw" crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction crates/wasmtime/src/runtime/vm/libcalls.rs crates/cranelift/src/func_environ.rs crates/cranelift/src/translate/code_translator.rs docs/shisoft
```

- [x] Commit with a human-only commit message:

```sh
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/durable_ref.rs crates/wasmtime/src/runtime/vm/libcalls.rs crates/environ/src/builtin.rs crates/cranelift/src/func_environ.rs crates/cranelift/src/translate/code_translator.rs docs/shisoft/2026-06-16-pre-gc-object-model-freeze-design.md docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md docs/shisoft/transactional-wasm-implementation-log.md docs/shisoft/2026-06-16-pre-gc-debt-completion-plan.md
git commit -m "Finish pre-GC transactional object model debts"
```

Expected: commit succeeds without `Co-authored-by` trailers.
