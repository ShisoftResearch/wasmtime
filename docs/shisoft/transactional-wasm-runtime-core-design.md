# Transactional Wasm Runtime Core Design

Date: 2026-06-04
Last updated: 2026-06-15

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
puts transactional persistence behind separate transactional memory,
transactional object heap, volatile recovery-time object index, durable
log/data publication, and persistent-reachability components.

The executable runtime core includes:

- `tfunc` transaction entry and exit semantics.
- `tmemory` separate from ordinary Wasmtime `memory`, with `VMemory`,
  PMEM-shaped `NVMemory`, and explicit `FileBackedMemory` backends.
- Per-transaction copy-on-write workspace.
- Wizard-style `GranuleId` ownership.
- Configurable transaction runtime shape with `LockBased` as the first
  implementation.
- Transactional table/global/memory runtime paths using `TTable`, `TTableSize`,
  `TGlobal`, `TMemory`, and `TMemorySize` granule ownership.
- Real text parsing/lowering/runtime coverage for the current
  `simple-transactions` proposal WAST tranche.

The persistent object direction adds:

- `ObjectId` as the runtime identity for persistent transactional objects.
- A persistent transactional object heap separate from ordinary Wasmtime GC.
- A volatile object table/index that is rebuilt from committed object log
  winners during recovery, rather than a persisted object-table log.
- Backend object records with explicit object headers.
- Inline durable reference values for `ti31`, `tfuncref`, and `texternref`
  inside persistent struct fields and array elements. These are not object-table
  payloads and do not allocate standalone `ObjectId`s.
- Zen-style fixed-size transaction log entries plus append-only variable data
  records for `tmemory`, globals, tables, and object records.
- Commit-time promotion from transaction-local volatile objects into persistent
  objects when persistent reachability requires it.
- Persistent reachability over durable roots and `ObjectId` edges.

`ttry` and `tfail` now have an executable structured-failure path for the current
WAST tranche, but they remain a later cleanup workstream for proposal-complete
semantics and diagnostics.

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

Storage clarification from Eliot Moss, June 5, 2026:

- Persistent storage is organized as a region of fixed-size aligned blocks.
  Blocks are grouped into chunks, where each chunk is a contiguous run of one
  or more blocks.
- The persistent object heap and persistent linear memories are separate logical
  consumers of the same block/chunk storage substrate.
- A linear `tmemory` is physically backed by a possibly discontiguous chunk
  list, but it is mapped into one contiguous virtual address reservation for
  the running VM.
- `ObjectId` is the granule identity for object-model transaction conflicts,
  together with the fact that the granule is an object rather than a linear
  memory, table, or other object space.

Later Zen-style persistence clarification:

- The default recovery model does not persist an object table. Persistent
  object updates go through the ordinary transaction log/data publication path,
  and recovery rebuilds the volatile object table/index from committed object
  winners.
- `ObjectId` remains persistent identity because it is stored in each object
  record header and embedded in object granule logical ids.
- A persistent object-table or persistent-index mode may be added behind a
  separate future feature/configuration switch if recovery time proves too
  expensive, but it is not the default branch direction.

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

Runtime transaction state is store-local and mirrored in a thread-local current
transaction id. A normal execution thread processes at most one active
transaction at a time. The spectest conflict helpers may suspend and restore
transaction workspaces to model multiple transaction ids in one test thread,
but production execution should keep the one-thread/one-active-transaction
invariant.

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
- Durable log streams for fixed-size transaction log entries.
- Durable data streams for append-only variable-size data records.
- Future transactional table, element, data, and minor runtime-state regions.
- Optional future `ObjectTableRegion` only if a persistent-index mode is added.

A region is divided into fixed-size aligned blocks. A chunk is a contiguous run
of blocks. All blocks in a region use the same fixed size to avoid
fragmentation between region users. The first Wasmtime implementation copies
Wizard's x86-64 Immix region default of 512 KiB blocks. That is a multiple of
the 64 KiB Wasm page size and sits within Eliot's suggested 256 KiB to 1 MiB
range. A chunk list records the logical order in which chunks make up a
higher-level region.

Blocks are allocated from a global block allocator. Transaction log and data
streams are thread-local at execution time, but they do not own fixed lanes:
threads request log/data blocks from the global allocator, and blocks from
ended threads can return to a global free list. Recovery recomputes allocator
tail/free knowledge from chunk-start scans and committed log records instead of
persisting allocator counters just to reduce recovery work.

Chunks are contiguous runs of one or more blocks. The first block of a chunk is
the chunk-start block and carries the chunk header. Continuation blocks in the
same chunk are raw payload blocks and do not repeat the chunk header. Recovery
scans chunk-start blocks, not every continuation block. Stream replay should be
structured so independent streams can be replayed in parallel.

Data records are dispatched by payload size:

- small records: multiple records packed into one data block/chunk
- medium records: one or more records within a block-sized chunk
- large records: one record spanning a multi-block chunk

Log entries are fixed size and never require multi-block records.

The volatile backend implements the same shape with anonymous mmap. `NVMemory`
and `FileBackedMemory` replace allocation flush/fence behavior without changing
the logical region frontends.

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
storage layer. Current backend roles are:

- `VMemory`: default volatile anonymous-mmap block/chunk storage with no-op
  flush/fence behavior.
- `NVMemory`: PMEM-shaped block/chunk storage. In
  `ResearchPretendPmem` mode it runs on ordinary mapped storage while preserving
  the PMEM publication shape; in `RequireHardwarePmem` mode it requires the
  target flush implementation, currently x86-64 `CLWB` plus `SFENCE`.
- `FileBackedMemory`: explicit filesystem-backed storage using a shared
  writable mapping, `msync(MS_SYNC)` for range flush, and `File::sync_data()`
  or `File::sync_all()` as the fence/metadata durability analogue.

