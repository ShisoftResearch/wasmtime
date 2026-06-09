# Transactional Wasm Runtime Core Design

Date: 2026-06-04

## Feature Switch Policy

All transactional Wasm and persistence work on this branch is guarded by the
Cargo feature named `transaction`. The feature is intentionally default-on on
the research `transaction` branch so future transaction work does not need
extra local build flags. Before any public or upstream-facing cleanup, this
default can be flipped off while preserving the same feature boundary.

Future transaction and persistence changes must stay behind this switch. That
includes parser/lowering bridges, runtime state, `tmemory` backends,
transactional object storage, persistent object identity, persistent GC work,
spectest helpers, and WAST harness transaction-proposal support.

## Goal

Replace the current mock transaction runtime with a Wizard-style runtime core for
Wasmtime, then extend it into a transactional persistent object system. This
design keeps ordinary Wasmtime memory and ordinary Wasmtime GC unchanged, and
puts transactional persistence behind separate transaction memory, object-table,
object-heap, and persistent-reachability components.

The executable runtime core includes:

- `tfunc` transaction entry and exit semantics.
- `VMemory`-backed `tmemory` separate from ordinary Wasmtime `memory`.
- Per-transaction copy-on-write workspace.
- Wizard-style `GranuleId` ownership.
- Configurable transaction runtime shape with `LockBased` as the first
  implementation.
- Transactional table/global/memory runtime paths using `TTable`, `TTableSize`,
  `TGlobal`, `TMemory`, and `TMemorySize` granule ownership.

The persistent object direction adds:

- `ObjectId` as the runtime identity for persistent transactional objects.
- `ObjectId` as the persistent identity for transactional function objects and
  persistent function references.
- A persistent transactional object heap separate from ordinary Wasmtime GC.
- Backend object records with explicit object headers.
- Commit-time promotion from transaction-local volatile objects into persistent
  objects when persistent reachability requires it.
- Persistent reachability over durable roots and `ObjectId` edges.

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
- Transactional memory currently uses `TMEMORY_GRANULE_SHIFT = 6`, or
  64-byte granules. This constant is the single source of truth shared by the
  transaction runtime, `VMemory` metadata, and transaction libcalls, so later
  granule-size experiments should change that constant rather than per-callsite
  arithmetic.
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

Every lowering and runtime helper must enforce this before reaching
transactional storage or object state. Implementation status is tracked in the
WAST roadmap and implementation log rather than in this design document.

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

Wizard's Immix line size is 256 bytes. The current transaction granule size is
64 bytes, so the implementation must keep these concepts separate: line marks
are allocation/GC metadata for block-region users, while transaction granule
metadata is ownership/version metadata for transactional conflict control and
COW.

## TMemory Storage

Ordinary Wasmtime `memory` remains unchanged. `tmemory` uses its own storage
path.

The runtime defines a `TMemoryRegion` frontend over the shared block/chunk
storage layer. The first implemented backend is `VMemory`, an anonymous
mmap-backed volatile block region. The first persistent backend should be
`NVMemory`, not `FileBackedMemory`: it uses the same block/chunk shape but
publishes committed ranges through a PMEM-style flush/fence path based on CLWB
and SFENCE on supported x86-64 systems. `FileBackedMemory` remains a later
compatibility backend.

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

`NVMemory` is allowed to run in a research mode on ordinary mapped storage while
still executing the PMEM-shaped persistence protocol. Tests that require actual
power-fail persistence or post-restart recovery stay ignored or gated until a
real PMEM machine is available. See
`docs/shisoft/transactional-wasm-nvmemory-pmem-design.md` for the detailed
backend design.

## Copy-On-Write Workspace

Each active transaction owns a private workspace.

The current runtime indexes workspace entries by Wizard-style `GranuleId`.
For `tmemory`, staged entries are keyed by
`TMemory { instance, memory_index, granule_index }`. The workspace value stores
the staged bytes for that granule.

Object-backed workspace entries remain keyed by `GranuleId`. `ObjectId` is not a
standalone `GranuleId` variant; it is the object-table slot and runtime identity
carried by persistent object-space variants such as `TStruct` and `TArray`.
Following Wizard, those variants are whole-object granules in the first
object-table workstream. Ownership, versioning, and conflict checks all use the
same `GranuleId` key family.

Reads check the private workspace by `GranuleId` first. For `tmemory`, if the
requested byte range has no staged data, the runtime reads committed bytes.
Reads spanning multiple memory granules merge staged and committed bytes by
granule key. For object records, reads check staged object payloads before
resolving the committed object-table slot.

Writes never update committed `tmemory` immediately. They acquire write
ownership for every affected granule, then write into the transaction
workspace.

