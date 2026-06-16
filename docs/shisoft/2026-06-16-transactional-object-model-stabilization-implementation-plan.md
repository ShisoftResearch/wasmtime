# Transactional Object Model Stabilization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:subagent-driven-development` or `superpowers:executing-plans` to
> implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** Turn the transactional object model roadmap into executable waves
that stabilize persistent `ObjectId` identity before persistent GC storage
reclamation work starts.

**Architecture:** Keep ordinary Wasmtime GC as the volatile object system and
make persistent transactional object identity flow through `ObjectId`.
Durable object payloads encode only durable leaves and `ObjectId` edges.
Temporary WAST/live bridges remain explicit while normal file-backed
persistence becomes strict.

**Tech Stack:** Rust, Wasmtime runtime/libcalls, Cranelift lowering,
wasmparser/wasm-tools transaction fork, file-backed transaction persistence,
transaction WAST harness, transaction Rust SDK tests.

---

## Execution Rules

- Use test-first edits for behavior changes. Each wave starts by adding or
  tightening a test that demonstrates the missing invariant.
- Keep commits wave-sized. Commit messages must not contain `Co-authored-by`.
- Do not change transaction WAST files. Harness changes are allowed only when
  they make a compatibility boundary explicit.
- Keep `transaction` feature boundaries intact. New transaction or persistence
  code belongs under the transaction switch.
- Use `SHISOFT-TWASM-MOCK` only for explicit compatibility fallback seams.
  Normal runtime and file-backed persistence paths must not depend on those
  seams.

---

## Wave 0 Implementation Plan: Baseline Inventory And Gates

**Files:**

- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
- Modify: `docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md`

**Tests/Checks:**

```sh
rg -n "SHISOFT-TWASM-MOCK|ObjectValueAbi|PersistentObjectRefRaw|VMGcRef|VMFuncRef|DurableReferenceRegistry|transaction_permission" crates/wasmtime crates/cranelift crates/test-util docs/shisoft
cargo fmt --check
git diff --check
```

- [ ] **Step 0.1: Write the inventory section**

Add a dated section to
`docs/shisoft/transactional-wasm-implementation-log.md` with this structure:

```markdown
## 2026-06-16 Object Model Stabilization Baseline

- Persistent identity:
  - `ObjectId` is the stable identity for persistent transactional heap objects.
  - `PersistentObjectRefRaw` is the durable raw object-ref encoding.
- Temporary live ABI debt:
  - `ObjectValueAbi` still carries live reference values across helper
    boundaries.
  - Some live paths still map `VMGcRef` to `ObjectId` through `ObjectTable`.
- Strict persistence boundary:
  - File-backed persistence must reject unsupported durable refs before
    publication.
  - WAST-only live reference fallback remains explicitly opt-in.
- Remaining stabilization waves:
  - final live `ObjectId` tref ABI
  - process-local bridge cleanup
  - restart-stable function/external refs
  - object header/layout freeze
  - roots/recovery closure
  - permission hardening
  - `TRefType` deferral documentation
```

- [ ] **Step 0.2: Record exact bridge classification**

Fill the section with concrete file/function names using the `rg` output. Every
entry must be classified as one of:

```text
ordinary volatile GC use
commit-time promotion source
WAST-only compatibility fallback
object-model debt
```

- [ ] **Step 0.3: Update roadmap status**

In
`docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md`,
mark Wave 0 completed after the inventory is written.

- [ ] **Step 0.4: Verify**

Run:

```sh
cargo fmt --check
git diff --check
```

Expected: both commands exit 0.

- [ ] **Step 0.5: Commit**

```sh
git add docs/shisoft/transactional-wasm-implementation-log.md \
  docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md \
  docs/shisoft/2026-06-16-transactional-object-model-stabilization-implementation-plan.md
git commit -m "Document transactional object model stabilization plan"
```

---

## Wave 1 Implementation Plan: Final Live Persistent Reference ABI

**Files:**

- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/cranelift/src/func_environ.rs`
- Test: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `crates/wasmtime/tests/transaction_persistence.rs`

**Target invariant:** Persistent transactional object refs returned from
`tstruct.get`, `tarray.get`, `tglobal.get`, and `ttable.get` use an explicit
`ObjectId` live ABI. They do not require a `VMGcRef` mirror after recovery.

- [ ] **Step 1.1: Add failing unit tests for raw object-ref ABI**

Add tests near the existing `PersistentObjectRefRaw`/`ObjectValueAbi` tests in
`crates/wasmtime/src/runtime/transaction.rs`:

```rust
#[test]
fn persistent_object_ref_raw_roundtrips_null_and_non_null() {
    assert_eq!(
        PersistentObjectRefRaw::from_optional_object_id(None)
            .unwrap()
            .decode(),
        None
    );
    let object = ObjectId { object_index: 41 };
    let raw = PersistentObjectRefRaw::from_optional_object_id(Some(object)).unwrap();
    assert_eq!(raw.as_raw(), 42);
    assert_eq!(raw.decode(), Some(object));
}

#[test]
fn live_ref_abi_for_recovered_persistent_object_does_not_need_gc_ref() {
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_payload_with_persistence(
            ObjectPayload::Struct(vec![ObjectValue::I32(7)]),
            true,
        )
        .unwrap();

    let (raw, kind) = objects.live_ref_abi_for_object_id(object).unwrap();

    assert_eq!(kind, OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT);
    assert_eq!(
        PersistentObjectRefRaw::from_raw(raw).decode(),
        Some(object)
    );
}
```

Run:

```sh
cargo test -p wasmtime transaction::persistent_object_ref_raw_roundtrips_null_and_non_null transaction::live_ref_abi_for_recovered_persistent_object_does_not_need_gc_ref --lib
```

Expected before implementation: the second test fails if the helper still
requires a GC association or narrows incorrectly.

- [ ] **Step 1.2: Add failing file-backed recovery ABI test**

Add a test to `crates/wasmtime/tests/transaction_persistence.rs` that:

1. Publishes a persistent root containing a child object reference.
2. Reopens file-backed storage into a new store.
3. Reads the recovered object field through `tstruct.get`.
4. Confirms the child object reference is usable without constructing a live
   Wasmtime GC mirror.

Use the existing `real_tfunc_promotes_ordinary_ref_array_with_i31_leaf_and_child_object`
and `store_transaction_open_file_backed_storage_rebuilds_recovered_object_table`
tests as patterns.

Run:

```sh
cargo test -p wasmtime-tests recovered_object_ref_get_uses_object_id_abi --test transaction_persistence
```

Expected before implementation: fail at the recovered live-ref boundary if the
path still needs a `VMGcRef` mirror.

- [ ] **Step 1.3: Introduce a named live ref ABI helper**

In `crates/wasmtime/src/runtime/transaction.rs`, keep
`PersistentObjectRefRaw` as the durable object-ref raw type and add a small
named helper for live persistent refs:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LivePersistentObjectRefAbi {
    raw: PersistentObjectRefRaw,
}

impl LivePersistentObjectRefAbi {
    pub(crate) fn from_object_id(object_id: ObjectId) -> Result<Self> {
        Ok(Self {
            raw: PersistentObjectRefRaw::from_optional_object_id(Some(object_id))?,
        })
    }

    pub(crate) fn from_raw(raw: u64) -> Self {
        Self {
            raw: PersistentObjectRefRaw::from_raw(raw),
        }
    }

    pub(crate) fn as_raw(self) -> u64 {
        self.raw.as_raw()
    }

    pub(crate) fn decode(self) -> Option<ObjectId> {
        self.raw.decode()
    }
}
```

Use this helper anywhere the live high-word kind is
`OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT`.

- [ ] **Step 1.4: Remove unnecessary 32-bit object-ref narrowing**

Audit `ObjectTable::live_ref_abi_for_object_id` and libcall callers. The final
value path should carry the `u64` raw object id and high-word kind together.
Keep `u32` only where Wasmtime's ordinary GC ABI is actually being used.

- [ ] **Step 1.5: Update Cranelift lowering constants and comments**

In `crates/cranelift/src/func_environ.rs`, keep the ref ABI as the 24-byte
`ObjectValueAbi` tuple for now, but document that persistent object refs are
the `low: u64, high: PERSISTENT_OBJECT` case and not a GC ref.

- [ ] **Step 1.6: Verify**

Run:

```sh
cargo test -p wasmtime transaction:: --lib
cargo test -p wasmtime-tests transaction_persistence --test transaction_persistence
```

Expected: both commands exit 0.

- [ ] **Step 1.7: Commit**

```sh
git add crates/wasmtime/src/runtime/transaction.rs \
  crates/wasmtime/src/runtime/vm/libcalls.rs \
  crates/cranelift/src/func_environ.rs \
  crates/wasmtime/tests/transaction_persistence.rs \
  docs/shisoft/transactional-wasm-implementation-log.md \
  docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md
git commit -m "Stabilize persistent object reference ABI"
```

---

## Wave 2 Implementation Plan: Remove Long-Lived Process-Local Bridges

**Files:**

- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/test-util/src/wast.rs`
- Test: `crates/wasmtime/tests/transaction_persistence.rs`

**Target invariant:** `VMGcRef` side maps are promotion-only or WAST-only. A
normal recovered persistent object does not need `gc_ref_to_object` or
`object_to_gc_ref`.

- [ ] **Step 2.1: Add failing strict recovery regression**

In `crates/wasmtime/tests/transaction_persistence.rs`, add a test that reopens
a store, reads a recovered persistent root, and asserts the object table has no
GC association for that root while object operations still work.

Run:

```sh
cargo test -p wasmtime-tests recovered_persistent_root_has_no_gc_ref_association --test transaction_persistence
```

Expected before implementation: fail if the public helper API cannot observe
the absence of a GC association or if the access path requires one.

- [ ] **Step 2.2: Rename debt-bearing bridge APIs**

Rename methods whose names imply durable identity can come from `VMGcRef`:

```text
known_object_id_for_gc_ref -> known_object_id_for_live_gc_ref
known_persistent_object_id_for_gc_ref -> known_persistent_object_id_for_live_gc_ref
object_id_for_transaction_ref_raw -> object_id_for_live_transaction_ref_raw
```

Keep compatibility wrappers only if too many callers need a staged migration;
wrappers must be marked `#[cfg(test)]` or documented as temporary.

- [ ] **Step 2.3: Add strict conversion helper**

Add an object-table helper:

```rust
pub(crate) fn persistent_object_id_for_live_ref_abi(
    &self,
    low: u64,
    high: u64,
) -> Result<Option<ObjectId>>
```

Behavior:

- `high == OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT`: decode `low` as
  `PersistentObjectRefRaw`.
- `high == OBJECT_VALUE_ABI_LIVE_REF_KIND_GC`: use only the live GC side map.
- `high == OBJECT_VALUE_ABI_LIVE_REF_KIND_I31`: return `Ok(None)` because i31
  is inline.
- other recognized live ref kinds are handled by durable ref registries, not
  object table identity.

- [ ] **Step 2.4: Keep WAST fallback harness-gated**

Ensure the only code that enables live WAST reference fallbacks is test harness
or explicit test utility code. The runtime default must remain strict.

- [ ] **Step 2.5: Verify**

Run:

```sh
rg -n "SHISOFT-TWASM-MOCK|known_object_id_for_gc_ref|object_id_for_transaction_ref_raw" crates/wasmtime crates/test-util docs/shisoft
cargo test -p wasmtime transaction:: --lib
cargo test -p wasmtime-tests transaction_persistence --test transaction_persistence
```

Expected: old API names disappear or remain only in deliberate test shims.

- [ ] **Step 2.6: Commit**

```sh
git add crates/wasmtime/src/runtime/transaction.rs \
  crates/wasmtime/src/runtime/vm/libcalls.rs \
  crates/test-util/src/wast.rs \
  crates/wasmtime/tests/transaction_persistence.rs \
  docs/shisoft/transactional-wasm-implementation-log.md \
  docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md
git commit -m "Remove persistent object GC bridge assumptions"
```

---

## Wave 3 Implementation Plan: Restart-Stable Function And External References

**Files:**

- Modify: `crates/wasmtime/src/runtime/transaction/durable_ref.rs`
- Modify: `crates/wasmtime/src/runtime/store.rs`
- Modify: `crates/wasmtime/src/lib.rs`
- Test: `crates/wasmtime/tests/transaction_persistence.rs`

