# Transactional Wasm Remaining Work Roadmap

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Advance the transactional Wasm research branch from the current
proposal-WAST-complete runtime core to a usable persistent-object runtime with
Zen-style recovery, durable object identity, and selectable backend/index
policies.

**Architecture:** Ordinary Wasmtime memory and ordinary Wasmtime GC remain
unchanged. Transactional state lives behind `GranuleId`, `LockBased`,
block/chunk-backed `tmemory` regions, and a persistent-object direction where
`ObjectId` is the runtime identity for persistent transactional objects. The
current branch now has full proposal-WAST coverage; the remaining work is to
replace volatile bridges with durable object records, Zen-style recovery that
rebuilds a volatile object index from persistent headers, persistent roots and
promotion, durable backends, and later an optional persistent-index mode for
faster restart.

**Tech Stack:** Rust, Wasmtime runtime internals, Cranelift translation hooks, local `wasm-tools-transaction` fork for `wasmparser`/`wast`, proposal WAST tests, `cargo test`.

---

## Current Baseline

Use the implementation log as the authoritative status file:

- `docs/shisoft/transactional-wasm-implementation-log.md`
- `docs/shisoft/transactional-wasm-runtime-core-design.md`

The current proposal-WAST branch baseline is:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 173 passed; 0 failed; 0 ignored
```

The older WAST-only roadmaps are now historical. This roadmap supersedes them
for post-WAST persistent-object and durable-backend sequencing.

Current real runtime foundations:

- `VMemory` block/chunk storage for `tmemory`.
- `FileBackedMemory` backend with shared writable mappings and explicit sync
  plumbing.
- `NVMemory` backend scaffold behind the same `tmemory` backend shape.
- COW workspace for scalar transactional memory.
- `LockBased` ownership over `GranuleId`.
- Numeric and v128 transactional globals for defined globals.
- Numeric imported transactional globals.
- Transactional SIMD memory load/store families.
- Funcref `ttable.get/set/size/grow/copy/init` smoke paths with ownership.
- Volatile `ObjectTable` foundation with dense `ObjectId`, slot versions,
  object-record publication, and staged struct/array payload helpers.

Current remaining mock categories:

- The runtime object path still uses volatile bridges for some Wasmtime GC and
  function-reference integration. Persistent transactional refs must end at
  `ObjectId`, not `VMGcRef` or `VMFuncRef`.
- Object records need durable publication metadata and recovery scanning so the
  volatile object index can be rebuilt after restart.
- Persistent roots and commit-time promotion need to publish only `ObjectId`
  edges into committed state.
- Table/global object snapshots still need the final `ObjectId`-based durable
  representation.
- `ttry`/`tfail` remains a later structured-failure workstream.
- Persistent-object GC remains future work after the durable object format and
  recovery path are stable.
- `FileBackedMemory` and `NVMemory` still need real recovery semantics; tests
  that require restart durability stay gated until hardware or restart harness
  support is ready.

## Ground Rules

- Do not edit proposal WAST files or test cases.
- Harness changes are allowed only to remove ignores, remove normalization, or
  route tests through real parser/runtime support.
- Wizard semantics are authoritative when proposal text and tests are
  ambiguous.
- `ObjectId` is the runtime identity for persistent transactional objects.
- `VMGcRef` remains ordinary volatile Wasmtime GC identity.
- Persistent payloads store `ObjectId` references, not raw `VMGcRef`s,
  process-local pointers, or PMEM addresses.
- The first object-index policy is Zen-style `RebuildOnRecovery`: object
  headers, object payloads, and durable roots are persisted; the runtime object
  index is rebuilt in DRAM during recovery.
- Persistent object-index maintenance is future work behind a separate policy.
  Do not add persistent-index writes to the first Zen-style path.
- Full persistent-object GC is a future workstream. Current object-runtime waves
  must make records traceable by `kind`, `type_index`, and `ObjectId` fields,
  but they do not need to reclaim persistent objects to pass proposal WAST.
- Reuse Wasmtime GC type/layout/cast/validation code where useful. Do not reuse
  `GcHeap`, `VMGcRef`, Wasmtime GC roots, or Wasmtime GC barriers as persistent
  object identity or storage.
- `VMemory` is sufficient for WAST completion. `FileBackedMemory` and
  `NVMemory` are important research backends but not blockers for all WAST.
- Each wave starts with focused failing unit tests or a currently ignored WAST
  file and ends with a recorded verification command.

## File Map

- `docs/shisoft/transactional-wasm-runtime-core-design.md`
  - Source design for transaction boundaries, block/chunk storage,
    `ObjectId`, promotion, permissions, and workstream order.
- `docs/shisoft/transactional-wasm-implementation-log.md`
  - Append verification results and mock removals after each wave.
- `crates/wasmtime/src/runtime/transaction.rs`
  - `TransactionState`, `GranuleId`, `LockBased`, `ObjectId`, the volatile
    recovery-built `ObjectTable`, staged payloads, and transaction COW APIs.
- `crates/wasmtime/src/runtime/vm/libcalls.rs`
  - Runtime entry points for transactional memory, globals, tables, refs, and
    structured failure.
- `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
  - `TMemory` frontend over transactional block/chunk storage.