A linear `tmemory` is physically backed by a possibly discontiguous chunk list.
For execution, it reserves a contiguous virtual address range for the running
VM, following Wizard's 8 GiB reservation model for the current Wasm32-style
research target. Unused virtual space remains inaccessible. Active chunks are
mapped read-write into their logical offsets inside that reservation, so
compiled code and runtime helpers still see a contiguous linear memory.

The `TMemoryRegion` frontend supports:

- current byte length and page length
- capacity and grow
- reading committed bytes
- committing byte ranges or granules
- resolving granule metadata for ownership and version checks
- mapping a chunk list into the linear virtual reservation
- growing by attaching or allocating additional chunks
- backend-specific range flush and fence hooks

The frontend must not encode a specific persistence model. `VMemory`,
`FileBackedMemory`, and `NVMemory` should be interchangeable behind the shared
block-region API.

`NVMemory` is allowed to run in a research mode on ordinary mapped storage while
still executing the PMEM-shaped persistence protocol. `FileBackedMemory` is the
filesystem analogue, not a PMEM replacement. Tests that require actual
power-fail persistence, real PMEM media, or restart recovery stay ignored or
environment-gated until the required durable metadata and hardware are
available. See:

- `docs/shisoft/transactional-wasm-nvmemory-pmem-design.md`
- `docs/shisoft/transactional-wasm-file-backed-memory-design.md`

## Durable Log And Data Records

Durable publication uses two append-only surfaces:

- Fixed-size log entries for recovery scanning and transaction commit
  recognition.
- Variable-size data records for payload bytes.

The log is unified across granule domains. `tmemory`, `tglobal`, `ttable`,
`TStruct`, and `TArray` updates all publish through the same transaction log
entry shape, distinguished by packed logical ids. Object metadata does not have
a separate object-table log in the default Zen-style mode.

The data side is intentionally split by semantics, even when the first
implementation reuses the same block/chunk allocator:

- **Object publication data** is object storage. `TStruct` and `TArray` records
  are new object versions; committed records are replayed by choosing the
  highest committed version per `ObjectId`.
- **Granule undo data** is rollback material. Linear `tmemory` undo records
  contain old committed granule bytes saved before in-place mutation. They are
  not object storage, are not winners, and are reclaimed by log/undo cleanup
  rules rather than by persistent object GC.

This separation matters because objects use COW/redo publication, while
persistent linear memory uses undo-before-in-place writes. Object data records
must be managed with object reachability; linear-memory undo records must be
managed with transaction completion and log cleanup.

Durable region metadata is block-derived, not byte-derived. Persistent headers
store block counts and block indices as `u32` values because block size is fixed
inside a region. This keeps the on-media headers compact while allowing large
regions without treating every offset as a byte address. The no-next sentinel is
`u32::MAX`.

The current durable headers are:

```rust
#[repr(C)]
struct RegionHeader {
    magic: u32,
    block_size: u32,
    num_blocks: u32,
    block_table_start_block: u32,
    block_table_block_count: u32,
    metadata_descs_start_block: u32,
    metadata_descs_block_count: u32,
    num_descs: u32,
}

#[repr(C)]
struct LogBlockHeader {
    magic: u32,
    stream_id: u32,
    block_seq: u32,
    next_block: u32,
    entry_count: u32,
}

#[repr(C)]
struct DataChunkHeader {
    magic: u32,
    stream_id: u32,
    chunk_seq: u32,
    next_chunk: u32,
    chunk_blocks: u32,
    tail_block_delta: u32,
    tail_in_block: u32,
}
```

`block_seq` is kept for validation during recovery. `tail_block_delta` and
`tail_in_block` are recomputed/validated from data-chunk append state and do
not imply a globally persisted allocator cursor.

Each object/global/table logical version is represented by one publication data
record and one publication log entry. The publication commit sequence is:

1. Append all data records.
2. Flush data.
3. Fence.
4. Append ordinary log entries.
5. Flush log.
6. Append the final log entry with the LP bit set.
7. Flush log.
8. Fence.

The final LP bit is enough for commit recognition. Recovery treats the first
appearance of a transaction id in a stream as transaction begin. Loose entries
without an LP are aborted. Because each thread writes to its own stream, a
sudden transaction-id change in one stream before LP also terminates the
previous transaction as aborted.

`TxLogEntry` is fixed at 32 bytes and aligned to 32 bytes, so two entries fit in
one 64-byte cache line:

```rust
#[repr(C, align(32))]
struct TxLogEntry {
    logical_id: u64,
    version: u32,
    tx_meta: u32,
    data_block: u32,
    data_offset: u32,
    crc32: u32,
    reserved: u32,
}
```

`data_block` is a block index and `data_offset` is an offset within that block.
`tx_meta` bit 0 is LP. The remaining bits carry the transaction id in the
current implementation (`txid << 1`). `reserved` carries the log-entry role:
zero is a `TObjectPub` entry, and one is a `TMemoryUndo` entry. Log
entries carry CRC32 because recovery scans them directly and must reject torn or
corrupt entries. Data records do not carry a checksum in this design: they are
not scanned independently, and their completeness is guaranteed by
data-before-log or undo-before-write ordering, flushes, fences, and LP
publication. Data corruption detection is left to future storage-layer
integrity work.

`TxDataRecordHeader` is a minimal 24-byte prefix before each data payload:

```rust
#[repr(C)]
struct TxDataRecordHeader {
    logical_id: u64,
    version: u32,
    kind: u16,
    role: u16,
    payload_len: u32,
    type_info: u32,
}
```

`kind` mirrors the packed granule domain and is just large enough for the
transactional granule/object kinds. The 16-bit role field is not a flag set:
zero means `TObjectPub` data and one means `TMemoryUndo` data. No format-version
field is stored because this research branch does not need backward-compatible
on-media formats yet.

