# Transactional Object Model Stabilization Roadmap

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:subagent-driven-development` or `superpowers:executing-plans` to
> implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** Stabilize the transactional persistent object model before starting
storage-reclaiming persistent GC work. After this roadmap, persistent
transactional objects should have a durable `ObjectId`-based runtime identity,
recover cleanly from file-backed storage, and avoid process-local GC references
in persistent object state.

**Architecture:** Ordinary Wasmtime GC remains the identity system for volatile
objects. Persistent transactional objects use `ObjectId` as runtime and durable
identity. Commit-time promotion copies supported volatile values into persistent
object storage and rewrites persistent references to durable encodings. Recovery
rebuilds the volatile object table from committed object/data records and
root records. Full persistent GC reclamation is intentionally outside this
roadmap; this roadmap prepares the object model that GC will trace later.

**Tech Stack:** Wasmtime runtime/libcalls, Cranelift Wasm lowering,
wasmparser/wasm-tools transaction extensions, file-backed transaction storage,
transaction WAST harness, transaction Rust SDK tests.

---

## Current Baseline

- [x] Transactional memory, globals, tables, and SIMD transactional memory paths
      have real runtime support and pass transaction WAST coverage.
- [x] Persistent object publication/recovery exists for transactional object
      records, roots, type/layout metadata, and reachable `tstruct`/`tarray`
      payloads.
- [x] Persistent linear memory has a file-backed undo-log path and mixed
      object/linear-memory transactions have file-backed tests.
- [x] Transaction permissions are enforced at transaction state/granule level.
- [x] A temporary type-system carrier,
      `RefType.transaction_permission`, exists to move permission metadata
      through existing parser/validator/lowering paths.
- [ ] Final live transactional reference ABI is not stable yet. Some helper and
      bridge paths still convert through process-local Wasmtime GC handles.
- [ ] Restart-stable `tfuncref` and `texternref` identity is only partially
      integrated.
- [ ] Durable block/chunk retirement and object storage reuse are not part of
      the stable object model yet.

---

## Stable Object Model Exit Criteria

- [ ] Persistent object references crossing transaction helper/libcall/lowering
      boundaries use a final `ObjectId`-carrying ABI, not raw `VMGcRef` or
      `VMFuncRef`.
- [ ] Raw `VMGcRef` is used only as an ordinary Wasmtime GC handle or as a
      short-lived commit-time promotion source.
- [ ] Persistent object payloads store durable values only:
      `ObjectId` edges, inline scalar values, inline `ti31` values, and
      restart-stable function/external reference identities.
- [ ] `tfuncref` and `texternref` either encode to restart-stable identities or
      fail the transaction before durable publication.
- [ ] Object headers, object record payloads, type/layout records, and root
      records have documented invariants sufficient for tracing and recovery.
- [ ] `tglobal` and `ttable` roots recover as durable object identities and use
      the same live ABI after reopening file-backed storage.
- [ ] Transaction permissions for object operations are keyed by
      `GranuleId::Object(ObjectId)` once the final live object ABI is active.
- [ ] `SHISOFT-TWASM-MOCK` paths remain only in explicit WAST compatibility
      fallback seams, not in normal runtime or file-backed persistence paths.
- [ ] Full transaction verification passes with no ignored transaction WAST
      cases.

---

## Wave 0: Baseline Inventory And Gates

**Purpose:** Make the current object-model seams explicit before changing ABI
or promotion paths.

- [x] Refresh the implementation log with the current object-model state.
- [x] List every remaining `SHISOFT-TWASM-MOCK` object/reference boundary.
- [x] List every remaining long-lived `VMGcRef`, `VMFuncRef`, and
      `ObjectValueAbi` bridge use in transaction object paths.
- [x] Classify each bridge as one of:
      ordinary volatile GC use, commit-time promotion source, WAST-only
      compatibility fallback, or object-model debt.
- [x] Record the baseline verification commands and their results.

**Verification:**

```sh
rg "SHISOFT-TWASM-MOCK|VMGcRef|VMFuncRef|ObjectValueAbi|PersistentObjectRefRaw|transaction_permission" crates docs
cargo fmt --check
git diff --check
```

---

## Wave 1: Final Live Persistent Reference ABI

**Purpose:** Make live transactional object references carry `ObjectId`
directly at the helper/libcall boundary.

- [ ] Define one runtime ABI representation for persistent object references.
      The existing `PersistentObjectRefRaw` shape is acceptable if it remains
      explicit: `0` means null and nonzero values encode `ObjectId`.
- [ ] Ensure all persistent `tstruct`/`tarray` helper paths accept and return
      the final object-reference ABI.
- [ ] Remove object-reference helper behavior that depends on a recovered live
      `VMGcRef` mapping.
- [ ] Keep ordinary Wasmtime `VMGcRef` values valid only at volatile GC and
      promotion-source boundaries.
- [ ] Update Cranelift lowering so transactional object refs lower to the final
      ABI shape consistently.
- [ ] Add unit tests for null, non-null, recovered, and cross-object reference
      roundtrips.

**Likely files:**

- `crates/wasmtime/src/runtime/transaction.rs`
- `crates/wasmtime/src/runtime/transaction/durable_ref.rs`
- `crates/wasmtime/src/runtime/vm/libcalls.rs`
- `crates/cranelift/src/func_environ.rs`
- `crates/cranelift/src/translate/code_translator.rs`

**Verification:**

```sh
cargo test -p wasmtime transaction:: --lib
cargo test -p wasmtime-tests transaction_persistence --test all
```

---

## Wave 2: Remove Long-Lived Process-Local Object Bridges

**Purpose:** Make process-local GC identity impossible to accidentally treat as
persistent identity.

- [ ] Rename or delete APIs that imply a raw `VMGcRef` can identify a
      persistent object after recovery.
- [ ] Add assertions in file-backed recovery paths that recovered object refs
      do not require live GC mappings.
- [ ] Make normal file-backed persistence strict by default: unsupported
      durable ref forms fail before publication.
- [ ] Keep WAST-only fallback behind an explicit harness flag and document the
      flag next to the code.
- [ ] Add a regression test that reopens storage and accesses recovered object
      refs without creating volatile GC mirror objects first.

**Verification:**

```sh
rg "VMGcRef.*ObjectId|ObjectId.*VMGcRef|SHISOFT-TWASM-MOCK" crates/wasmtime crates/test-util docs
cargo test -p wasmtime-tests transaction_persistence --test all
```

---

## Wave 3: Restart-Stable Function And External References

**Purpose:** Make `tfuncref` and `texternref` durable when stored inside
persistent objects.

- [ ] Define the durable function identity tuple used by persistent payloads.
      The tuple should include enough namespace information to survive restart:
      module identity, instance identity policy, and function index or export
      identity.
- [ ] Define the durable external reference identity API. Host code must
      register a stable namespace/key before a `texternref` can be committed.
- [ ] Encode durable function/external refs inside object payload records,
      without allocating standalone object-table entries for them.
- [ ] Reintegrate durable function/external refs during store reopen.
- [ ] Fail commit before publication when a function/external ref lacks a
      stable durable identity.
- [ ] Keep live-only WAST fallback explicit and separate from file-backed
      persistence.

**Verification:**

```sh
cargo test -p wasmtime transaction_durable_ref --lib
cargo test -p wasmtime-tests transaction_persistence --test all
```

---

## Wave 4: Object Header, Payload, And Layout Freeze

**Purpose:** Freeze the durable object-data shape enough for tracing, recovery,
and later GC.

- [ ] Document object header fields: `ObjectId`, object kind, type/layout id,
      payload length, version, and minimal record metadata.
- [ ] Document which values are traceable edges and which values are inline
      leaves.
- [ ] Ensure `ti31` is encoded inline as scalar object payload data, not as an
      object-table entry.
- [ ] Ensure `tstruct` and `tarray` records use layout metadata to trace fields
      and elements.
- [ ] Make decode fail clearly when a persistent object references missing or
      incompatible layout metadata.
- [ ] Add encode/decode tests for structs, arrays, nested object refs, inline
      `ti31`, function refs, and external refs.

**Verification:**

```sh
cargo test -p wasmtime transaction_object_layout --lib
cargo test -p wasmtime-tests transaction_persistence --test all
```

---

## Wave 5: Roots And Recovery Closure

**Purpose:** Make recovery reconstruct the persistent object table from roots
and committed object winners only.

- [ ] Verify `tglobal` roots publish and recover `ObjectId` references through
      the final live ABI.
- [ ] Verify `ttable` roots publish and recover `ObjectId` references through
      the final live ABI.
- [ ] Rebuild persistent object table entries by tracing reachable committed
      object winners from roots.
- [ ] Do not recover volatile-only object indices.
- [ ] Add tests for nested reachable objects, unreachable committed objects,
      root replacement, array element roots, table copy/fill/init roots, and
      mixed object/linear-memory transactions.
- [ ] Add a test with a reachable cycle so recovery cannot rely on recursive
      stack-only traversal.

**Verification:**

```sh
cargo test -p wasmtime-tests transaction_persistence --test all
WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_BUILD_JOBS=2 python3 ./ci/run-tests.py --locked --exclude=wasi-preview1-component-adapter -- --format terse
```

---

## Wave 6: Permission And Transaction Semantics Hardening

**Purpose:** Ensure object permissions follow the same transaction-state model
as memory/table/global granules.

- [ ] Route object read/write permission checks through
      `GranuleId::Object(ObjectId)`.
- [ ] Replace the current `GranuleId::TStruct` / `GranuleId::TArray` split with
      one object granule identity after updating tests to prove permissions no
      longer depend on object kind.
- [ ] Verify object writes acquire ownership according to the lock-based scheme.
- [ ] Verify abort, trap, and `tfail` release object ownership and discard
      staged object changes.
- [ ] Verify nested object reads and writes require an active transaction.
- [ ] Add conflict tests for two transaction ids touching the same object.
- [ ] Add mixed conflict tests where one transaction touches linear memory and
      persistent objects in the same commit.

**Verification:**

```sh
cargo test -p wasmtime transaction_permission --lib
cargo test -p wasmtime-tests transaction_concurrency --test all
```

---

## Wave 7: Type-System Cleanup Boundary

**Purpose:** Keep the temporary `RefType.transaction_permission` carrier honest
without expanding the type-system rewrite before the object ABI is stable.

- [ ] Update design docs to state that `RefType.transaction_permission` is a
      temporary parser/validator/lowering carrier, not runtime object state.
- [ ] Add a future `TRefType` migration note with prerequisites:
      final object ABI, stable durable ref identities, and complete WAST/fuzz
      coverage.
- [ ] Add regression checks that ordinary volatile refs have no transaction
      permission metadata.
- [ ] Avoid implementing the `TRefType` split in this roadmap.

**Verification:**

```sh
rg "transaction_permission|TRefType" crates docs
cargo check -p wasmtime-fuzzing --lib
```

---

## Wave 8: Stabilization Gate

**Purpose:** Confirm the object model is stable enough to start persistent GC
storage reclamation work.

- [ ] Run Wasmtime formatting and whitespace checks.
- [ ] Run wasm-tools fork formatting and workspace checks needed by the local
      transaction parser/generator dependencies.
- [ ] Run transaction unit tests, persistence tests, WAST tests, Rust SDK tests,
      and fuzzing crate checks.
- [ ] Build the WASI preview1 component adapter with the local transaction
      `wasm-tools` binary on `PATH`.
- [ ] Update the implementation log with completed waves and remaining GC-only
      work.
- [ ] Commit each completed wave without `Co-authored-by` annotations.

**Verification:**

```sh
cargo fmt --check
git diff --check
cargo check -p wasmtime-fuzzing --lib

(cd /home/shisoft/Code/Research/wasm-tools-transaction && cargo fmt --check)
(cd /home/shisoft/Code/Research/wasm-tools-transaction && git diff --check)
(cd /home/shisoft/Code/Research/wasm-tools-transaction && cargo check -p wasm-smith --all-features --lib)
(cd /home/shisoft/Code/Research/wasm-tools-transaction && cargo build --bin wasm-tools)

WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_BUILD_JOBS=2 python3 ./ci/run-tests.py --locked --exclude=wasi-preview1-component-adapter -- --format terse
PATH=/home/shisoft/Code/Research/wasm-tools-transaction/target/debug:$PATH ./ci/build-wasi-preview1-component-adapter.sh
```

---

## Deliberate Non-Goals

- [ ] Do not implement storage-reclaiming persistent GC in this roadmap.
- [ ] Do not reuse `ObjectId`s until durable block/chunk retirement and reuse
      rules are implemented.
- [ ] Do not persist volatile object-table implementation details.
- [ ] Do not introduce a full `TRefType` split before the final live
      `ObjectId` ABI and durable reference identities are stable.
- [ ] Do not make WAST compatibility fallback part of normal file-backed
      persistence semantics.