**Target invariant:** `tfuncref` and `texternref` stored in persistent objects
are inline durable leaves. They are not `ObjectId`s and are not object-table
entries.

- [ ] **Step 3.1: Add failing restart-resolution tests**

Extend `crates/wasmtime/tests/transaction_persistence.rs` with tests for:

- a registered exported `funcref` stored in a persistent struct, reopened, and
  resolved after registering the same module/function identity again
- an unregistered `funcref` rejected before commit
- a registered `externref` stored in a persistent array, reopened, and resolved
  after host registration
- an unregistered `externref` rejected before commit

Run:

```sh
cargo test -p wasmtime-tests durable_funcref_persistence durable_externref_persistence --test transaction_persistence
```

Expected before implementation: fail on whichever identity path is missing.

- [ ] **Step 3.2: Tighten fallback checks**

In `DurableReferenceRegistry`, keep
`enable_live_wast_reference_fallbacks_for_test` private to test/harness callers.
Normal runtime code must call `register_func_ref`, `register_extern_ref`, or
return an error before publication.

- [ ] **Step 3.3: Confirm inline payload encoding**

Verify `ObjectValue::FuncRef` and `ObjectValue::ExternRef` encode through
`ObjectValueAbi` tags `FUNCREF` and `EXTERNREF` and that
`object_heap::trace_object_ids` treats them as scalars.

- [ ] **Step 3.4: Add public internal test APIs only where needed**

Keep helper APIs under `_internal::transaction_persistence` for tests. Do not
expose durable reference registration as a stable public API yet.

- [ ] **Step 3.5: Verify**

Run:

```sh
cargo test -p wasmtime transaction::durable_ref --lib
cargo test -p wasmtime-tests transaction_persistence --test transaction_persistence
```

- [ ] **Step 3.6: Commit**

```sh
git add crates/wasmtime/src/runtime/transaction/durable_ref.rs \
  crates/wasmtime/src/runtime/store.rs \
  crates/wasmtime/src/lib.rs \
  crates/wasmtime/tests/transaction_persistence.rs \
  docs/shisoft/transactional-wasm-implementation-log.md \
  docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md
git commit -m "Stabilize durable transaction reference identities"
```

---

## Wave 4 Implementation Plan: Object Header, Payload, And Layout Freeze

**Files:**

- Modify: `crates/wasmtime/src/runtime/transaction/object_heap.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/type_layout.rs`
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Test: `crates/wasmtime/src/runtime/transaction/object_heap.rs`

**Target invariant:** Object records are traceable by type layout and contain
only durable values.

- [ ] **Step 4.1: Add failing decode/layout tests**

Add tests in `object_heap.rs` for:

- struct payload with two `ObjectId` fields traces both edges
- array payload with `ObjectId` elements traces all non-null edges
- `I31`, `FuncRef`, and `ExternRef` do not trace as object edges
- missing layout metadata causes a clear recovery error
- wrong layout kind for object kind causes a clear recovery error

Run:

```sh
cargo test -p wasmtime transaction::object_heap --lib
```

- [ ] **Step 4.2: Freeze and document header fields**

Update `transactional-wasm-runtime-core-design.md` to state that object record
headers contain:

```text
ObjectId
kind
type_layout_id
version
payload length
record location metadata
```

State that `ObjectId` is not reused until block retirement and generation rules
are implemented.

- [ ] **Step 4.3: Harden object heap decode errors**

Make decode functions include the object id, record version, and layout id in
errors when available.

- [ ] **Step 4.4: Verify**

Run:

```sh
cargo test -p wasmtime transaction::object_heap --lib
git diff --check
```

- [ ] **Step 4.5: Commit**

```sh
git add crates/wasmtime/src/runtime/transaction/object_heap.rs \
  crates/wasmtime/src/runtime/transaction/type_layout.rs \
  docs/shisoft/transactional-wasm-runtime-core-design.md \
  docs/shisoft/transactional-wasm-implementation-log.md \
  docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md
git commit -m "Freeze transactional object record layout invariants"
```

---

## Wave 5 Implementation Plan: Roots And Recovery Closure

**Files:**

- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/store.rs`
- Test: `crates/wasmtime/tests/transaction_persistence.rs`

**Target invariant:** Recovery rebuilds persistent object table entries by
tracing committed object winners from durable `tglobal` and `ttable` roots.

- [ ] **Step 5.1: Add failing root-closure tests**

Add file-backed tests for:

- reachable cycle through two persistent structs
- `ttable.fill` root publication and recovery
- `ttable.copy` root publication and recovery
- root replacement where the old object becomes unrecovered
- committed object with no root is not installed into the recovered object table

Run:

```sh
cargo test -p wasmtime-tests persistent_recovery_roots --test transaction_persistence
```

- [ ] **Step 5.2: Harden recovery closure**

Ensure `ObjectTable::rebuild_reachable_from_recovered_object_winners`:

- builds a temporary winner map by `ObjectId`
- marks from recovered root ids
- installs only reachable winners
- rejects dangling `ObjectId` edges with a clear error

- [ ] **Step 5.3: Verify**

Run:

```sh
cargo test -p wasmtime-tests transaction_persistence --test transaction_persistence
```

- [ ] **Step 5.4: Commit**

```sh
git add crates/wasmtime/src/runtime/transaction.rs \
  crates/wasmtime/src/runtime/store.rs \
  crates/wasmtime/tests/transaction_persistence.rs \
  docs/shisoft/transactional-wasm-implementation-log.md \
  docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md
git commit -m "Close persistent object root recovery"
```

---

## Wave 6 Implementation Plan: Permission And Transaction Semantics Hardening

**Files:**

- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Test: `crates/wasmtime/src/runtime/transaction.rs`

**Target invariant:** Transaction permissions are state attached to granules,
not reference-type identity. Object read/write permissions use
`GranuleId::Object(ObjectId)`.

- [x] **Step 6.1: Add failing object permission tests**

Add tests in `transaction.rs` for:

- read acquisition on a persistent object records `GranuleId::Object`
- write acquisition on a persistent object records `GranuleId::Object`
- conflicting write ownership fails for another transaction id
- abort releases object ownership
- trap/fail path aborts staged object writes
- generic transaction terminal paths reject object-cleanup cases unless the
  object-aware API receives an `ObjectTable`
- `ttable.set` stays private until commit
- `ttable.grow` exposes the privately grown region through staged table size
  and element overlays
- `ttable.grow` stays private to host-visible table size and element access
  until commit
- read/write-range helper bounds use staged table size for privately grown
  regions; bulk table data COW remains deferred

Run:

```sh
cargo test -p wasmtime lower_transaction_id_aborts_higher_suspended_object_writer --lib
cargo test -p wasmtime higher_transaction_id_cannot_write_lower_owned_object --lib
cargo test -p wasmtime transaction_object_existing_staged_write_rolls_back_on_tfail_and_trap --lib
cargo test -p wasmtime transaction_ttable_set_is_private_until_commit --lib
cargo test -p wasmtime transaction_ttable_grow_new_region_uses_staged_overlay --lib
cargo test -p wasmtime transaction_ttable_grow_is_private_until_commit --lib
cargo test -p wasmtime transaction_table_range_bounds_use_staged_size_for_private_grow --lib
cargo test -p wasmtime generic_abort_rejects_active_object_allocations_without_object_table --lib
cargo test -p wasmtime generic_commit_rejects_pending_conflict_aborted_object_allocations --lib
cargo test -p wasmtime generic_abort_transaction_rejects_suspended_object_allocations_without_object_table --lib
```

- [x] **Step 6.2: Route object helpers through object granules**

Audit `acquire_tref_read_for_gc_ref`, `acquire_tref_write_for_gc_ref`,
`stage_struct_field`, `stage_array_element`, and table/global root staging.
Every persistent object operation should derive the object id first and acquire
permission on `GranuleId::Object(object_id)`.

- [x] **Step 6.3: Verify**

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

- [ ] **Step 6.4: Commit**

```sh
git add crates/wasmtime/src/runtime/transaction.rs \
  crates/wasmtime/src/runtime/func.rs \
  crates/wasmtime/src/runtime/vm/libcalls.rs \
  docs/shisoft/transactional-wasm-implementation-log.md \
  docs/shisoft/2026-06-16-transactional-object-model-stabilization-implementation-plan.md \
  docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md \
  docs/shisoft/2026-06-16-wave6-object-permission-hardening-plan.md