- `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
  - Wizard-style volatile block/chunk backend.
- `crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs`
  - Linear virtual mapping layer for chunk-backed `tmemory`.
- `crates/environ/src/transaction.rs`
  - Transaction opcode and metadata decoding.
- `crates/environ/src/module.rs`
  - Module-level transaction metadata storage.
- `crates/environ/src/compile/module_environ.rs`
  - Translation of transaction object metadata into module environment.
- `crates/cranelift/src/translate/code_translator.rs`
  - Operator-level lowering for transaction-prefixed instructions.
- `crates/cranelift/src/func_environ.rs`
  - Libcall routing, table/global helpers, and lowering glue.
- `crates/test-util/src/wast.rs`
  - Proposal WAST discovery, ignore policy, normalization adapter, and
    path-scoped mock replacement registry.
- `/home/shisoft/Code/Research/wasm-tools-transaction`
  - Local `wasmparser`/`wast` fork for proposal text and binary parser support.
- `/home/shisoft/Code/Research/wizard-engine`
  - Reference semantics on branch `transactions`.

## Current Direction: Zen-First Persistent Objects

This section supersedes older persistent-object-table assumptions in the
historical waves below.

### Policy

The runtime should expose object-index durability as a policy:

```rust
enum ObjectIndexPersistencePolicy {
    RebuildOnRecovery,
    PersistentIndex,
}
```

Only `RebuildOnRecovery` is implemented in this phase.

### Persistent State

- object heap records with explicit durable headers
- `ObjectId` stored in each persistent object header
- payload bytes containing only persistent scalars/immediates and `ObjectId`
  edges
- durable roots
- publication metadata sufficient to identify the latest committed record for
  each `ObjectId`
- backend allocation metadata needed to discover committed object records during
  recovery

### Recovered Volatile State

- `ObjectId -> current record` index
- lock ownership tables
- free lists and allocation cursors
- temporary Wasmtime-bridge maps needed while the final `ObjectId` ABI is still
  being wired through all runtime paths

### Post-WAST Workstreams

1. **Durable object headers and publication metadata**
   - make `TxObjectHeader` the canonical durable identity carrier
   - add commit/publication fields needed for Zen-style visibility and recovery
   - ensure new committed object records never require a separately persisted
     object-index update

2. **Recovery-rebuilt object index**
   - scan persistent object storage on restart
   - rebuild the volatile `ObjectTable` from committed object headers
   - select the latest committed record for each `ObjectId`
   - regenerate allocation/free metadata in DRAM where possible

3. **Persistent roots and promotion**
   - make committed `tglobal`, `ttable`, and future explicit root sets publish
     `ObjectId` edges only
   - promote volatile Wasmtime objects during commit when they become reachable
     from persistent state

4. **Remove remaining volatile reference bridges**
   - replace `VMGcRef`/`VMFuncRef` scaffolding in transactional object and
     function-reference paths with real `ObjectId` runtime values
   - keep Wasmtime GC/type machinery only as layout/type knowledge

5. **Durable backend recovery**
   - finish FileBackedMemory restart recovery using sync-based durability
   - finish NVMemory publication and recovery semantics with CLWB/SFENCE-shaped
     plumbing

6. **Future optional persistent index**
   - evaluate whether Eliot's faster-restart requirement justifies persisting an
     object index
   - if needed, implement `PersistentIndex` as a second policy over the same
     durable object-header/object-payload format

The historical waves below remain useful for file ownership and older WAST
milestones, but when they conflict with this section, this section wins.

## Wave 0: Refresh The Baseline And Ledger

**Purpose:** make the next implementation wave start from measured state, not
from the older WAST roadmap whose counts have drifted.

**Files:**

- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
- Modify: `docs/shisoft/transactional-wasm-wast-ledger.md`
- Inspect: `crates/test-util/src/wast.rs`

- [ ] **Step 1: Run the current proposal baseline**

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```

Expected: all executed tests pass; ignored test count is nonzero until the
object/ref and structured failure waves complete.