`version` is `u32` in durable log/data records and object records. Recovery
chooses the highest committed version for each logical id. Duplicate committed
versions for the same logical id are corruption. A higher-version delete entry
will beat older live entries when deletion is designed with GC; deletion is not
implemented in the current object workstream.

Linear-memory undo records use a different ordering because the data record
contains the old granule bytes, not the new bytes. The current implementation
still stages transactional `tmemory` writes during execution; for persistent
backends, the first in-place write happens when commit applies those staged
granules. Before that in-place write to a persistent `tmemory` granule, the
runtime must:

1. Copy the old granule bytes into a granule-undo data record.
2. Flush the undo data.
3. Fence.
4. Append a `TMemoryUndo` log entry for the same transaction and granule.
5. Flush the undo log entry.
6. Fence.
7. Only then allow the in-place write to the persistent linear-memory bytes.

At transaction commit, persistent backends append undo records for staged
granules, mutate the base image in place through the backend commit path, flush
and fence those writes, and then publish the transaction LP. After LP is
durable, the in-place bytes are the committed state and the undo records for
that transaction are obsolete. At transaction abort before commit, the runtime
discards staged bytes without touching the persistent base image. At crash
recovery, undo records in transactions without LP are replayed as rollbacks;
undo records in transactions with LP are ignored.

If a transaction only mutates persistent `tmemory` and has no object/global/table
publication entries, commit still needs an LP. The substrate publishes that LP
as a second final `TMemoryUndo` log entry pointing at the last undo data record;
it does not append another data record. Recovery treats the transaction as
committed because LP is present and ignores all undo entries for that
transaction.

Recovery discovers streams by scanning chunk-start blocks and validates
log-entry CRC32 values. Publication records from LP-marked transactions are
merged by logical id/version. Undo records are instead grouped by transaction:
committed transactions discard their undo records, while loose-end transactions
produce rollback actions. Stream replay should stay pure enough that streams
can be rebuilt in parallel.

The runtime commit path now uses one transaction-owned durable stream for
persistent `tmemory` undo entries. `TMemory` does not own transaction log
streams; it is a participant that can prepare undo records from committed
bytes, apply staged granules in place through its backend, and rely on the
backend commit path to flush/fence persistent data writes. `TransactionState`
owns the unified durable-log object. During commit, the transaction layer
appends all persistent `TMemoryUndo` records into that stream before any
persistent in-place memory write, applies every touched `tmemory` participant,
and then publishes one LP marker for the whole transaction.

This removes the earlier conservative rejection for transactions that touch
more than one persistent `tmemory` participant. The durable-log owner dispatches
through a storage-backend trait. The current implementations are the original
in-memory research scaffold and a file-backed block-region sink that writes data
chunks and fixed-size log entries through the existing region layout, flushes
them with `msync`/`sync_data`, and can be scanned by restart recovery. File
backing is therefore one persistent-backend implementation, not a special
transaction path. Future hardware-PMEM validation should add a CLWB/SFENCE
backend at the same append/flush/fence boundary, without moving log ownership
back into `TMemory`. The remaining runtime limitation is selection and
integration: ordinary store construction still defaults to the in-memory
variant until transaction-log backing is wired through configuration.

## Copy-On-Write Workspace

Each active transaction owns a private workspace.

The current runtime indexes workspace entries by Wizard-style `GranuleId`.
For `tmemory`, staged entries are keyed by
`TMemory { instance, memory_index, granule_index }`. The workspace value stores
the staged bytes for that granule. Persistent `tmemory` keeps the same execution
workspace and changes the commit path: durable undo records store old granule
bytes for crash rollback, then the live persistent linear-memory bytes are
mutated in place only after the undo record is durable.

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

Transactional `tmemory` writes acquire write ownership for every affected
granule, then write into the transaction workspace. Volatile `VMemory` commit
copies staged memory granules into committed `tmemory` with no durable undo
records. Persistent `NVMemory` and `FileBackedMemory` commit durably log each
staged granule's old bytes once, then update the linear-memory bytes in place.
This makes commit/truncation cheaper than linear-memory redo logging: committed
data is already in the persistent base image, and recovery only needs undo
rollback for transactions that did not publish LP.

Commit applies staged memory granules, publishes staged object records through
`PendingPublication`/durable log entries when a durable backend is active,
updates the volatile object index, and releases ownership.
Object commit must complete promotion of any transaction-local volatile objects
before publishing persistent object records. Abort discards the workspace,
drops uncommitted object records, restores persistent `tmemory` undo bytes when
needed, and releases ownership.

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

Durable log entries encode granules as a compact `PackedGranuleId` in the
`logical_id` field. The current encoding reserves the high 4 bits for the
domain and the low 60 bits for the domain payload. Object granules embed
`ObjectId` directly in that payload:

```text
logical_id = (domain << 60) | payload

domains:
  1 = TMemory
  2 = TMemorySize
  3 = TGlobal
  4 = TTable
  5 = TTableSize
  6 = TStruct
  7 = TArray

TStruct/TArray payload = ObjectId.object_index
```

The object-id payload limit is therefore 60 bits in durable log records. Other
domains can pack their instance/module/table/memory indices into the same low
60-bit payload as their durable publication helpers are completed.

The first in-memory `ObjectTable` foundation allocates dense stable
`ObjectId`s, reuses freed slots, stores live-slot versions, and maps persistent
struct and array slots to `TStruct`/`TArray` granules. It also supports staged
struct/array payload snapshots so transactions can read staged values, discard
them on abort, apply them on commit, and validate optimistic object reads
against current object slot versions.

This in-memory table is the volatile recovery-time index for Zen-style object
persistence. It must preserve the final identity split: ordinary volatile
objects use `VMGcRef`, while persistent transactional objects use `ObjectId` at
runtime and in storage. Persistent object records store their `ObjectId` in the
record header so recovery can rebuild the volatile table from committed log
winners.

