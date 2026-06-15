# Complete Persistent Promotion Workstreams Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: use
> `superpowers:subagent-driven-development` or `superpowers:executing-plans`
> to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

Date: 2026-06-15
Last updated: 2026-06-15

## Goal

Complete commit-time promotion so every value that can become reachable from
persistent transactional roots is durable before commit, while keeping the
Zen-style fixed transaction log entry compact.

Promotion runs before durable object/root publication. It rewrites volatile
transaction-mirrored heap objects into persistent `ObjectId` object records and
preserves inline durable scalar/reference leaves inside those records.

## Detour Decision: Inline Durable Leaves

The current design deliberately does **not** allocate object-table payloads for
`ti31`, `tfuncref`, or `texternref`.

- `ti31` is encoded inline as `ObjectValue::I31`.
- `tfuncref` is encoded inline as `ObjectValue::FuncRef(DurableFuncIdentity)`.
- `texternref` is encoded inline as
  `ObjectValue::ExternRef(DurableExternIdentity)`.
- `ObjectPayload` contains object-table records only for `Struct` and `Array`
  in the current implementation.
- `ObjectId` remains the persistent identity for heap objects. Inline durable
  leaves are values inside object payloads, not independently lockable
  object-granules.
- If first-class persistent function/external wrapper objects are needed later,
  they must be introduced as explicit object kinds instead of implicit
  allocation from raw references.

## Non-Negotiable Invariants

- `TxLogEntry` must remain at most 32 bytes so two entries fit in one 64-byte
  cache line.
- Do not add promotion-specific fields to `TxLogEntry`.
- Extra promotion metadata belongs in object data records, object headers,
  type-layout records, or host-side encoder registries.
- Persistent heap-object references store `ObjectId`, not `VMGcRef`,
  raw `VMFuncRef`, raw `externref`, or process-local addresses.
- Ordinary Wasmtime GC identity is valid only inside the running process. It
  may be a promotion source but never a recovered persistent identity.
- A transaction that cannot encode a reachable value into a durable
  representation must fail before mutating committed state.

## File Map

- `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`
  - Fixed-size transaction log entry guard.
- `crates/wasmtime/src/runtime/transaction.rs`
  - `ObjectValue`, promotion maps, graph rewrite, object table APIs, and tests.
- `crates/wasmtime/src/runtime/transaction/durable_ref.rs`
  - Durable symbolic identities for function and external references.
- `crates/wasmtime/src/runtime/transaction/object_heap.rs`
  - Object record encode/decode and trace handling for inline durable values.
- `crates/wasmtime/src/runtime/transaction/type_layout.rs`
  - Layout metadata needed for recovery-time tracing.
- `crates/wasmtime/src/runtime/vm/libcalls.rs`
  - Transactional ref helper boundaries and raw i31 conversion.
- `crates/cranelift/src/translate/code_translator.rs`
  - Transactional ref lowering once the final ABI lands.
- `crates/cranelift/src/func_environ.rs`
  - Additional libcall signatures once the final ABI lands.
- `crates/test-util/src/wast.rs`
  - Real transactional syntax and file-backed recovery harness paths.

## Workstream 0: Guard Fixed Durable Log Shape

Status: implemented and committed as `bee38c36c`.

- [x] Add `tx_log_entry_fits_half_cache_line`.
- [x] Add comment near `TxLogEntry` requiring promotion metadata to stay out of
  fixed log entries.
- [x] Verify:

```bash
cargo test -p wasmtime --lib tx_log_entry_fits_half_cache_line -- --format terse
```

## Workstream 1: Inline Durable Reference Value Model

Status: implemented in the current detour.

- [x] Add `DurableFuncIdentity` and `DurableExternIdentity`.
- [x] Add inline `ObjectValue::I31`, `ObjectValue::FuncRef`, and
  `ObjectValue::ExternRef`.
- [x] Remove standalone `ObjectPayload::I31`, `ObjectPayload::Func`, and
  `ObjectPayload::Extern`.
- [x] Reject `ObjectTable::allocate(ObjectKind::I31 | Extern | Func)` with a
  clear "durable values, not object-table payloads" diagnostic.
- [x] Encode/decode inline durable leaves through the object value ABI and
  object record payloads.
- [x] Ensure persistent object tracing treats inline durable leaves as scalar
  leaves and only traces `ObjectValue::Ref(Some(ObjectId))`.
- [x] Remove transaction/object-table i31 promotion side maps.
- [x] Verify:

```bash
cargo test -p wasmtime --lib durable_ -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
```

## Workstream 2: Transaction-Mirrored Object Graph Promotion

Status: implemented for current `tstruct`/`tarray` object-table payloads.

- [x] Promote non-persistent transaction-mirrored struct/array objects to new
  persistent `ObjectId`s before commit.
- [x] Reserve target `ObjectId`s before payload rewrite so cycles are preserved.
- [x] Rewrite `ObjectValue::Ref(Some(source))` edges to promoted or already
  persistent targets.