- [ ] **Step 2: Capture the ignored-file list**

Run:

```bash
rg -n "ignored|transaction_proposal_adapter_mock|SIMPLE_TRANSACTION_REAL_TEXT_CORE|TRANSACTION_PROPOSAL" crates/test-util/src/wast.rs
```

Expected: the output identifies path-scoped replacements, real-parser
allowlists, and ignored proposal files.

- [ ] **Step 3: Update the ledger**

Record the command output summary in
`docs/shisoft/transactional-wasm-implementation-log.md` and update the WAST
ledger with the exact ignored filenames.

- [ ] **Step 4: Commit**

Run:

```bash
git diff --check -- docs/shisoft/transactional-wasm-implementation-log.md docs/shisoft/transactional-wasm-wast-ledger.md
git add docs/shisoft/transactional-wasm-implementation-log.md docs/shisoft/transactional-wasm-wast-ledger.md
git commit -m "Refresh transactional Wasm WAST baseline"
```

## Wave 1: Persistent Object Runtime Shape

**Purpose:** turn the existing volatile `ObjectTable` foundation into the
runtime shape needed by persistent `ObjectId` refs, while still using volatile
`VMemory`. This wave does not implement persistent GC, but it makes object
records traceable without redesign.

**Files:**

- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Create: `crates/wasmtime/src/runtime/transaction/object_heap.rs`
- Modify: `crates/wasmtime/src/runtime/mod.rs`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [x] **Step 1: Add object-record header tests**

Add tests under `crates/wasmtime/src/runtime/transaction.rs` or the new
`object_heap.rs` test module for:

- `TxObjectHeader` stores `record_len`, `object_id`, `kind`, `flags`, and
  `type_index`.
- `TxArrayHeader` embeds `TxObjectHeader` and stores `length`.
- volatile object-index version changes when a new object record is published.
- `ObjectId` remains stable while the current record address changes.
- struct and array payload records can report the `ObjectId` fields they contain
  using Wasmtime-derived type/layout metadata.

Run:

```bash
cargo test -p wasmtime --lib tx_object_header -- --format terse
cargo test -p wasmtime --lib object_table_publish -- --format terse
```

Expected: tests fail before the object-record layer exists.

- [x] **Step 2: Implement volatile object records**

Add a volatile object heap layer with these responsibilities:

- allocate record bytes from the existing `VMemory` block-region substrate
- write the explicit header
- store payload bytes separately from `ObjectTableSlot`
- return a record handle/address that the volatile object index can publish
- keep slot ownership and versioning in `ObjectTable`, not in the record header
- store enough layout metadata to later trace embedded `ObjectId` references
- avoid depending on `GcHeap` storage, `VMGcRef` identity, or Wasmtime GC roots

- [x] **Step 3: Move committed payloads behind record publication**

Change `ObjectTableSlot` from owning `ObjectPayload` directly to owning current
record metadata. Keep the existing public helpers:

- `allocate_struct`
- `allocate_array`
- `payload`
- `update_payload`
- `granule_id`
- `version`

- [x] **Step 4: Add trace-descriptor helpers**

Add helpers that identify reference-bearing payload slots without tracing them
yet:

- struct field descriptor: field index, field offset, field kind
- array descriptor: element kind and length
- object payload scanner returning embedded `ObjectId`s

Use Wasmtime shared type/layout metadata where available. If a layout cannot be
decoded yet, return an explicit unsupported-object-layout error rather than
storing an untraceable committed payload.

Expected behavior stays the same for current unit tests, but the storage shape
now matches the persistent design and can support a later `ObjectId` collector.

- [x] **Step 5: Verify and commit**

Run:

```bash
cargo test -p wasmtime --lib transaction -- --format terse
git diff --check -- crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_heap.rs
```