git commit -m "Harden object transaction permissions"
```

---

## Wave 7 Implementation Plan: Type-System Cleanup Boundary

**Files:**

- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify: `docs/shisoft/transactional-wasm-remaining-work-roadmap.md`
- Modify: `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wasmparser/src/readers/core/types.rs`
- Modify: `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wasmparser/src/validator/types.rs`

**Target invariant:** Current permission metadata in `RefType` is documented as
temporary, and ordinary volatile refs stay permission-free.

- [ ] **Step 7.1: Add or tighten wasmparser tests**

In the wasm-tools transaction fork, add parser/validator tests proving ordinary
volatile refs have no transaction permission metadata, while transactional refs
carry permission only for validation/lowering.

Run:

```sh
cd /home/shisoft/Code/Research/wasm-tools-transaction
cargo test -p wasmparser transaction_permission_ref_type --lib
```

- [ ] **Step 7.2: Update design docs**

Add a section explaining:

- runtime permissions live on transaction state and `GranuleId`
- `RefType.transaction_permission` is a temporary carrier
- a future `TRefType` split can remove permission from ordinary `RefType`
- prerequisites for `TRefType`: final `ObjectId` live ABI, durable ref
  identities, and complete WAST/fuzz coverage

- [ ] **Step 7.3: Verify**

Run:

```sh
cd /home/shisoft/Code/Research/wasm-tools-transaction
cargo fmt --check
cargo check -p wasmparser --lib
cd /home/shisoft/Code/Research/wasmtime
cargo check -p wasmtime-fuzzing --lib
```

- [ ] **Step 7.4: Commit**

Commit the wasm-tools fork first, then this repository:

```sh
cd /home/shisoft/Code/Research/wasm-tools-transaction
git add crates/wasmparser/src/readers/core/types.rs \
  crates/wasmparser/src/validator/types.rs
git commit -m "Document transaction reference permission carrier"

cd /home/shisoft/Code/Research/wasmtime
git add docs/shisoft/transactional-wasm-runtime-core-design.md \
  docs/shisoft/transactional-wasm-remaining-work-roadmap.md \
  docs/shisoft/transactional-wasm-implementation-log.md \
  docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md
git commit -m "Document transaction reference type cleanup boundary"
```

---

## Wave 8 Implementation Plan: Stabilization Gate

**Files:**

- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
- Modify: `docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md`

**Target invariant:** The stabilized object model passes the same verification
gate used for the branch.

- [ ] **Step 8.1: Run Wasmtime checks**

```sh
cargo fmt --check
git diff --check
cargo check -p wasmtime-fuzzing --lib
WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_BUILD_JOBS=2 python3 ./ci/run-tests.py --locked --exclude=wasi-preview1-component-adapter -- --format terse
```

- [ ] **Step 8.2: Run wasm-tools fork checks**

```sh
cd /home/shisoft/Code/Research/wasm-tools-transaction
cargo fmt --check
git diff --check
cargo check -p wasm-smith --all-features --lib
cargo build --bin wasm-tools
```

- [ ] **Step 8.3: Build adapter with local wasm-tools**

```sh
cd /home/shisoft/Code/Research/wasmtime
PATH=/home/shisoft/Code/Research/wasm-tools-transaction/target/debug:$PATH ./ci/build-wasi-preview1-component-adapter.sh
```

- [ ] **Step 8.4: Update docs with final status**

Record exact pass/fail results and remaining GC-only work in
`docs/shisoft/transactional-wasm-implementation-log.md`.

- [ ] **Step 8.5: Commit**

```sh
git add docs/shisoft/transactional-wasm-implementation-log.md \
  docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md
git commit -m "Record transactional object model stabilization status"
```

---

## Self-Review Checklist

- [ ] Every wave starts with a concrete test or inventory check.
- [ ] Normal file-backed persistence remains strict.
- [ ] WAST fallback remains explicit and test-only.
- [ ] Persistent payloads store `ObjectId` edges and inline durable leaves only.
- [ ] `ti31`, `tfuncref`, and `texternref` are not object-table entries.
- [ ] Persistent GC reclamation and `ObjectId` reuse are still excluded from
      this roadmap.