Commit copies staged memory granules into committed `tmemory`, publishes staged
object-table slot updates, and releases ownership. Object commit must complete
promotion of any transaction-local volatile objects before publishing persistent
object records. Abort discards the workspace, drops uncommitted object records,
and releases ownership without copying bytes.

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
`ObjectId`s, reuses freed slots, stores live-slot versions, and maps persistent
struct and array slots to `TStruct`/`TArray` granules. It also supports staged
struct/array payload snapshots so transactions can read staged values, discard
them on abort, apply them on commit, and validate optimistic object reads
against current object slot versions.

This in-memory table is the volatile stepping stone toward the persistent object
table. It must preserve the final identity split: ordinary volatile objects use
`VMGcRef`, while persistent transactional objects use `ObjectId` at runtime and
in storage.

The current `ttable` runtime writes funcref table entries directly to
Wasmtime's table backing after acquiring `TTable` ownership. This is a
scaffolded execution path, not table-element COW. The future table/object-table
workspace must replace direct mutation so abort can discard table writes.

## Transactional Persistent Object Identity

Ordinary Wasmtime GC remains responsible for ordinary, volatile Wasm GC objects.
The persistent transactional object space is a separate heap/collector path. It
may reuse Wasmtime type, layout, cast, and validation machinery where useful, but
it owns object identity, object-table publication, COW, recovery, and persistent
reachability.

The runtime identity split is:

```text
ordinary volatile object runtime identity = VMGcRef
persistent object runtime identity        = ObjectId
ordinary Wasmtime function metadata       = FuncIndex / compiled code handles
persistent transactional function identity = ObjectId
```

`ObjectId` is used at runtime only for persistent transactional objects. It is
also the durable object-table slot identity and the object granule used for
transaction conflicts. `VMGcRef` remains the handle for ordinary Wasmtime GC
objects and must not become the persistent object identity because it is
collector-owned, may require rooting and barriers, may move under a moving
collector, and can encode immediate `i31ref` values that are not heap records.

Transactional references are `ObjectId`-carrying runtime values, not ordinary
GC references backed by a side table. A transitional implementation may need a
Wasmtime-compatible wrapper while parser, lowering, and ABI plumbing are being
replaced, but the semantic identity of a persistent `tref` is `ObjectId`.

The same rule applies to transactional function references. Wasmtime
`FuncIndex`, `VMFuncRef`, and compiled-code handles identify executable code
inside the current process/module, but they are not persistent identity. A
persistent transactional function reference is a function object table entry
identified by `ObjectId`; its payload may point to module/function metadata,
signature metadata, and compiled entry stubs, but the stable reference stored in
persistent objects, roots, tables, and transaction workspaces is the `ObjectId`.

Persistent object records live in `ObjectHeapRegion` and are reached through the
object table:

```text
tref/ObjectId
  -> object table slot
      -> current backend object record address
          -> object record header + payload bytes
```

Every persistent object record has an explicit backend header. The exact layout
can evolve with the backend, but the first shape should include at least:

```rust
#[repr(C)]
struct TxObjectHeader {
    record_len: u64,
    object_id: u64,
    kind: u16,
    flags: u16,
    type_index: u32,
}

#[repr(C)]
struct TxArrayHeader {
    base: TxObjectHeader,
    length: u32,
}
```

The `kind` field identifies the persistent object payload kind, such as struct,
array, external object wrapper, or future persistent runtime object kinds.
`type_index` records the Wasmtime shared type identity or a backend layout id.
Live-slot versioning and write ownership remain attached to the stable object
table slot because COW writes publish a new object record at commit.

Persistent-by-reachability is defined over durable roots and `ObjectId` edges:

```text
durable roots -> ObjectId graph -> persistent object records
```

Committed persistent payloads must not contain raw `VMGcRef`s, process-local
pointers, or PMEM addresses to other objects. Persistent object fields and array
elements store `ObjectId` references for persistent refs, plus scalars, nulls,
and supported immediate values.

Volatile ordinary objects may refer to persistent objects only as short-lived
transaction-local values. A committed persistent object graph cannot depend on an
ordinary volatile GC object. During commit, if a staged persistent object refers
to an ordinary volatile object, the commit path promotes that volatile object
into the persistent object space first and stores the promoted `ObjectId`.

Promotion is graph-based:

- Maintain a transaction-local `VMGcRef` to `ObjectId` promotion map.
- Reuse the same promoted `ObjectId` when the same volatile object is encountered
  multiple times.
- Reserve `ObjectId`s before filling payload records so cyclic volatile graphs
  can be promoted.
- Recursively promote volatile objects reachable from promoted objects when they
  become part of the committed persistent graph.