The current `ttable` runtime writes funcref table entries directly to
Wasmtime's table backing after acquiring `TTable` ownership. This is a
scaffolded execution path, not table-element COW. The future table/object-table
workspace must replace direct mutation so abort can discard table writes.

## Transactional Persistent Object Identity

Ordinary Wasmtime GC remains responsible for ordinary, volatile Wasm GC objects.
The persistent transactional object space is a separate heap/collector path. It
may reuse Wasmtime type, layout, cast, and validation machinery where useful, but
it owns object identity, volatile object-index publication, COW, recovery, and
persistent reachability.

The runtime identity split is:

```text
ordinary volatile object runtime identity = VMGcRef
persistent object runtime identity        = ObjectId
ordinary Wasmtime function metadata       = FuncIndex / compiled code handles
persistent transactional function ref      = durable symbolic function identity
persistent transactional external ref      = durable symbolic external identity
```

`ObjectId` is used at runtime only for persistent transactional heap objects
that have durable object records. It is the durable object identity stored in
object headers and the object granule used for transaction conflicts. `VMGcRef`
remains the handle for ordinary Wasmtime GC objects and must not become the
persistent object identity because it is collector-owned, may require rooting
and barriers, may move under a moving collector, and can encode immediate
`i31ref` values that are not heap records.

Transactional references should end as `ObjectId`-carrying runtime values, not
ordinary GC references backed by a durable side table. The current branch still
uses a volatile Wasmtime-compatible `VMGcRef -> ObjectId` bridge for some parser,
lowering, and ABI paths. That bridge is process-local scaffolding only: the
semantic identity of a persistent `tref` is `ObjectId`.

`ti31`, `tfuncref`, and `texternref` are not persistent heap objects in the
current design. They are inline durable values that can appear inside
persistent struct fields and array elements:

- `ti31` stores the scalar immediate directly.
- `tfuncref` stores a durable symbolic function identity such as module
  fingerprint, function index, and layout id.
- `texternref` stores a durable symbolic external identity such as namespace,
  handle, and layout id.

These values do not allocate object-table slots and do not participate in
object-granule locking. If a later design needs first-class persistent
function/external wrapper objects, that must be introduced as an explicit
object kind rather than by implicitly allocating `Func`, `Extern`, or `I31`
payload records.

Persistent object records live in `ObjectHeapRegion` and are reached through the
volatile object table/index rebuilt at startup:

```text
tref/ObjectId
  -> volatile object table slot
      -> current backend object record address
          -> object record header + payload bytes
```

Every persistent object record has an explicit backend header. The exact layout
can evolve with the backend, but the current shape includes object-specific
metadata only:

```rust
#[repr(C)]
struct TxObjectHeader {
    record_len: u64,
    object_id: u64,
    version: u32,
    kind: u16,
    flags: u16,
    type_layout_id: u32,
}

#[repr(C)]
struct TxArrayHeader {
    base: TxObjectHeader,
    length: u32,
}
```

The `kind` field identifies the persistent object payload kind. The current
recoverable object-table payloads are `Struct` and `Array`; future persistent
runtime wrapper objects may add more explicit kinds. Inline durable values such
as `ti31`, `tfuncref`, and `texternref` are encoded in struct/array payload
slots rather than as object records.
`version` is the durable object-record version used by log recovery.
`type_layout_id` points at durable trace metadata in region metadata space. It
is not the Wasmtime type identity used for validation or casts. Wasmtime module
metadata remains the semantic authority for type checking; the persistent
layout registry stores only the facts needed after recovery to size objects and
trace `ObjectId` references.
Write ownership remains attached to the stable `ObjectId` granule because COW
writes publish a new object record at commit. The volatile table may keep
additional live-slot versions for in-process optimistic-read validation, but it
is not the durable source of object identity.

Persistent type layout records are compact little-endian metadata records stored
in region metadata space before any object publication that references them.
The registry is keyed by nonzero `TypeLayoutId`s and contains:

- `Struct` layouts: deterministic fingerprint, body size, and trace fields with
  field index, payload offset, value size, and scalar/object-reference kind.
- `Array` layouts: deterministic fingerprint, element size, and scalar or
  object-reference element kind.
- Built-in scalar/reference slot encodings for inline `ti31`, durable function
  identities, and durable external identities.

The durable transaction data header still has a generic `type_info` wire field,
but object publication APIs map that field to `type_layout_id` at the runtime
boundary. Recovery rejects an object publication if the referenced
`type_layout_id` is missing, if the object header and outer data header disagree,
or if the header kind disagrees with the recovered layout kind.

Persistent-by-reachability is defined over durable roots and `ObjectId` edges:

```text
durable roots -> ObjectId graph -> persistent object records
```

Committed persistent payloads must not contain raw `VMGcRef`s, process-local
pointers, or PMEM addresses to other objects. Persistent object fields and array
elements store `ObjectId` references for persistent heap-object refs, plus
scalars, nulls, `ti31` immediates, and durable symbolic function/external
references.

Volatile ordinary objects may refer to persistent objects only as short-lived
transaction-local values. A committed persistent object graph cannot depend on an
ordinary volatile GC object. Promotion now covers transaction-mirrored volatile
`tstruct`/`tarray` objects that already have `ObjectTable` payloads and the
first store-backed subset of ordinary Wasmtime GC heap objects: `struct` and
`array` objects whose fields/elements can be encoded as supported scalars,
nulls, inline `ti31` leaves, promoted `ObjectId` edges, or explicitly registered
durable function/external identities. During commit, if a staged persistent root
or persistent object payload refers to one of those objects, the commit path
promotes it into the persistent object space first and stores the promoted
`ObjectId`. Inline `ti31`, durable function, and durable external references
inside those payloads are preserved as values and do not allocate promoted
objects. The source `VMGcRef` and its transaction-local object-table association
remain volatile runtime wrappers; the durable identity is the new persistent
`ObjectId` stored in object records and root publications.

Promotion is graph-based:

- Maintain a transaction-local `VMGcRef` to `ObjectId` promotion map.
- Reuse the same promoted `ObjectId` when the same volatile object is encountered
  multiple times.
- Reserve `ObjectId`s before filling payload records so cyclic volatile graphs
  can be promoted.
- Recursively promote volatile objects reachable from promoted objects when they
  become part of the committed persistent graph.
- Acquire optimistic object reads for promoted struct/array source payloads
  before commit applies live memory/global/table/object mutations.
- Abort the transaction if a value cannot be promoted into the persistent object
  format. Non-null `tfuncref` and `texternref` values are durable only when the
  runtime can encode their symbolic identities inline. The current branch uses
  an explicit per-store durable identity registry for function refs, an
  internal exported-function helper that derives module fingerprints from
  retained original Wasm bytecode and function indices from Wasmtime export
  metadata, and an embedded durable host-data wrapper for test external refs.
  Store-local function resolution is available after a loaded exported function
  has been explicitly registered in that store. Registered raw `tfuncref`
  values can enter transactional object payloads as inline durable `FuncRef`
  leaves, and same-store `tstruct`/`tarray` reads can resolve those leaves back
  to live `tfuncref` values. Internal durable `texternref` test values register
  their current store raw externref and can roundtrip through the same helper
  boundary as `externref` leaves, `anyref`-converted leaves, and `tarray`
  element values. Proposal-WAST live fallback identities are disabled by
  default and are enabled only by an explicit WAST-harness store policy. They
  are same-run execution bridges, not restart-stable persistent identities.
  elements. The live compiled-code-to-libcall bridge tags reference kind in the
  `ObjectValueAbi` high word to disambiguate `VMGcRef`, `VMFuncRef`, externref,
  and inline `ti31`; generic GC/untyped ref decoding also consults the
  store-local external-reference registry before allocating a promoted object.
  Persistent object records still require `REF` high = 0. Ambiguous duplicate
  identity bindings are rejected; public identity APIs, recovered-payload
  reintegration, and multi-instance/module namespace policy remain future work.

After commit, persistent reachability contains persistent roots, `ObjectId`
heap-object edges, and inline durable scalar/reference leaves. Transaction
abort discards uncommitted records and the promotion map.

## Persistent Object GC Strategy

Wave 9B/9C now provides the first implemented persistent-object GC baseline for
this branch. The current runtime has a logical marker, commit-coupled
`PersistentGcState` delta observation, tombstone-less recovery-time winner
filtering, recovered record location reporting, and volatile sweep/reporting.
Durable block/chunk reclamation and reuse remain later workstreams, not
blockers for the first real object runtime or proposal WAST completion.

The current implementation direction is:

- allocate persistent object records in `ObjectHeapRegion`
- publish records through stable `ObjectId` identities and volatile table slots
- keep payloads traceable by `kind` and recovered `type_layout_id` metadata
- store persistent object references through `PersistentObjectRefRaw`, where
  `0` is null and non-zero values encode `ObjectId.object_index + 1`
- reject or promote volatile `VMGcRef` values before commit
- defer durable block/chunk reuse until recovery can safely ignore retired
  object-data ranges

Wasmtime GC code should be reused for type knowledge, not persistent storage.
Useful reusable pieces include shared type indices, struct and array layout
metadata, cast/subtyping validation, and ordinary GC handling for volatile
transaction-local objects. The persistent object space must not reuse
`GcHeap`, `VMGcRef`, Wasmtime GC roots, or Wasmtime GC barriers as durable
object identity or durability mechanisms.

The implemented collector pipeline remains an `ObjectId` graph collector:

```text
persistent roots
  -> rebuilt ObjectId table slots
      -> object records
          -> ObjectId fields in payloads
```

Because persistent graph mutation becomes visible only at transaction commit,
incremental marking uses commit barriers rather than ordinary per-field write
barriers on staged writes. Staged `tstruct`, `tarray`,
`tglobal`, and `ttable` reference writes remain private transaction workspace
state until commit. The commit path is the first point where those changes can
become durable and visible to persistent reachability.

A successful commit should emit a volatile GC delta from the committed graph
publication:

- updated persistent object records and their introduced `ObjectId` edges
- new or updated persistent roots from committed `TGlobal`/`TTable` records
- objects promoted from ordinary volatile GC values into persistent `ObjectId`s
- root additions caused by explicit durable-root mechanisms added later

The first incremental policy should use direct child marking. If commit
publishes an edge `A -> B` and `A` is already marked reachable in the active
cycle, enqueue `B` into the grey queue. If `A` is not marked yet, no immediate
work is needed because scanning `A` later in the same cycle will observe the
current committed record. New root publications enqueue their root `ObjectId`s
directly.

GC progress is commit-coupled rather than periodic in the current branch. The
store keeps an opportunistic volatile `PersistentGcState` with a reachable set
and grey queue. Each successful transaction commit observes the commit barrier
delta and then performs a bounded marking minibatch. The first budget unit is
scanned object count; later versions may switch to edge count or payload bytes.
Because this state is auxiliary GC progress, not transaction correctness state,
post-commit observer failures clear the cached marker and must not report the
already-completed transaction commit as failed.

The minibatch must not scan mutable staged workspaces as if they were committed
state. It should run only after the commit graph is frozen and visible, either
in a commit epilogue after the transaction is no longer considered active or
under a store-level commit/GC gate that prevents concurrent graph mutation. The
current full marker keeps the stricter rule and rejects arbitrary active
transactions.

This incremental design is conservative. Removing a root or deleting an edge
during a mark cycle invalidates the cached reachable set in the current
implementation. Until the runtime has a complete committed-root enumerator, any
commit that changes root-bearing globals/tables or publishes an object record
resets the opportunistic marker and then seeds only roots introduced by that
commit. This may lose incremental progress and delay reclamation, but it avoids
using stale reachability as evidence that later edge additions are live.