- [x] Preserve inline `I31`, `FuncRef`, and `ExternRef` values during rewrite.
- [x] Register optimistic reads for promoted source payloads.
- [x] Roll back reserved/promoted objects if promotion fails.
- [ ] Move promotion implementation into a dedicated
  `transaction/promotion.rs` module once the final ABI work stabilizes.

## Workstream 3: Ordinary Wasmtime GC Heap Promotion Adapter

Status: adapter contract and first store-backed reader implemented; durable
function/external identity still pending.

The current promotion path handles transaction-mirrored objects and the first
store-backed subset of ordinary Wasmtime GC heap objects: raw `i31` leaves plus
`struct` and `array` heap objects whose fields/elements can be encoded as
supported scalar values, nulls, inline `i31` leaves, or promoted `ObjectId`
edges. Function/external durable identity remains outside this slice.

- [x] Add an adapter that can classify ordinary Wasmtime GC heap references as
  struct, array, i31, function, external, null, or unsupported.
- [x] Wire the commit prepass through an `OrdinaryGcPromotionAdapter` seam.
- [x] Keep the production default conservative with a no-op adapter that
  preserves the existing unsupported-ref failure.
- [x] Add fake-adapter tests that promote ordinary-GC-shaped struct/array
  graphs, rewrite nested/shared refs, preserve self-cycles, encode raw i31
  leaves inline, remember top-level durable leaf refs as non-object roots, and
  roll back on unsupported child objects.
- [x] Implement the first real store-backed adapter that inspects `VMGcRef`
  headers, classifies i31/struct/array/extern refs, and reads supported struct
  fields or array elements from Wasmtime GC storage.
- [x] For ordinary struct/array objects, extract supported scalar/ref payload
  values and persistent trace-layout facts into the persistent object value
  model.
- [x] Convert real ordinary heap object references into either promoted
  `ObjectId` edges or inline durable leaves.
- [ ] Add durable function/external identity handling to the store-backed
  adapter once Workstream 4 defines the identity sources.
- [x] Make persistent trace-layout metadata type-driven so ref-capable
  struct fields and array elements keep tracing object edges even when the
  current value is null or an inline `i31` leaf.
- [x] Fail before commit if an ordinary heap object cannot be encoded durably.
- [x] Keep the existing file-backed test that mutates a persistent root to
  reference an ordinary Wasmtime GC struct global and recovers the promoted
  persistent graph.
- [x] Add a dedicated ordinary Wasmtime GC array recovery test.

## Workstream 4: Durable Func/Extern Identity Sources

Status: partially implemented. Function references use explicit per-store
registered identities; external references use an embedded durable externref
host-data wrapper in the internal test seam. Automatic module fingerprinting,
restart-time function resolution, and public identity APIs remain pending.

The object value ABI can store durable function/external identities, but runtime
helpers still need authoritative identity encoders at the boundaries where raw
Wasmtime refs are observed.

- [ ] Define automatic module/function fingerprints for `tfuncref` values that
  survive restart and can be resolved against loaded modules.
- [x] Define explicit external-reference namespaces and handles for
  `texternref` values.
- [x] Add internal per-store registration APIs for durable function identities
  and internal durable externref host-data construction for durable external
  identities. Public host/module APIs remain future API work.
- [x] Wire the store-backed ordinary GC adapter to encode registered non-null
  function refs and embedded durable externrefs as inline durable leaves.
- [x] Reject raw function/external references that have no durable identity
  before commit.
- [x] Add recovery tests for object payloads containing inline `FuncRef` and
  `ExternRef` values.

## Workstream 5: Final Persistent Reference ABI

Status: pending.

The branch still has Wasmtime-compatible volatile bridges for some parser,
lowering, and libcall paths. The final form should use `ObjectId` for
persistent heap-object refs and inline durable leaves for scalar/function/
external values.

- [ ] Define the final transactional reference raw ABI.
- [ ] Update Cranelift libcall signatures and lowering to pass the final ABI.
- [ ] Remove remaining process-local `VMGcRef -> ObjectId` assumptions from
  persistent paths.
- [ ] Keep raw `VMGcRef` only as an ordinary Wasmtime GC runtime handle or as a
  short-lived promotion source.
- [ ] Add WAST/runtime tests that verify recovered object references are not
  dependent on process-local raw refs.

## Workstream 6: Recovery And GC Integration

Status: partially implemented for persistent object winners and reachable
struct/array graphs.

- [x] Rebuild volatile object-table slots from committed reachable object
  winners.
- [x] Trace reachable `ObjectId` edges through struct/array payloads.
- [x] Treat inline durable leaves as non-traced values.
- [ ] Finish recovery integration for promoted ordinary Wasmtime GC heap
  objects once Workstream 3 lands.
- [ ] Add durable block/chunk reclamation and reuse after recovery can identify
  retired object-data ranges safely.

## Verification Gate

Before claiming a workstream complete, run the most specific unit test filter
for that workstream and at least:

```bash
cargo test -p wasmtime --lib transaction -- --format terse
```

Before committing broad promotion changes, also run the relevant WAST and
file-backed recovery tests that exercise the touched ABI/runtime path.