Commit:

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_heap.rs crates/wasmtime/src/runtime/mod.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Add transactional object record heap"
```

## Wave 2: ObjectId-Carrying Transactional References

**Purpose:** remove the ref-control path-scoped mocks by making transactional
refs carry `ObjectId` semantics instead of relying on fixture replacement.

**Files:**

- Modify: `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wasmparser/src/readers/core/operators.rs`
- Modify: `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wast/src/core/*.rs`
- Modify: `crates/environ/src/transaction.rs`
- Modify: `crates/cranelift/src/translate/code_translator.rs`
- Modify: `crates/cranelift/src/func_environ.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/test-util/src/wast.rs`

- [ ] **Step 1: Add parser tests for ref-control instructions**

Add or extend parser tests in the local `wasm-tools-transaction` fork for:

- `tref.null`
- `tref.is_null`
- `tref.as_non_null`
- `br_on_tnull`
- `br_on_tnon_null`
- `tcall_ref`
- `return_tcall_ref`

Run the relevant local fork tests, then run Wasmtime parser-facing tests:

```bash
cargo test -p wasmtime-environ transaction -- --format terse
```

Expected: newly added parser tests pass in the fork; Wasmtime still fails on
runtime/lowering until the following steps land.

- [ ] **Step 2: Define the transitional runtime representation**

Use an internal representation that can cross Cranelift/libcall boundaries now
while preserving the design target:

```text
null tref      = zero
nonnull tref   = encoded ObjectId
persistent ref = ObjectId in persistent object identity space
ordinary ref   = not accepted as persistent tref unless promotion occurs during commit
```

Add checked conversions between raw libcall values and `Option<ObjectId>`.

- [ ] **Step 3: Lower non-mutating ref-control operators**

Route the operators through runtime helpers or direct lowering:

- `tref.null`: produce encoded null
- `tref.is_null`: compare against encoded null
- `tref.as_non_null`: trap with Wasmtime-style null-reference diagnostic
- `br_on_tnull`: branch on encoded null
- `br_on_tnon_null`: branch and pass non-null encoded `ObjectId`

Progress: `tref.as_non_null`, `br_on_tnull`, and `br_on_tnon_null` now bypass
the path-scoped fixture replacement and execute through the real text parser
and Wasmtime's existing reference lowering. They still use ordinary Wasmtime
reference values, not the final encoded persistent `ObjectId` representation.

- [x] **Step 4: Lower transactional function refs**

Implement enough `tcall_ref` and `return_tcall_ref` to remove the path-scoped
fixture replacements. The first implementation may support transaction function
refs only, with validation rejecting ordinary function refs.

Progress: `tcall_ref` and `return_tcall_ref` now bypass the path-scoped fixture
replacement and execute through the real text parser plus Wasmtime's existing
`call_ref`/`return_call_ref` lowering. They still use ordinary Wasmtime function
references, not the final persistent `ObjectId` function-reference encoding.

- [x] **Step 5: Remove the path-scoped ref-control replacements**

Delete the replacement entries and their tests for:

- [x] `tcall_ref.wast`
- [x] `return_tcall_ref.wast`
- [x] `br_on_tnon_null.wast`
- [x] `br_on_tnull.wast`
- [x] `tref_as_non_null.wast`

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tcall_ref.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/return_tcall_ref.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/br_on_tnon_null.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/br_on_tnull.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tref_as_non_null.wast -- --format terse
```

Expected: these files run through the real parser and real engine path.

- [ ] **Step 6: Commit**

Run:

```bash
rg "transaction_proposal_adapter_mock|SHISOFT-TWASM-MOCK" crates/test-util/src/wast.rs crates/wasmtime/src/runtime/vm/libcalls.rs crates/cranelift/src
git diff --check
git add crates/test-util/src/wast.rs crates/environ/src/transaction.rs crates/cranelift/src/translate/code_translator.rs crates/cranelift/src/func_environ.rs crates/wasmtime/src/runtime/vm/libcalls.rs
git commit -m "Route transactional refs through ObjectId runtime values"
```

## Wave 3: Transactional Structs, Arrays, I31, And Externs

**Purpose:** make object-model WAST files execute against `ObjectId` slots and
object COW rather than the normalization adapter.

**Files:**

- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/object_heap.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/cranelift/src/translate/code_translator.rs`
- Modify: `crates/cranelift/src/func_environ.rs`
- Modify: `crates/environ/src/transaction.rs`
- Modify: `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wasmparser/src/readers/core/operators.rs`
- Modify: `crates/test-util/src/wast.rs`

- [x] **Step 1: Add object COW unit tests**

Add unit tests for:

- `tstruct.new` allocates a committed `ObjectId` with struct kind.
- `tstruct.get` reads staged payload first, then committed record.
- `tstruct.set` stages a whole-object payload and does not mutate committed
  state until commit.
- abort drops staged struct payloads.
- array `get/set/len/fill/copy` obeys whole-object granule ownership.
- `ti31` encodes immediate integer payloads without allocating a heap record
  unless the proposal test requires persistent object identity.
- unsupported `textern` promotion aborts the transaction with a clear
  Wasmtime-style error.

Run:

```bash
cargo test -p wasmtime --lib transaction_object -- --format terse
```

Status: complete for the runtime core foundation. The current test run covers
store-owned `ObjectTable`, struct field staged-before-committed reads, staged
struct abort, array len/get/set/fill/copy COW, immediate `ti31`, and unsupported
`textern` promotion abort:

```text
cargo test -p wasmtime --lib transaction_object -- --format terse
test result: ok. 15 passed; 0 failed; 0 ignored
```

- [ ] **Step 2: Implement struct and array libcalls**

Add runtime helpers for:

- create struct/array payloads
- read field/element
- write field/element through staged payloads
- read array length
- fill/copy/init array ranges
- validate object kind on every operation
- acquire `TStruct` or `TArray` read/write permission before payload access

Progress: runtime-level helpers now exist on `TransactionState` for struct
field reads/writes, array len/element/fill/copy operations, whole-object staged
payload publication, and store-owned `ObjectTable` access. Exported object
libcalls and Cranelift lowering are still pending.

- [ ] **Step 3: Implement `ti31` and `textern` operations**

Keep `ti31` simple and Wasmtime-compatible. Use `ObjectId` only where the
transactional object model requires persistent object identity. For `textern`,
support null and cast/test behavior first; abort promotion for unsupported
host-only extern objects.

Progress: `ti31` has an immediate 31-bit runtime value helper that requires an
active transaction and does not allocate an object record. Unsupported
persistent `textern` promotion aborts the active transaction with a clear
runtime error. Full `textern` null/cast/test behavior remains pending.

- [ ] **Step 4: Lower object operators**

Route parser operators to Cranelift lowering and then to the runtime helpers.
Use existing Wasmtime GC type/layout metadata for validation where possible,
but do not store `VMGcRef` inside persistent payloads.

Progress: the local `wasm-tools-transaction` fork now emits distinct
`0xfa 0xfb` parser operators for `tstruct`, `tarray`, `ti31`, and
`textern` object operations, and Wasmtime's reachable Cranelift translator has
explicit arms for those variants. The current Wasmtime arms are intentionally
tagged `SHISOFT-TWASM-MOCK` because they bridge to ordinary volatile Wasmtime
GC lowering. The fork also now emits a data-count section for transactional
array data operators. The next part of this step is to replace the volatile-GC
bridge with `ObjectId` COW object libcalls.

- [ ] **Step 5: Move object WAST files to the real path**

Run focused files first:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tstruct.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tarray.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/ti31.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/textern.wast -- --format terse
```

Then run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
```

Expected: object WAST files no longer require normalization or ignore entries.

- [ ] **Step 6: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_heap.rs crates/wasmtime/src/runtime/vm/libcalls.rs crates/cranelift/src/translate/code_translator.rs crates/cranelift/src/func_environ.rs crates/environ/src/transaction.rs crates/test-util/src/wast.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Implement transactional object operators"
```

## Wave 4: Reference Casts, Branches, And Type Validation

**Purpose:** finish transaction reference typing after the basic object
operators exist.

**Files:**

- Modify: `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wasmparser/src/readers/core/operators.rs`
- Modify: `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wast/src/core/*.rs`
- Modify: `crates/environ/src/transaction.rs`
- Modify: `crates/environ/src/compile/module_environ.rs`
- Modify: `crates/cranelift/src/translate/code_translator.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/test-util/src/wast.rs`

- [ ] **Step 1: Add validation tests**

Add tests for Wizard permission subtyping:

```text
write <= read <= none
none is not usable where read is required
read is not usable where write is required
ordinary ref is not usable where persistent tref is required
```

Run:

```bash
cargo test -p wasmtime-environ transaction_object_metadata -- --format terse
cargo test -p wasmtime-environ transaction_ref -- --format terse
```

- [ ] **Step 2: Implement casts and tests**

Implement:

- `tref.eq`
- `tref.test`
- `tref.cast`
- `br_on_tcast`
- `br_on_tcast_fail`
- nullable and non-null typed transactional refs
- recursive type, subtyping, canonicalization, and equivalence metadata needed
  by the WAST files

- [ ] **Step 3: Run type and cast WAST files**

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tref_eq.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tref_test.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tref_cast.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/br_on_tcast.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/ttype-rec.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/ttype-subtyping.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/ttype-canon.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/ttype-equivalence.wast -- --format terse
```

- [ ] **Step 4: Commit**

Run:

```bash
git diff --check
git add crates/environ/src/transaction.rs crates/environ/src/compile/module_environ.rs crates/cranelift/src/translate/code_translator.rs crates/wasmtime/src/runtime/vm/libcalls.rs crates/test-util/src/wast.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Validate transactional reference types"
```

## Wave 5: Reference-Valued Globals And Table COW

**Purpose:** remove the remaining reference/object table and global scaffolds
after `ObjectId` refs are real.

**Files:**

- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/cranelift/src/func_environ.rs`
- Modify: `crates/cranelift/src/translate/code_translator.rs`
- Modify: `crates/test-util/src/wast.rs`

- [ ] **Step 1: Add global snapshot tests**

Add tests that stage and commit:

- nullable `tref` global
- non-null persistent object `tref` global
- imported reference-valued `tglobal`
- abort of staged reference global

Run:

```bash
cargo test -p wasmtime --lib transaction_global_ref -- --format terse
```

- [ ] **Step 2: Replace raw reference snapshots**

Replace `GlobalSnapshot::GcRef(u32)` raw-word handling with a rooted or
`ObjectId`-based representation that cannot outlive the transaction unsafely.
Committed persistent globals store `ObjectId` refs.

- [ ] **Step 3: Add table COW tests**

Add tests that:

- `ttable.set` stages an element and abort discards it.
- `ttable.fill` stages all touched 16-element granules.
- `ttable.copy` reads staged source elements before committed elements.
- `ttable.init` stages initialized elements and `telem.drop` follows the
  proposal-visible transaction behavior.

Run:

```bash
cargo test -p wasmtime --lib transaction_table_cow -- --format terse
```

- [ ] **Step 4: Implement staged table elements**

Add a staged table workspace keyed by `TTable { instance, table_index,
granule_index }`. Table reads merge staged and committed values, matching
`tmemory` overlay semantics.

- [ ] **Step 5: Run table/global WAST files**

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tglobal.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/ttable.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/ttable-sub.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tbulk.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/telem.wast -- --format terse
```

- [ ] **Step 6: Commit**

Run:

```bash
rg "SHISOFT-TWASM-MOCK" crates/wasmtime/src/runtime/vm/libcalls.rs crates/cranelift/src/func_environ.rs crates/test-util/src/wast.rs
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/libcalls.rs crates/cranelift/src/func_environ.rs crates/cranelift/src/translate/code_translator.rs crates/test-util/src/wast.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Stage reference globals and transactional table elements"
```

## Wave 6: Structured `ttry` And `tfail`

**Purpose:** replace the `ttry-basic.wast` path-scoped replacement with real
structured transaction failure semantics.

**Files:**

- Modify: `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wasmparser/src/readers/core/operators.rs`
- Modify: `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wast/src/core/*.rs`
- Modify: `crates/environ/src/transaction.rs`
- Modify: `crates/cranelift/src/translate/code_translator.rs`
- Modify: `crates/cranelift/src/func_environ.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/test-util/src/wast.rs`

- [ ] **Step 1: Copy Wizard semantics into a focused design note**

Inspect Wizard's `ttry`/`tfail` implementation on branch `transactions` and add
a short section to `docs/shisoft/transactional-wasm-runtime-core-design.md`
covering:

- failure payload type
- handler stack behavior
- whether `tfail` aborts immediately or transfers to a handler that owns abort
- commit behavior after handled failure
- interaction with traps

- [ ] **Step 2: Add transaction-state tests**

Add tests for:

- `tfail` aborts staged memory/object/table/global writes.
- handled `tfail` resumes at the `ttry` handler with the failure value.
- unhandled `tfail` aborts the active transaction and reports a trap-like
  failure to the host.
- normal completion of `ttry` commits when the outer transaction commits.

Run:

```bash
cargo test -p wasmtime --lib transaction_ttry -- --format terse
cargo test -p wasmtime --lib transaction_tfail -- --format terse
```

- [ ] **Step 3: Implement parser and lowering**

Implement real text and binary parsing for the structured operators, then lower
to Wasmtime control flow. Keep transaction abort centralized in
`TransactionState` so all staged domains are discarded consistently.

- [ ] **Step 4: Remove the path-scoped `ttry-basic` replacement**

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/ttry-basic.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/ttry-abort-commit.wast -- --format terse
```

- [x] **Step 5: Commit**

Run:

```bash
git diff --check
git add docs/shisoft/transactional-wasm-runtime-core-design.md crates/environ/src/transaction.rs crates/cranelift/src/translate/code_translator.rs crates/cranelift/src/func_environ.rs crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/libcalls.rs crates/test-util/src/wast.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Implement structured transaction failure"
```

## Wave 7: Conflict Harness And Multi-Transaction Semantics

**Purpose:** make conflict WAST files verify real `LockBased` behavior instead
of synchronous smoke helpers.

**Files:**

- Modify: `crates/wast/src/spectest.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/test-util/src/wast.rs`

- [ ] **Step 1: Add LockBased conflict tests**

Add tests for:

- two transaction ids cannot write the same granule concurrently
- write after another transaction's optimistic read invalidates the reader
- abort releases owned object/table/memory granules
- no automatic retry occurs after conflict

Run:

```bash
cargo test -p wasmtime --lib lockbased_conflict -- --format terse
```

- [ ] **Step 2: Replace spectest transaction smoke helpers**

Make proposal spectest helpers operate on real transaction ids and the store's
active transaction state. Keep deterministic single-threaded scheduling for
WAST, but make reads/writes pass through the same `GranuleId` ownership layer as
compiled code.

- [ ] **Step 3: Run conflict WAST files**

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tconflict-basic.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tconflict-tmemory.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tconflict-tmemory_1.wast -- --format terse
```

- [ ] **Step 4: Commit**

Run:

```bash
git diff --check
git add crates/wast/src/spectest.rs crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/libcalls.rs crates/test-util/src/wast.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Run transaction conflict tests on LockBased state"
```

## Future Loom Workstream

This is documentation-only for now. The active strategy for `LockBased`
correctness remains the current bounded deterministic state-space/model tests
and focused conflict WAST coverage. Do not add `loom` coverage until the future
shared-concurrency implementation satisfies every entry criterion below:

- lock ownership is stored in thread-safe shared runtime structures
- tests can run lock table behavior directly without invoking Wasmtime
  compilation
- transaction ids are deterministic per simulated thread
- tests do not depend on sleeps, wall-clock timing, or OS scheduler behavior

The first future `loom` scenario should be:

1. simulated thread 1 acquires write ownership on granule A
2. simulated thread 2 attempts write ownership on granule A
3. exactly one owner remains for that `GranuleId`
4. an aborted transaction releases any partially acquired independent granules

Until those criteria are met, extend the deterministic model tests instead of
adding scheduler-dependent concurrency tests.

## Wave 8: Parser And Harness Mock Removal

**Purpose:** remove the normalization adapter and remaining parser scaffolds
after runtime support exists.

**Files:**

- Modify: `crates/test-util/src/wast.rs`
- Modify: `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wasmparser/src/readers/core/operators.rs`
- Modify: `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wast/src/core/*.rs`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Remove real-parser allowlists**

Delete file-by-file allowlists that decide which proposal files use real parser
support. Once this wave starts, every transaction proposal file should parse
through the forked parser.

- [ ] **Step 2: Remove the text normalization adapter**

Delete the compatibility mapping that rewrites transaction proposal text to
ordinary Wasm. Keep proposal discovery and the `WASMTIME_TEST_TRANSACTION_WAST`
guard.

- [ ] **Step 3: Remove parser scaffold tags**

Run:

```bash
rg "SHISOFT_TRANSACTION_SCAFFOLD|SHISOFT-TWASM-MOCK|transaction_proposal_adapter_mock|normalize_transaction_proposal_wast" crates/test-util/src/wast.rs /home/shisoft/Code/Research/wasm-tools-transaction
```

Expected: no executable parser or harness path depends on a scaffold tag.

- [ ] **Step 4: Run the full proposal WAST set**

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```

Expected:

```text
test result: ok. <N> passed; 0 failed; 0 ignored
```

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/test-util/src/wast.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Remove transactional WAST normalization"
```

## Wave 9A: Persistent Object GC Logical Marker

**Purpose:** add the first persistent-object collector proof point after
WAST-visible object semantics are stable. This wave is intentionally mark-only:
it proves persistent reachability without deleting, sweeping, tombstoning, or
reclaiming storage.

**Files:**

- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `docs/shisoft/2026-06-14-persistent-object-gc-wave1-design.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
- Modify: `docs/shisoft/transactional-wasm-remaining-work-roadmap.md`

- [x] **Step 1: Add persistent root tests**

Add tests for a root set built from committed persistent state:

- persistent `tglobal` refs through recovered root ids
- explicit recovered root ids
- no active transaction requirement for the first collector

Run:

```bash
cargo test -p wasmtime --lib persistent_object_marker -- --format terse
```

- [x] **Step 2: Add object graph tracing tests**

Add tests that trace:

- struct fields containing `ObjectId`
- array elements containing `ObjectId`
- null refs
- scalar-only objects
- cyclic object graphs
- shared children
- untraceable payload layouts returning an explicit error

Run:

```bash
cargo test -p wasmtime --lib persistent_object_marker -- --format terse
```

- [x] **Step 3: Implement non-mutating mark report**

Implement a stop-the-world logical marker over `ObjectId`:

- reject collection while a transaction is active
- mark from explicit persistent roots
- scan committed object records through `kind` and `type_layout_id`
- mark reachable `ObjectId`s
- report unreachable persistent `ObjectId`s
- report missing/non-persistent roots
- report missing/non-persistent child refs
- return layout/tracing errors without mutating state

Do not compact, move, sweep, tombstone, free slots, or reclaim object-record
storage in this first collector.

- [x] **Step 4: Verify no ordinary GC coupling**

Run:

```bash
rg -n "GcHeap|VMGcRef|Rooted|GcStore" crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_heap.rs
cargo test -p wasmtime --lib transaction -- --format terse
cargo test -p wasmtime --lib persistent_object_marker -- --format terse
```

Expected: ordinary Wasmtime GC names only appear at promotion or volatile bridge
boundaries, not in persistent record identity, root sets, or collector storage.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs docs/shisoft/2026-06-14-persistent-object-gc-wave1-design.md docs/shisoft/transactional-wasm-implementation-log.md docs/shisoft/transactional-wasm-remaining-work-roadmap.md
git commit -m "Add persistent object marker"
```

## Wave 9B: Persistent Object Sweep And Tombstones

**Purpose:** design and implement actual persistent-object reclamation after the
logical marker has enough coverage.

Deferred scope:

- non-durable sweep/report-only mode, if useful for runtime cleanup tests
- durable tombstone records and recovery precedence
- root consistency rules across deletion
- object-table slot/free-list handling
- block/chunk reclamation and future Immix line reuse

## Wave 10: Durable Backends

**Purpose:** add the selectable storage backends needed for the thesis system
after WAST-visible semantics are stable. The next backend is `NVMemory`, a real
PMEM-shaped backend that uses CLWB/SFENCE on supported x86-64 hardware even
when development runs on non-PMEM storage. True durability and recovery tests
remain ignored or gated until a PMEM machine is available.

**Files:**

- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Add backend-selection tests**

Add tests for:

- `VMemory` remains the default.
- `NVMemory` can be selected with a path/device abstraction and creates a
  PMEM-shaped block region.
- `NVMemory` rejects unsupported targets or missing CLWB support unless the
  test explicitly selects a non-durable research mode.
- real PMEM restart/recovery tests are gated with `WASMTIME_TEST_REAL_PMEM=1`.
- `FileBackedMemory` remains represented but is deferred behind `NVMemory`.
- ordinary Wasmtime memories are unaffected by transaction backend selection.

- [ ] **Step 2: Implement NVMemory block region**

Implement block allocation, mmap, CLWB-backed flush, and SFENCE hooks for
`NVMemory`. Keep the same `BlockRegionBackend` interface used by `VMemory`.
The first implementation may map ordinary storage while still following the
PMEM-shaped persistence protocol; do not claim crash persistence from that mode.

- [ ] **Step 3: Implement NVMemory configuration shell**

Add the configuration and error boundaries for `NVMemory`. The first NVMemory
implementation should use the same block/chunk API, name a PMEM path/device,
and make unsupported platforms fail with an explicit backend-selection error.

- [ ] **Step 4: Defer FileBackedMemory**

Keep `FileBackedMemory` in the enum and configuration story, but leave it
unsupported until after the thesis-specific `NVMemory` path exists.

- [ ] **Step 5: Verify**

Run:

```bash
cargo test -p wasmtime --lib tmemory -- --format terse
cargo test -p wasmtime --lib transaction_backend -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```

- [ ] **Step 6: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs crates/wasmtime/src/runtime/vm/memory/tmemory.rs docs/shisoft/transactional-wasm-runtime-core-design.md docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Add selectable transactional memory backends"
```

## Final Acceptance

Run these commands before declaring the roadmap complete:

```bash
cargo test -p wasmtime-environ transaction -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
cargo test -p wasmtime --lib tmemory -- --format terse
cargo test -p wasmtime --lib persistent_object -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
rg "SHISOFT-TWASM-MOCK|SHISOFT_TRANSACTION_SCAFFOLD|transaction_proposal_adapter_mock|normalize_transaction_proposal_wast" crates docs/shisoft /home/shisoft/Code/Research/wasm-tools-transaction
```

Completion requires:

- full transaction proposal WAST run reports zero ignored tests
- no path-scoped WAST fixture replacements
- no transaction text normalization
- no ordinary-memory stand-in for `tmemory`
- no proposal-visible runtime mock
- `ObjectId` is the runtime identity for persistent transactional objects
- persistent object records are traceable by `kind`, `type_index`, and
  embedded `ObjectId` refs
- the first persistent-object collector marks and sweeps `ObjectId` graphs
- ordinary Wasmtime GC and ordinary Wasmtime memory remain unchanged for
  non-transactional code