The persistent GC baseline is tombstone-less, following the Makalu/Ralloc-style
tradeoff: avoid persistent writes for collector metadata during normal
execution, then rebuild the auxiliary allocator and object-table state with
recovery-time GC. Persistent storage keeps only program state and scan-critical
layout facts:

Wave 9C now uses tombstone-less Makalu/Ralloc-style recovery. Persistent
storage contains committed object records, roots, type/layout metadata, and
scan-critical region metadata. Recovery selects live object-data winners, marks
from persistent roots, rebuilds the volatile object table only for reachable
objects, and reconstructs auxiliary allocator state. Durable block reuse
remains deferred until block-generation or checkpoint retirement metadata
exists.

Research basis: Makalu uses lazily persisted non-essential metadata plus
post-failure recovery-time GC to reduce allocation persistence overhead. Ralloc
adopts the same recovery-time GC direction, defines allocator recoverability,
and adds filter functions to identify pointers inside persistent blocks.

The recovery correctness target is Ralloc-style recoverability: after recovery,
allocator and object-table metadata must describe all and only persistent
objects reachable from persistent roots. A crash may temporarily leak allocated
but unattached objects, detached-but-not-yet-reclaimed objects, thread-local
cache contents, or stale free-list entries, but recovery must not allow the same
persistent storage to be used for two live objects. Leaks are acceptable before
recovery; double allocation is not.

The Makalu-style ownership rule is that a block or chunk must be durably marked
as owned before it can be handed to the mutator. If a crash happens after this
ownership handoff but before the object becomes reachable, recovery-time GC will
discover that the object is unreachable and rebuild volatile free/reclaim state
accordingly. This shifts the steady-state cost away from per-allocation
collector metadata and into recovery.

Ralloc's filter-function idea maps to our persistent type/layout metadata.
Because persistent Wasm objects are typed, recovery does not need conservative
pointer guessing for `tstruct`/`tarray` payloads. The recovered `kind` and
`type_layout_id` identify exactly which fields/elements contain `ObjectId`
references. Raw log replay keeps scan-valid object winners even when their
layout metadata is missing, so stale unreachable records cannot abort recovery.
When a winner is reachable and must be traced or rebuilt into the volatile
object table, unknown or missing layout metadata is a recovery error, not a
reason to scan arbitrary words as object references.

- committed object data records and their `ObjectId`, `version`, `kind`, and
  `type_layout_id` headers
- durable roots from `tglobal`, `ttable`, and explicit root metadata
- persistent type/layout metadata needed to trace object payloads
- minimal region, block, chunk, and log headers needed to scan committed data

The following are volatile or reconstructable and must not be maintained as
durable per-object GC metadata in the baseline design:

- object table entries
- mark bits and line marks
- free lists and reclaim queues
- block live-byte summaries
- per-thread allocator caches
- tombstones for unreachable objects

Recovery uses full reachability instead of tombstones:

```text
1. Scan committed transaction logs and object data records.
2. Select the highest committed live object-data version per ObjectId.
3. Build a temporary ObjectId -> object-record winner map.
4. Load persistent roots and type/layout metadata.
5. Run full ObjectId graph marking over the winner map.
6. Record reachable and unreachable durable record locations from recovered
   `data_block`, `data_offset`, and `record_len` metadata.
7. Rebuild the volatile ObjectTable only for reachable winners.
8. Reconstruct line marks, block live-byte summaries, free lists, and reclaim
   queues from reachable records and region metadata.
9. Expose the VM.
```

If the future block allocator uses persistent undo/redo metadata to move blocks
between free, owned, retired, and active generations, recovery must repair those
metadata operations before the reachability pass. After repair, reachability
remains authoritative for object liveness and volatile free/reclaim structures.

Runtime persistent GC now uses the same reachability machinery to remove
unreachable entries from volatile indices and update volatile block summaries
without persisting dead-object state. Recovery and runtime sweep both report
durable record locations for reachable versus unreachable winners. If the
process crashes before runtime cleanup finishes, recovery repeats the full
reachability computation and reaches the same logical object table.

Durable storage reuse is a block/chunk-cleaning problem, not a per-object
tombstone problem. Old unreachable object data remains in object-data blocks
until a cleaner copies live records into new blocks and publishes durable
block-generation or checkpoint metadata that lets recovery ignore retired
blocks. Until that block-retirement mechanism exists, 9C may report reclaim
candidates and rebuild volatile free knowledge, but must not reuse persistent
blocks whose old records would still be scanned by recovery. `ObjectId`s remain
non-reused until generation/reuse rules are explicitly designed.

Read-heavy workloads with few commits may not advance commit-coupled marking.
A later explicit maintenance API can expose the same minibatch scanner, for
example `persistent_gc_step(budget)`, without changing the commit-barrier
semantics.

## Runtime Permissions

Wizard permissions follow the meeting model: they are type-state on
transactional reference types, not fields on reference values, objects, or all
granules. A `tref` starts with permission `none`. Object operations require
permissioned `tref` types: reads require `read` or `write`, and writes require
`write`. Validation rejects object accesses whose operand type does not carry
the required permission.

`tref.cast_read` and `tref.cast_write` are the dynamic acquisition boundary.
The cast does not change the reference identity. It performs the transaction
check once for a known persistent object reference, records the object granule
in the active transaction table, and returns the same reference with upgraded
validator type-state. Later `tstruct` and `tarray` accesses rely on that
permissioned type instead of probing the transaction table on every access.
`tarray.len` is the current exception: the proposal text uses it without an
explicit permission cast, so lowering performs a read acquisition before the
runtime length helper.

