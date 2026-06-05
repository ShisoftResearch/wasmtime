# Transactional Wasm Runtime Core Design

Date: 2026-06-04

## Goal

Replace the current mock transaction runtime with the first real Wizard-style
runtime core for Wasmtime. This design targets the smallest slice that can
remove the most important scaffolds while keeping later persistence, object
table, GC, and concurrency-control work open.

The first real core includes:

- `tfunc` transaction entry and exit semantics.
- `VMemory`-backed `tmemory` separate from ordinary Wasmtime `memory`.
- Per-transaction copy-on-write workspace.
- Wizard-style `GranuleId` ownership.
- Configurable transaction runtime shape with `LockBased` as the first
  implementation.
- First `ttable.get/set/size/grow` runtime paths for funcref tables, using
  `TTable` and `TTableSize` granule ownership.

`ttry` and `tfail` are intentionally deferred to a later workstream. They stay
out of the first real runtime core except where existing tests remain disabled
or explicitly marked as pending.

## Source Of Truth

Wizard semantics are authoritative, even where the proposal text or WAST corpus
is ambiguous. The local reference is:

- `/home/shisoft/Code/Research/wizard-engine`, branch `transactions`

Important Wizard behavior observed for this design:

- A top-level transactional function starts a transaction when execution
  begins.
- Non-transactional-to-transactional `tcall` starts a transaction around the
  callee and ends it when the callee returns.
- Transactional memory uses 256-byte granules.
- Transactional table element ownership uses 16-element granules, with table
  size tracked by a separate granule.
- Lock-based concurrency supports optimistic reads and pessimistic writes.
- Transaction abort releases ownership and discards uncommitted data.

## Transaction Boundaries

Wasmtime should copy Wizard's transaction entry model.

An exported/top-level `tfunc` starts a transaction when invoked from the host or
WAST `tinvoke`. Normal return commits the transaction. Traps abort the
transaction before returning the trap to the caller.

A non-transactional function may call a transactional function only through the
transactional call path. That call is an explicit boundary: the callee runs in a
new transaction, normal return commits it, and trap/abort discards it.

A transactional function may call another transactional function without
starting a nested transaction. The existing active transaction remains current.

Ordinary `call`, `call_indirect`, `call_ref`, and return-call variants cannot
target transactional functions. Transactional call variants cannot target
ordinary functions. Validation should reject mismatches when enough metadata is
available; runtime checks should remain defensive.

## Active Transaction Requirement

All transaction-prefixed object and memory operations require an active
transaction:

- scalar and packed `*.tload`
- scalar and packed `*.tstore`
- SIMD `v128.tload*`
- SIMD `v128.tstore*`
- `tmemory.*`
- `tglobal.*`
- `ttable.*`
- `telem.*`
- `tdata.*`
- transactional refs, casts, and branch-on-ref operations
- transactional struct, array, i31, and extern object operations

The first runtime core should enforce this for the operations it implements.
Current coverage includes scalar memory/global operators and funcref
`ttable.get/set/size/grow`. Later workstreams extend the same rule to table
bulk operators, GC objects, refs, and SIMD.

## TMemory Storage

Ordinary Wasmtime `memory` remains unchanged. `tmemory` uses its own storage
path.

The runtime defines a backend trait for transactional memory storage. The first
implemented backend is `VMemory`, an anonymous mmap-backed volatile store.
`FileBackedMemory` and `NVMemory` stay represented in configuration but are
rejected until their backends exist.

The backend trait must support:

- current byte length and page length
- capacity and grow
- reading committed bytes
- committing byte ranges or granules
- resolving granule metadata for ownership and version checks

The trait should not encode a specific persistence model. `VMemory`,
`FileBackedMemory`, and `NVMemory` should be interchangeable behind the
transaction runtime.

## Copy-On-Write Workspace

Each active transaction owns a private workspace.

The first runtime core indexes workspace entries by Wizard-style `GranuleId`.
For `tmemory`, staged entries are keyed by
`TMemory { instance, memory_index, granule_index }`. The workspace value stores
the staged bytes for that granule.

When the persistent object table lands, object-backed workspace entries remain
keyed by `GranuleId`. `ObjectId` is not a standalone `GranuleId` variant; it is
the object-table slot identity carried by object-space variants such as
`TStruct` and `TArray`. Following Wizard, those variants are whole-object
granules in the first object-table workstream. Ownership, versioning, and
conflict checks all use the same `GranuleId` key family.

Reads check the private workspace by `GranuleId` first. If the requested byte
range has no staged data, the runtime reads committed `tmemory` bytes. Reads
spanning multiple granules merge staged and committed bytes by granule key.

Writes never update committed `tmemory` immediately. They acquire write
ownership for every affected granule, then write into the transaction
workspace.

Commit copies staged granules from the workspace into committed `tmemory` and
releases ownership. Abort discards the workspace and releases ownership without
copying bytes.

`tmemory.grow` grows committed storage immediately and is not rolled back on
abort. The grow path must also grow or initialize the granule metadata needed by
the selected backend. Automatic growth is allowed when the selected backend can
support it.

## Granule Identity

The first implementation models Wizard `GranuleId` directly, then adds
Wasmtime-specific identity fields.

Initial granule IDs:

- `TMemory { instance, memory_index, granule_index }`
- `TMemorySize { instance, memory_index }`
- `TGlobal { instance, global_index }`

Table granule IDs:

- `TTable { instance, table_index, granule_index }`
- `TTableSize { instance, table_index }`

`TTable` uses Wizard's 16-element granule size. `TTableSize` is intentionally
separate from element granules so `ttable.size` and `ttable.grow` can conflict
on table growth without aliasing element slot ownership.

Object-table phase identities carry `ObjectId` as the stable object-table slot:

- `TStruct { object_id }`
- `TArray { object_id }`

The object table decides how `object_index` maps to structs, arrays, function
references, external objects, or future GC-backed transaction objects.

This is not the persistent object-table implementation. Persistent object-space
`GranuleId` variants and the full `ObjectTable` come later. The purpose of this
phase is to establish the runtime `GranuleId` ownership shape without tying it
to storage durability, GC, or one concurrency-control implementation.

The current `ttable` runtime writes funcref table entries directly to
Wasmtime's table backing after acquiring `TTable` ownership. This is a
scaffolded execution path, not table-element COW. The future table/object-table
workspace must replace direct mutation so abort can discard table writes.

## LockBased Concurrency

The runtime remains configurable, but the first real implementation is
`LockBased` with optimistic reads and pessimistic writes.

Read path:

- Verify that no other transaction owns the granule for writing.
- Record the granule version in the active transaction table.
- Do not take a long-lived read lock.

Write path:

- If the transaction already owns the granule for writing, continue.
- If it only has an optimistic read record, validate the recorded version and
  upgrade to write ownership.
- If another transaction owns the granule, abort the current transaction.
- On successful acquisition, record the granule as write-owned by the active
  transaction.

Commit path:

- Validate all optimistic read records.
- Copy workspace writes into committed storage.
- Release all owned granules.
- Mark the transaction inactive.

Abort path:

- Discard the workspace.
- Release all owned granules without copying bytes.
- Mark the transaction inactive.

There is no automatic retry in this phase. Conflict policy can be represented
in configuration, but the implemented default is deterministic abort of the
current transaction.

## Object Table Direction

The first runtime core does not implement the full persistent object table.
However, it must avoid designs that would block it.

Object-table-facing constraints:

- All transactional access routes through `GranuleId`; object IDs are carried
  by object-space variants such as `TStruct` and `TArray`.
- Ownership/version metadata is attached to the transactional object or
  granule, not to ordinary Wasmtime memory.
- Runtime APIs accept a transaction context and `GranuleId` rather than
  assuming one global store-local map.
- Backends own payload bytes; concurrency control owns access policy; the
  future object table owns stable object-space `GranuleId` assignment.

The object table workstream will later wire `TStruct` and `TArray` granules to
persistent slot layout, function references, GC object identity, and backend
integration. Following Wizard, they start at whole-object granularity. More
precise field or element granules are a later Wasmtime-specific extension, not
part of the Wizard-first runtime core.

## Parser, Validation, And Lowering Implications

The parser must stop relying on the WAST normalization adapter as real support
lands. Transaction text and binary forms should produce real operators and
metadata.

Validation must enforce transaction/non-transaction call boundaries and object
space separation:

- `tfunc` signatures are distinct from ordinary `func` signatures.
- `tmemory` operators target `tmemory`, not ordinary `memory`.
- `tglobal` operators target `tglobal`, not ordinary `global`.
- Transaction operations require active transactional context.
- Wasmtime-style equivalent diagnostics are acceptable; exact proposal strings
  are not required for this research branch.

Cranelift lowering should route transaction operators to typed transaction
runtime helpers. Ordinary memory/global/table/reference lowering must remain
unchanged.

SIMD arithmetic uses ordinary Wasmtime SIMD lowering inside `tfunc`. SIMD
transactional memory operators route through the transaction memory access path.

## Mock Removal Strategy

Intermediate waves may keep feature gates and ignored WAST files. Each wave
should remove one category of `SHISOFT-TWASM-MOCK` scaffold and record the
change in `docs/shisoft/transactional-wasm-implementation-log.md`.

Final success requires:

- no `transaction_proposal_adapter_mock`
- no transaction proposal normalization adapter
- no ordinary-memory backing stand-in for `tmemory`
- no ignored proposal WAST tests
- all proposal WAST tests using the real parser and real Wasmtime engine path

## Workstream Order

1. Runtime metadata and transaction boundary tracking for `tfunc` entry/exit.
2. `TMemoryBackend` trait and wired `VMemory` allocation for `tmemory`.
3. Copy-on-write workspace reads, writes, commit, and abort for `tmemory`.
4. Wizard-style `GranuleId` plus `LockBased` optimistic-read/pessimistic-write
   ownership.
5. Real parser and validation migration for scalar memory/global WAST files.
6. Remove path-scoped mocks for `tcall_ref`, `return_tcall_ref`, and
   transactional ref-control only after object identity exists.
7. Full object table and transactional refs/GC workstream.
8. SIMD transactional memory workstream.
9. `ttry`/`tfail` structured failure handler workstream.

This order prioritizes the executable runtime foundation before the object-table
and structured failure-handler work that depends on it.

## Verification Strategy

Every workstream should include unit tests plus focused WAST commands.

Baseline commands:

```bash
cargo test -p wasmtime --lib mock_transaction_
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
```

As mocks are removed, the focused WAST commands should target the newly real
files first, then the full proposal set:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```

The full proposal set is complete only when it reports zero ignored
transaction-proposal tests and no `SHISOFT-TWASM-MOCK` tags remain in executable
transaction paths.
