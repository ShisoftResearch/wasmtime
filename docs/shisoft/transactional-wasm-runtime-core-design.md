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

Additional design clarification from Eliot Moss, June 5, 2026:

- Persistent storage is organized as a region of fixed-size aligned blocks.
  Blocks are grouped into chunks, where each chunk is a contiguous run of one
  or more blocks.
- The persistent object heap, persistent object table, and persistent linear
  memories are separate logical consumers of the same block/chunk storage
  substrate.
- A linear `tmemory` is physically backed by a possibly discontiguous chunk
  list, but it is mapped into one contiguous virtual address reservation for
  the running VM.
- The persistent object table is also physically chunk-backed and virtually
  contiguous, so `ObjectId` can index directly into the table. DRAM object-table
  space can use large anonymous mmap reservations with inaccessible gaps for
  shared non-transactional objects and transaction-local objects.
- `ObjectId` is the granule identity for object-model transaction conflicts,
  together with the fact that the granule is an object rather than a linear
  memory, table, or other object space.

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

## Shared Block/Chunk Storage

The long-term storage boundary is not `TMemory` itself. Following Eliot's
design clarification, `VMemory`, `FileBackedMemory`, and `NVMemory` implement a
shared block/chunk region layer that can back multiple logical consumers:

- `TMemoryRegion` for linear transactional memories.
- `ObjectHeapRegion` for transactional Wasm heap objects.
- `ObjectTableRegion` for persistent object-table entries.
- Future transactional table, element, data, and minor runtime-state regions.

A region is divided into fixed-size aligned blocks. A chunk is a contiguous run
of blocks. Block size is a backend policy. The first Wasmtime implementation
should copy Wizard's x86-64 Immix region default of 512 KiB blocks. That is a
multiple of the 64 KiB Wasm page size and sits within Eliot's suggested
256 KiB to 1 MiB range. A chunk list records the logical order in which chunks
make up a higher-level region.

The first volatile backend may implement the same shape with anonymous mmap
rather than durable storage. `FileBackedMemory` and `NVMemory` later replace
the allocation and flush/fence behavior without changing the logical region
frontends.

### Wizard Region Layout To Copy

The first block/chunk implementation should mirror Wizard's
`TxnPWRegion.v3` layout names and invariants, translated into Rust rather than
inventing a Wasmtime-specific allocator shape:

- `PWRegionHeader`: offsets to block table, list sentinels, metadata area, and
  metadata descriptors, plus region block and byte counts.
- `MetaDataDesc`: metadata kind, fixed bytes, unit size, bytes per unit, and
  metadata offset.
- `BlockEntry`: per-block list state with memory-order links and free-list
  links.
- `ChunkHeader`: allocation cursor, allocation limit, chunk limit, and
  `linemarks` offset.
- `ListKind`: metadata, none, used, small-free, and large-free block states.
- `LineMark`: one-byte Immix line mark.

Wizard's Immix line size is 256 bytes. That currently matches Wizard's
transactional memory granule size, but the concepts must remain separate:
line marks are allocation/GC metadata for block-region users, while
transaction granule metadata is ownership/version metadata for transactional
conflict control and COW.

## TMemory Storage

Ordinary Wasmtime `memory` remains unchanged. `tmemory` uses its own storage
path.

The runtime defines a `TMemoryRegion` frontend over the shared block/chunk
storage layer. The first implemented backend is `VMemory`, an anonymous
mmap-backed volatile block region. `FileBackedMemory` and `NVMemory` stay
represented in configuration but are rejected until their block-region backends
exist.

A linear `tmemory` is physically backed by a possibly discontiguous chunk list.
For execution, it reserves a contiguous virtual address range for the running
VM, following Wizard's 8 GiB reservation model for the current Wasm32-style
research target. Unused virtual space remains inaccessible. Active chunks are
mapped read-write into their logical offsets inside that reservation, so
compiled code and runtime helpers still see a contiguous linear memory.

The `TMemoryRegion` frontend must support:

- current byte length and page length
- capacity and grow
- reading committed bytes
- committing byte ranges or granules
- resolving granule metadata for ownership and version checks
- mapping a chunk list into the linear virtual reservation
- growing by attaching or allocating additional chunks

The frontend must not encode a specific persistence model. `VMemory`,
`FileBackedMemory`, and `NVMemory` should be interchangeable behind the shared
block-region API.

## Copy-On-Write Workspace

Each active transaction owns a private workspace.

The first runtime core indexes workspace entries by Wizard-style `GranuleId`.
For `tmemory`, staged entries are keyed by
`TMemory { instance, memory_index, granule_index }`. The workspace value stores
the staged bytes for that granule.

Object-backed workspace entries remain keyed by `GranuleId`. `ObjectId` is not
a standalone `GranuleId` variant; it is the object-table slot identity carried
by object-space variants such as `TStruct` and `TArray`. Following Wizard,
those variants are whole-object granules in the first object-table workstream.
Ownership, versioning, and conflict checks all use the same `GranuleId` key
family.

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