Only persistent objects participate in runtime object-granule locking. Objects
created by static/global transactional initializers are persistent and are
therefore lockable through `tref.cast_read/write`. Ordinary runtime
`tstruct.new` and `tarray.new` allocations are volatile transaction objects for
now: they are still indexed by the volatile object table so field/element
helpers can find their payloads, but permission casts only update validator
type-state and do not acquire persistent object locks for them. The current
commit-time promotion pass can turn those transaction-mirrored volatile objects
into persistent objects when persistent roots or persistent object payloads make
them reachable. Promotion records optimistic source-object reads and validates
them before commit mutates live storage.

The runtime still uses `GranuleId` internally for concurrency control and
commit validation. For persistent object references, `tref.cast_read/write`
maps the reference to the object-space granule (`TStruct` or `TArray`) and
acquires the read/write state there. Write ownership implies read access in
the active transaction.

Linear memory, globals, and tables do not gain Wasm-visible `tref`
permissions. Their transactional operations continue to acquire and validate
their own `GranuleId` ranges dynamically. The handle mechanism discussed in the
meeting could hoist linear-memory range checks in a future design, but it is
not part of the current spec or implementation.

Runtime permissions are transaction-scoped. Commit, abort, trap, and `tfail`
discard the transaction-local access records and release write ownership.

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
- If another transaction owns the granule, use the deterministic
  `TransactionId` priority rule: a lower id can abort/release a higher-id
  owner; a higher id conflicts with the lower-id owner and aborts/fails the
  current access path.
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
in configuration, but the implemented default is deterministic id-priority
locking with no retry.

## Future Loom Workstream

This section is documentation-only for now. The active validation strategy for
`LockBased` remains bounded deterministic state-space/model tests over
`GranuleId` ownership until the future shared-concurrency implementation is in
place and the runtime state is ready for `loom`.

Do not add `loom` tests for `LockBased` until all of the following are true:

- lock ownership is stored in thread-safe shared runtime structures rather than
  only in single-threaded store-local state
- tests can exercise lock-table behavior directly without invoking Wasmtime
  compilation
- transaction ids are deterministic per simulated thread so `loom` schedules do
  not hide id-priority behavior
- tests do not depend on sleeps, wall-clock timing, or OS scheduler behavior

Once those entry criteria are met, the first `loom` scenario should stay
minimal and target the `LockBased` write-ownership rule directly:

1. simulated thread 1 acquires write ownership on granule A
2. simulated thread 2 attempts write ownership on the same granule A
3. the final state leaves exactly one write owner for that `GranuleId`
4. if the losing or aborted transaction had partially acquired independent
   granules before the conflict resolved, abort releases those independent
   granules as well

Until then, deterministic model tests remain the required coverage for
conflict rules, abort release, and `GranuleId` ownership transitions.

## Object Table Direction

The current executable core uses a volatile object table/index. This is now the
default design target, not merely a stepping stone to a persisted object table.
Persistent object records are durable; the table that maps `ObjectId` to the
current winning object record is reconstructed during recovery.

Recovery rebuilds the object table as follows:

1. Load the persistent type layout registry from region metadata space.
2. Scan durable log streams and validate `TxLogEntry` CRC32 values.
3. Keep only transactions whose stream contains an LP-marked final entry.
4. Decode committed `TStruct` and `TArray` data records.
5. Select the highest `version` for each object logical id; duplicate committed
   versions are corruption.
6. Validate scan-critical object-record facts: logical id, version, data role,
   object kind, object header identity, and outer/header layout agreement.
   Do not reject an otherwise scan-valid winner only because its layout record
   is absent; unreachable stale records must not abort raw recovery.
7. Reconstruct phase-1 durable roots from committed `TGlobal`/`TTable`
   root-bearing winners.
8. Run full `ObjectId` graph marking over the recovered winner map.
9. Validate reachable winners against recovered `type_layout_id` metadata while
   tracing and rebuilding them. Missing or kind-mismatched layout metadata is a
   recovery error only for reachable winners.
10. Record reachable and unreachable durable record locations from recovered
   `data_block`, `data_offset`, and `record_len` metadata.
11. Reinstall only reachable object record bytes into the runtime object heap.
12. Reconstruct volatile object table slots only for reachable winners from
    `TxObjectHeader.object_id`, `kind`, `version`, and `type_layout_id`.

Object-table-facing constraints:

- All transactional access routes through `GranuleId`; object IDs are carried
  by object-space variants such as `TStruct` and `TArray`.
- Ownership/version metadata is attached to the transactional object or
  granule, not to ordinary Wasmtime memory.
- Runtime APIs accept a transaction context and `GranuleId` rather than assuming
  raw `VMGcRef` identity or one global durable object map.
- The shared block/chunk backend owns physical storage; concurrency control
  owns access policy; `TMemoryRegion`, `ObjectHeapRegion`, and durable log/data
  streams own their logical interpretations.
- The persistent object heap is a not-necessarily-contiguous collection of
  object chunks. The current volatile object-record staging heap allocates from
  growable `VMemoryBlockRegion` chunks, records each object's
  `chunk_start_block`, `chunk_blocks`, `data_block`, `data_offset`, and
  `record_len`, packs small/medium records into default chunks, and allocates
  multi-block chunks for records larger than the current chunk can hold.
- Persistent object payloads store references as `ObjectId`s, never raw
  `VMGcRef`s, `VMFuncRef`s, process-local pointers, or PMEM addresses.
- Persistent transactional function references are stored as `ObjectId`s whose
  object payload describes the target function metadata.
- Ordinary volatile Wasmtime objects can enter the persistent graph only through
  transaction commit promotion.

An optional persistent-index/object-table mode remains possible if recovery
latency becomes unacceptable. That mode must be behind an explicit future
feature/configuration switch and must not add object-table writes to the
default Zen-style path.

The persistent object workstream wires `TStruct` and `TArray` granules to
durable object records, function references, persistent object identity,
external object wrappers, commit-time promotion, and backend integration.
Following Wizard, they start at whole-object granularity. More precise field or
element granules are a later Wasmtime-specific extension.

## Parser, Validation, And Lowering Implications