- Abort the transaction if an object cannot be promoted into the persistent
  object format, such as an unsupported host-only `externref`.

After commit, persistent reachability contains only persistent roots and
`ObjectId` references. Transaction abort discards uncommitted records and the
promotion map.

## Persistent Object GC Strategy

Full persistent-object GC is a future workstream, not a blocker for the first
real object runtime or proposal WAST completion. The current object runtime must
still be persistent-GC-ready from day one.

The current implementation direction is:

- allocate persistent object records in `ObjectHeapRegion`
- publish records through stable `ObjectId` table slots
- keep payloads traceable by `kind` and `type_index`
- store persistent references as `ObjectId`
- reject or promote volatile `VMGcRef` values before commit
- allow the first volatile backend to leak or explicitly free test objects
  rather than implementing a full collector immediately

Wasmtime GC code should be reused for type knowledge, not persistent storage.
Useful reusable pieces include shared type indices, struct and array layout
metadata, cast/subtyping validation, and ordinary GC handling for volatile
transaction-local objects. The persistent object space must not reuse
`GcHeap`, `VMGcRef`, Wasmtime GC roots, or Wasmtime GC barriers as durable
object identity or durability mechanisms.

The future persistent collector is an `ObjectId` graph collector:

```text
persistent roots
  -> ObjectId table slots
      -> object records
          -> ObjectId fields in payloads
```

The first collector should be non-moving mark/sweep and should run only when no
transaction is active. Later versions can treat active transaction read sets,
write sets, staged payloads, and promotion maps as temporary roots, then add
Wizard-style Immix line/block reuse and durable recovery.

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

The current executable core starts with a volatile object-table foundation. The
design target is the full persistent object table, so the volatile foundation
must preserve these constraints.

Object-table-facing constraints:

- All transactional access routes through `GranuleId`; object IDs are carried
  by object-space variants such as `TStruct` and `TArray`.
- Ownership/version metadata is attached to the transactional object or
  granule, not to ordinary Wasmtime memory.
- Runtime APIs accept a transaction context and `GranuleId` rather than assuming
  raw `VMGcRef` identity or one global store-local object map.
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
- Persistent object payloads store references as `ObjectId`s, never raw
  `VMGcRef`s, `VMFuncRef`s, process-local pointers, or PMEM addresses.
- Persistent transactional function references are stored as `ObjectId`s whose
  object-table payload describes the target function metadata.
- Ordinary volatile Wasmtime objects can enter the persistent graph only through
  transaction commit promotion.

The persistent object workstream wires `TStruct` and `TArray` granules to
persistent slot layout, function references, persistent object identity,
external object wrappers, commit-time promotion, and backend integration.
Following Wizard, they start at whole-object granularity. More precise field or
element granules are a later Wasmtime-specific extension.

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
- Ordinary `ref` values use Wasmtime GC identity; persistent `tref` values use
  `ObjectId` identity once the persistent object runtime is enabled.
- Persistent object writes must either store persistent `ObjectId` references or
  promote reachable volatile objects during commit.
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

## Remaining Workstream Order

Implementation progress is tracked in
`docs/shisoft/transactional-wasm-implementation-log.md` and the WAST roadmap.
From this design point, the remaining architecture work should proceed in this
order:

1. Keep the executable `tfunc`, `tmemory`, `tglobal`, `ttable`, and SIMD
   transaction paths stable while removing remaining parser/harness scaffolds.
2. Define the `ObjectId`-carrying `tref` runtime representation and ABI boundary.
3. Define transactional function objects so persistent `tfunc` and function
   references use `ObjectId` identity while Wasmtime function indices remain
   payload metadata.
4. Add volatile `VMemory` object-heap records and object-table slots with the
   explicit `TxObjectHeader` shape.
5. Wire `tstruct` and `tarray` operators to `ObjectId`, whole-object granules,
   and COW payload staging.
6. Add commit-time promotion from transaction-local volatile `VMGcRef` graphs to
   persistent `ObjectId` graphs.
7. Keep payload records traceable by `ObjectId` refs and Wasmtime-derived layout
   metadata so persistent GC can be added without changing committed formats.
8. Implement `ttry`/`tfail` structured failure semantics after transaction entry,
   object COW, promotion, and ownership are stable.
9. Add the first non-moving persistent `ObjectId` mark/sweep collector.
10. Add the NVMemory backend first, then defer FileBackedMemory behind
    the same block/chunk interfaces.

This order keeps ordinary Wasmtime GC isolated while the persistent object heap
is brought online, then adds persistent reachability collection after the
runtime object path is stable.

## Verification Strategy

Every workstream should include unit tests plus focused WAST commands.

Baseline commands:

```bash
cargo test -p wasmtime --lib transaction
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