The first in-memory `ObjectTable` foundation allocates dense stable
`ObjectId`s, reuses freed slots, stores live-slot versions, and maps struct and
array slots to `TStruct`/`TArray` granules. It also supports staged
struct/array payload snapshots so transactions can read staged values, discard
them on abort, apply them on commit, and validate optimistic object reads
against current object slot versions. Later object-model work will attach full
payload layout, GC identity, transactional refs, and persistence.

This is not the persistent object-table implementation. Persistent object-space
payload storage comes later. The purpose of this phase is to establish the
runtime `GranuleId` ownership shape without tying it to storage durability, GC,
or one concurrency-control implementation.

The current `ttable` runtime writes funcref table entries directly to
Wasmtime's table backing after acquiring `TTable` ownership. This is a
scaffolded execution path, not table-element COW. The future table/object-table
workspace must replace direct mutation so abort can discard table writes.

## Object Identity And Record Header

Wizard has two relevant object-header shapes. The interpreter object model stores
transaction metadata in `HeapTObject.hdr: GranuleInfo`; the object kind is
implied by the concrete object class and type declaration. The later
object-table prototype separates the stable table slot from the object payload:
the object table maps `ObjectId` to a current address, while the payload record
carries a fixed header and object bytes. Wasmtime should copy the observable
Wizard semantics, but use an explicit backend record header so the design is
ready for volatile `VMemory`, file-backed mmap, and PMEM backends.

`VMGcRef` remains the Wasm-visible GC handle. It must not become the durable
transaction identity because it is collector-owned, may require rooting/barriers,
and can represent immediate `i31ref` values that are not heap records.
Transactional object identity is instead:

```text
VMGcRef -> volatile TxObjectRegistry -> ObjectId
ObjectId -> object table slot -> current object record address
object record header -> object_id + kind + type/layout metadata
```

The volatile `TxObjectRegistry` associates Wasmtime GC objects with `ObjectId`s
for the running store. It must create the association atomically so concurrent
transactions cannot assign different `ObjectId`s to the same GC object. After
lookup, concurrency control operates on `GranuleId::TStruct { object_id }` or
`GranuleId::TArray { object_id }`, not on the raw `VMGcRef`.

The object table owns current-address publication, live-slot versioning,
freelist reuse, and lock state. These properties must stay with the stable
`ObjectId` slot because COW writes allocate a new private object record before
commit. The object record header owns stable facts about the bytes at that
address:

```rust
#[repr(C)]
struct TxObjectHeader {
    record_len: u64,
    object_id: u64,
    kind: u16,
    flags: u16,
    type_index: u32,
    version: u64,
}

#[repr(C)]
struct TxArrayHeader {
    base: TxObjectHeader,
    length: u32,
}
```

The first implementation can keep the table and object records volatile, but it
should keep this separation intact: `ObjectId` is the granule and backend slot
identity, `VMGcRef` is a transient Wasmtime handle, and object references inside
transactional object payloads should be represented as object IDs rather than
raw PMEM or process-local addresses.

## Runtime Permissions

Wizard permissions are modeled as read/write access modes over `GranuleId`.
They are not limited to object references. The same permission API applies to
`TMemory`, `TMemorySize`, `TGlobal`, `TTable`, `TTableSize`, `TStruct`, and
`TArray` granules.

The Wasm-visible `tref none/read/write` permission is the frontend type-system
surface for transactional references. Runtime enforcement still resolves to
granule acquisition: reads acquire optimistic read permission for the target
granule, and writes acquire pessimistic write ownership for the target granule.
Write ownership implies read access in the active transaction.

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
- The shared block/chunk backend owns physical storage; concurrency control
  owns access policy; `TMemoryRegion`, `ObjectHeapRegion`, and
  `ObjectTableRegion` own their logical address/object interpretations.
- The persistent object heap is a not-necessarily-contiguous collection of
  object chunks. Small objects usually use chunks of one block; objects larger
  than a block may use larger chunks.
- The persistent object table is a not-necessarily-contiguous collection of
  blocks physically, but it is mapped into contiguous virtual table space so an
  `ObjectId` can index directly into the table.
- The DRAM object table can be a large anonymous mmap reservation with most
  pages inaccessible and with deliberate gaps between persistent,
  non-transactional shared, and transaction-local object-id ranges.

The object table workstream will later wire `TStruct` and `TArray` granules to
persistent slot layout, function references, GC object identity, external
object references, and backend integration. Following Wizard, they start at
whole-object granularity. More precise field or element granules are a later
Wasmtime-specific extension, not part of the Wizard-first runtime core.

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
2. Shared block/chunk storage facade, with `VMemory` as the first volatile
   block-region backend.
3. `TMemoryRegion` frontend that maps chunk lists into a contiguous linear
   virtual reservation.
4. Copy-on-write workspace reads, writes, commit, and abort for `tmemory`.
5. Wizard-style `GranuleId` plus `LockBased` optimistic-read/pessimistic-write
   ownership.
6. Real parser and validation migration for scalar memory/global WAST files.
7. Remove path-scoped mocks for `tcall_ref`, `return_tcall_ref`, and
   transactional ref-control only after object identity exists.
8. Full object table and transactional refs/GC workstream, reusing the shared
   block/chunk backend through object-heap and object-table regions.
9. SIMD transactional memory workstream.
10. `ttry`/`tfail` structured failure handler workstream.

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