The current `simple-transactions` proposal tranche is expected to run through
real transaction text parsing, lowering, and runtime paths without generated
fixture replacement. Future parser work should keep removing any remaining
normalization/scaffold paths outside that tranche. Transaction text and binary
forms should produce real operators and metadata.

Validation must enforce transaction/non-transaction call boundaries and object
space separation:

- `tfunc` signatures are distinct from ordinary `func` signatures.
- `tmemory` operators target `tmemory`, not ordinary `memory`.
- `tglobal` operators target `tglobal`, not ordinary `global`.
- Transaction operations require active transactional context.
- Ordinary `ref` values use Wasmtime GC identity. Persistent `tref` values use
  `ObjectId` identity semantically; current VMGcRef bridges are volatile ABI
  scaffolding.
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

Intermediate waves may keep feature gates and ignored tests only for boundaries
that genuinely need unavailable hardware or future subsystems, such as real
PMEM restart tests and persistent GC/reclamation tests. Each wave should remove
one category of `SHISOFT-TWASM-MOCK` scaffold and record the change in
`docs/shisoft/transactional-wasm-implementation-log.md`.

Final success requires:

- no `transaction_proposal_adapter_mock`
- no transaction proposal normalization adapter
- no ordinary-memory backing stand-in for `tmemory`
- no ignored proposal WAST tests in the targeted proposal tranche
- all proposal WAST tests using the real parser and real Wasmtime engine path
- no durable-object recovery dependence on a persisted object table in the
  default Zen-style mode

## Remaining Workstream Order

Implementation progress is tracked in
`docs/shisoft/transactional-wasm-implementation-log.md` and the WAST roadmap.
The runtime-only marker, commit-coupled incremental marking, and tombstone-less
recovery-time persistent GC are now implemented on this branch. From this
design point, the remaining architecture work should proceed in this order:

1. Keep the executable `tfunc`, `tmemory`, `tglobal`, `ttable`, SIMD, `ttry`,
   `tfail`, object, and conflict paths stable while removing remaining parser
   or harness scaffolds outside the current passing WAST tranche.
2. Extend commit-time promotion coverage beyond the first store-backed
   Wasmtime GC adapter subset. The current adapter handles i31 leaves,
   struct/array heap objects, promoted `ObjectId` edges, durable function and
   external identities, plus explicit harness-enabled live-only WAST fallback
   identities; future work is unsupported ref/payload forms and removal of the
   fallback namespace.
3. Replace volatile `VMGcRef -> ObjectId` bridges with the final
   `ObjectId`-carrying live `tref` ABI for transactional helper boundaries.
   Persistent object records and recovered root records already use the
   centralized `PersistentObjectRefRaw` durable encoding.
4. Finish restart-stable namespace policy for `tfuncref` and `texternref`
   inline values. Registered durable identities and embedded durable extern
   identities exist; live-only WAST fallback identities are explicit harness
   execution bridges, not a recovery-stable external/function namespace.
   First-class persistent
   function/external wrapper objects remain a separate future design and must
   not be implied by raw ref promotion.
5. Complete public/runtime surface reintegration for recovered `tglobal` and
   `ttable` reference-bearing roots. Recovery already reconstructs root
   `ObjectId`s from committed `TGlobal`/`TTable` winners and seeds persistent
   GC; the final live helper ABI still needs to expose those roots without raw
   `VMGcRef` bridges.
6. Keep payload records traceable by `ObjectId` refs and Wasmtime-derived layout
   metadata while extending coverage for promotion and root reintegration paths.
7. Extend the object-record staging heap's block/chunk metadata into the durable
   object-data stream where needed, then add durable block/chunk retirement
   metadata, safe persistent block reuse, and any required allocator repair for
   future ownership-generation transitions.
8. Design explicit `ObjectId` reuse rules only after durable block retirement
   and reuse semantics exist.
9. Add optional persistent-index/object-table persistence only behind an
   explicit future feature/configuration switch if Zen-style recovery time is
   unacceptable.

This order keeps ordinary Wasmtime GC isolated while the persistent object heap
is brought online, then adds persistent reachability collection after the
runtime object path is stable.

## Verification Strategy

Every workstream should include unit tests plus focused WAST commands.

Baseline commands:

```bash
cargo test -p wasmtime --lib transaction
CARGO_INCREMENTAL=0 WASMTIME_TEST_TRANSACTION_WAST=1 \
  cargo test --test wast transaction-proposal/simple-transactions -- --format terse
```

The current simple-transactions gate is expected to report all enabled tests
passing with zero ignored tests. Focused regressions should target the newly
real file first, then the tranche:

```bash
CARGO_INCREMENTAL=0 WASMTIME_TEST_TRANSACTION_WAST=1 \
  cargo test --test wast transaction-proposal/simple-transactions/tconflict-basic.wast -- --format terse
CARGO_INCREMENTAL=0 WASMTIME_TEST_TRANSACTION_WAST=1 \
  cargo test --test wast transaction-proposal/simple-transactions/tconflict-tmemory_1.wast -- --format terse
```

As remaining proposal scopes are enabled, run the full proposal filter:

```bash
CARGO_INCREMENTAL=0 WASMTIME_TEST_TRANSACTION_WAST=1 \
  cargo test --test wast transaction-proposal -- --format terse
```

Durability/recovery tests that require hardware or restart conditions stay
explicitly gated, for example:

```bash
WASMTIME_TEST_REAL_PMEM=1 cargo test -p wasmtime --lib real_pmem -- --ignored
WASMTIME_TEST_FILE_BACKED_TMEMORY_RECOVERY=1 \
  cargo test -p wasmtime --lib file_backed_memory_restart_recovery_is_not_implemented_yet -- --ignored
```

The full proposal set is complete only when it reports zero unexpected ignored
transaction-proposal tests and no `SHISOFT-TWASM-MOCK` tags remain in
executable transaction paths.
