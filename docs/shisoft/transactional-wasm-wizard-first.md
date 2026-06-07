# Wizard-First Transactional WebAssembly Prototype

This document describes the first Wasmtime prototype for transactional
WebAssembly in this research fork. It is not an upstream Wasmtime proposal
document. The intent is to copy the current Wizard transaction semantics first,
then use Wasmtime as the replacement execution engine for thesis experiments.

The prototype should follow the Bytecode Alliance AI Tool Use Policy for all
work in this repository. In particular, this research branch must not use
automation to open, comment on, or review Wasmtime pull requests.

## Goal

Build an executable Wasmtime implementation of Wizard-style transactional
WebAssembly operations, starting with a small instruction slice that has clear
rollback behavior and can be tested against Wizard and the simple-transactions
proposal tests.

## Source of Truth

Wizard is the first compatibility target. The OCaml simple-transactions
interpreter and tests are input material, but when they differ from Wizard the
initial Wasmtime prototype should prefer Wizard behavior.

Relevant prior work:

- `../wizard-engine`, branch `transactions`
- `../wasm-persistence/proposals/simple-transactions`
- `../wasm-persistence/test/core/simple-transactions`
- `../wasm-persistence/interpreter/main/txns.ml`
- `../wasm-persistence/interpreter/main/txns2.ml`

Wizard implementation areas to mirror:

- `src/engine/Opcodes.v3`
- `src/engine/BinParser.v3`
- `src/engine/WasmParser.v3`
- `src/engine/CodeValidator.v3`
- `src/engine/Type.v3`
- `src/engine/Runtime.v3`
- `src/engine/TxnRuntime.v3`
- `src/engine/TxnLockBasedCC.v3`
- `src/engine/TxnLockBasedCCOptimistic.v3`

## Compatibility Choices

Use Wizard's granule model for the first Wasmtime implementation, but keep the
memory granule size centralized so experiments can adjust it by changing one
constant:

- `TMEMORY_GRANULE_SHIFT = 6` in the current branch, so each transactional
  memory granule is 64 bytes.
- `TTABLE_GRANULE_SHIFT = 4`, so each transactional table granule is 16 table
  entries.

Do not default to the OCaml interpreter's transactional memory granule size.
When proposal tests depend on the OCaml value, adapt or split those tests so
the Wizard-first behavior stays explicit.

Use Wizard's opcode shape as the binary compatibility target. The OCaml
prototype and Wizard both use the `0xfa` prefix for the transactional variants
needed by the first milestones:

- `ttry`, `tfail`, `tcall`, `tcall_indirect`, `tcall_ref`
- `tglobal.get`, `tglobal.set`
- `ttable.get`, `ttable.set`, `ttable.size`, `ttable.grow`,
  `ttable.fill`, `ttable.copy`, `ttable.init`
- scalar `i32.tload`, `i64.tload`, `f32.tload`, `f64.tload`, packed
  transactional loads, and matching transactional stores
- `tmemory.size`, `tmemory.grow`, `tmemory.fill`, `tmemory.copy`,
  `tmemory.init`
- transactional reference and GC operations under the same prefixed family

## Non-Goals For The First Slice

The first slice should not implement the whole proposal. Defer:

- durable object tables
- checkpointing and redo logging
- persistent `tmemory` backends such as file-backed mmap and PMEM/DAX
- transactional GC object persistence
- full `tref` permission enforcement
- transactional SIMD
- full multi-threaded contention experiments
- MVCC beyond what is needed to copy Wizard's current runtime behavior

The first slice may include the data structures needed to grow into these
areas, but it should not block on them. In particular, milestone 1 should give
`tmemory` its own volatile mmap-backed storage boundary now, while leaving
durable mappings for later.

## Wasmtime Architecture Touch Points

Wasmtime's ordinary memory stores are compiled as direct machine stores, so
transactional visibility cannot be implemented by observing the public
`Memory` API. The transactional variants must lower to code paths that acquire
transactional access and stage copy-on-write data before any committed
`tmemory` mutation.

Expected local areas:

- Parser and validation:
  - external `wasmparser = 0.251.0`
  - external `wast = 251.0.0`
  - local harness crate `crates/wast`
  - local test discovery in `tests/wast.rs`
  - local test configuration in `crates/test-util/src/wast.rs`
- Module translation and environment:
  - `crates/environ/src`
  - `crates/environ/src/module.rs`
  - `crates/environ/src/types.rs`
- Cranelift lowering:
  - `crates/cranelift/src/translate/code_translator.rs`
  - `crates/cranelift/src/func_environ.rs`
- Runtime store and objects:
  - `crates/wasmtime/src/runtime/store.rs`
  - `crates/wasmtime/src/runtime/store/data.rs`
  - `crates/wasmtime/src/runtime/memory.rs`
  - `crates/wasmtime/src/runtime/vm/memory.rs`
  - `crates/wasmtime/src/runtime/externals/global.rs`
  - `crates/wasmtime/src/runtime/externals/table.rs`
- Transactional memory storage:
  - `crates/wasmtime/src/runtime/memory.rs`
  - `crates/wasmtime/src/runtime/vm/memory.rs`
  - `crates/wasmtime/src/runtime/vm/memory/mmap.rs`
  - `crates/wasmtime/src/runtime/vm/sys/unix/mmap.rs`
  - `crates/wasmtime/src/runtime/vm/sys/custom/mmap.rs`

Because `wasmparser` and upstream `wast` are external workspace dependencies,
the first parser milestone will likely need one of these approaches:

- patch the workspace dependencies locally with a fork or `[patch]` entry
- vendor minimal parser/text support for the research branch
- generate binary tests with the proposal/Wizard encodings while deferring text
  parser support

For this research fork, prefer the route that gets executable semantics tested
fastest. Text-format `.wast` migration can follow once binary parsing works.

## Transactional Memory Storage

`tmemory` should not be implemented as a regular WebAssembly linear memory with
only extra validation rules. It needs a special storage system, following
Wizard's `Target.newTMemory = X86_64Memory.new(_, true)` path.

The reason for this separation is not only transaction rollback. In the long
term, transactional storage frontends can become persistent, while ordinary
WebAssembly `memory` remains the regular volatile linear memory. Wasmtime's
ordinary memory allocation, growth, direct-store lowering, and embedder API
should stay as they are. Future backends should be able to map `tmemory` and
later object-storage frontends onto anonymous mmap, file-backed mmap, or
persistent memory such as Optane-style PMEM/DAX without changing the semantics
of ordinary memories.

The storage boundary should follow Eliot Moss's June 5, 2026 clarification:
persistent storage is a region of fixed-size aligned blocks, grouped into
chunks. `TMemory`, the persistent object heap, and the persistent object table
are separate logical consumers of that same block/chunk substrate.

Wizard's current transactional memory allocation has two spatially related
mmap-backed regions:

- the normal linear byte-addressed memory reservation
- a side reservation for per-granule transaction metadata

In Wizard, the linear memory reserves a large inaccessible address range, then
uses `mprotect` to make the currently live pages readable and writable. For a
transactional memory, Wizard also reserves a granule-info region sized by
`TMEMORY_GRANULE_SHIFT`. Each `TMEMORY_GRANULE_SIZE` memory granule has a
corresponding metadata record containing transaction ownership/version
information and a hash. Growing and shrinking `tmemory` changes both the live
linear-memory range and the live granule-info range.

Wasmtime should copy the execution shape while making the storage substrate
block/chunk based:

- ordinary `memory` continues to use Wasmtime's existing linear-memory
  allocation path and is not expected to become persistent
- `tmemory` uses a separate `TMemoryRegion` frontend over the shared
  block/chunk storage substrate
- the persistent object heap and persistent object table later reuse that same
  substrate through their own frontends rather than through `TMemory`
- transaction configuration selects only the `tmemory` backend; it must not
  change how ordinary Wasmtime memories are allocated
- the first volatile implementation may use mmap/reservation/protection
  directly, but the long-term backend interface should allocate, map, protect,
  flush, and fence fixed-size block chunks
- a linear `tmemory` is physically backed by a possibly discontiguous chunk
  list, but its chunks are mapped into one contiguous virtual reservation for
  the running VM
- the allocator interface should leave room for multiple backends: `VMemory`
  for volatile anonymous mmap in milestone 1, `FileBackedMemory` for
  persistence experiments, and `NVMemory` for PMEM/DAX mappings when the
  platform supports them
- the base pointer must stay stable across growth whenever the reservation can
  cover the declared maximum, so compiled transactional access can safely cache
  memory base information like ordinary Wasmtime code does
- the storage object owns both the byte region and the granule-info region
- every transaction granule lookup maps `address >> TMEMORY_GRANULE_SHIFT` to
  a metadata slot in the side region
- `tmemory.grow` grows the committed region immediately and is not rolled back
  on abort in the Wasmtime research runtime
- growing `tmemory` attaches or allocates additional chunks, maps them into
  the virtual reservation, and initializes new granule-info records
- `tmemory` storage should expose helper methods for transactional lowering,
  such as `txn_info(granule_index)`, `copy_granule(granule_index)`, and
  `write_granule(granule_index, bytes)`

The first implementation implements only `VMemory` and keeps both data and
metadata volatile. `FileBackedMemory`, `NVMemory`, durable transaction metadata,
and MAP_SYNC/DAX-style behavior belong to later durability/object-table work and
should not block milestone 1. The storage boundary should nevertheless be
designed now so persistence can be added by changing the configured `tmemory`
backend and shared block-region backend rather than rewriting every
transactional memory operation.

## Dynamic Configuration

The system should be dynamic. Milestone 1 uses fixed implemented defaults, but
the runtime architecture should not bake one persistent-memory backend, one
concurrency-control strategy, or one transaction policy permanently into the
instruction semantics. This dynamic configuration applies to `tmemory` and the
transaction runtime; ordinary Wasmtime `memory` remains outside this transaction
backend selection.

Selectable parts should include:

- `tmemory` backend:
  - `VMemory`: volatile anonymous mmap
  - `FileBackedMemory`: file-backed mmap
  - `NVMemory`: PMEM/DAX-style mmap when available
- transaction concurrency control:
  - lock-based control, matching Wizard's current behavior first
  - optimistic read validation with pessimistic write ownership
  - later MVCC-style experiments
- durability policy:
  - volatile rollback only
  - persistent data with volatile metadata
  - persistent data and persistent transaction metadata
- conflict policy:
  - abort immediately
  - wait-die or wound-wait style behavior
  - validation failure at commit

The first implementation should expose this as an internal configuration object,
even if only one option is implemented for each category. Requests for
`FileBackedMemory` or `NVMemory` should be rejected until those backends are
actually implemented. Transactional op lowering should call through the selected
runtime components instead of hard-coding storage and concurrency behavior in
every lowered instruction. This keeps milestone 1 small while preserving the
ability to run thesis experiments by switching backends and policies.

## Transaction Model

Start with Wizard's single-current-transaction model:

- A store can have zero or one active transaction for the first slice.
- `ttry` or a top-level transactional call starts a transaction when no
  transaction is active.
- Successful completion commits staged transaction state into the concrete
  runtime objects.
- `tfail`, transactional conflict failure, or a trapping transactional
  operation aborts and drops staged values without writing them into `tmemory`.
- Nested transaction constructs should be treated flatly until a later milestone
  needs stricter nesting behavior.

The runtime should keep transaction state in the Wasmtime `Store` boundary. This
matches Wasmtime's existing isolation model: store-owned instances, memories,
tables, globals, and GC objects do not outlive the store.

Initial transaction state should include:

- active transaction id
- monotonically increasing next transaction id
- transaction table or ownership map
- read set for validation when optimistic behavior is enabled
- write set or owned-granule set
- staged globals, staged memory granules, and staged memory sizes

The default conflict-control strategy should copy Wizard's lock-based behavior
closely enough to preserve semantics. The implementation should still route
through a selected concurrency-control component so optimistic validation,
wait-die, wound-wait, and MVCC-style experiments can be added without changing
instruction semantics.

## Copy-On-Write Staging

Milestone 1 uses copy-on-write staging for transactional state. The committed
`tmemory` byte region is not modified by `tstore`; only commit writes staged
bytes back to the concrete `tmemory` storage.

The first staged record set should support:

- transactional global final values
- transactional memory granule final bytes
- transactional memory final size

Milestone 2 adds:

- transactional table granule final values
- transactional table final size
- transactional passive data/elem effects if needed by migrated tests

The first write to a memory granule copies the committed granule into the
transaction write set. The `tstore` operation then mutates that staged buffer.
Later writes to the same granule update the same staged buffer. A `tload`
checks the staged buffer first and reads committed `tmemory` only when the
current transaction has not staged the addressed granule.

Commit writes staged globals, memory granules, and memory size changes into the
concrete runtime objects. Abort/fail drops staged records and releases
ownership. This keeps rollback-free abort behavior compatible with future
persistent backends: uncommitted transactional bytes never need to be removed
from `NVMemory` or a file-backed mmap because they were never written there.

## Milestone 1: Executable Core

Instruction families:

- `ttry`
- `tfail`
- `tglobal.get`
- `tglobal.set`
- scalar and packed `*.tload`
- scalar and packed `*.tstore`
- `tmemory.size`
- `tmemory.grow`

Required behavior:

- transactional globals are separate from ordinary globals
- transactional memories are separate from ordinary memories
- transactional memories use the dedicated `tmemory` storage path, even when
  the first backend is volatile anonymous mmap
- non-transactional ops cannot access transactional objects
- transactional ops cannot access non-transactional objects
- stores stage copy-on-write buffers before mutation
- abort drops all staged globals, memory granules, and memory size changes
- commit writes staged values into concrete runtime objects and releases
  transaction ownership
- ordinary traps inside a transaction abort before control returns to host

Tests to migrate first from the proposal tree:

- `ttry-basic.wast`
- `ttry-abort-commit.wast`
- `tglobal.wast`, limited to numeric globals and abort/commit cases first
- `tload.wast`
- `tstore.wast`
- `tmemory.wast`
- `tmemory_size.wast`
- `tmemory_grow.wast`

Wizard unit semantics to mirror as Rust tests:

- global staging is dropped after `tfail`
- memory granule staging is dropped after `tfail`
- failed `tmemory.grow` leaves committed size unchanged
- successful `tmemory.grow` is visible immediately and survives later abort
- multiple writes to one granule update one staged buffer
- writes across two transaction granules stage both granules
- commit writes staged values

## Milestone 2: Bulk Memory And Tables

Instruction families:

- `tmemory.fill`
- `tmemory.copy`
- `tmemory.init`
- `tdata.drop`
- `ttable.get`
- `ttable.set`
- `ttable.size`
- `ttable.grow`
- `ttable.fill`
- `ttable.copy`
- `ttable.init`
- `telem.drop`

Required behavior:

- memory bulk operations acquire and stage every affected 256-byte memory
  granule before mutating staged bytes
- table operations acquire and stage every affected 16-entry table granule
  before mutating staged entries
- table growth stages table size and any initialized granules
- abort drops staged table contents and size
- commit writes staged table contents and size

Tests to migrate:

- `tmemory_copy.wast`
- `tmemory_fill.wast`
- `tmemory_init.wast`
- `ttable_get.wast`
- `ttable_set.wast`
- `ttable_size.wast`
- `ttable_grow.wast`
- `ttable_fill.wast`
- `ttable_copy.wast`
- `ttable_init.wast`

Wizard unit semantics to mirror:

- table granule staging abort
- table size staging abort
- table grow/drop staging abort
- bulk memory staging abort over one granule
- bulk memory staging abort over multiple granules

## Milestone 3: Transactional References And GC Objects

Instruction families:

- `tref.null`
- `tref.is_null`
- `tref.func`
- `tref.eq`
- `tref.as_non_null`
- `br_on_tnull`
- `br_on_tnon_null`
- `tref.test`
- `tref.cast`
- `tref.cast_read`
- `tref.cast_write`
- `tstruct.*`
- `tarray.*`

Required behavior:

- transactional references carry Wizard-style permissions
- read casts acquire read permission
- write casts acquire write permission
- transactional structs and arrays stage fields before mutation
- abort drops staged transactional GC object fields
- commit writes staged object mutations

Tests to migrate:

- `tref*.wast`
- `tstruct.wast`
- `tarray*.wast`
- Wizard `TxnObjectTable*` tests only after the runtime object model is clear

## Milestone 4: Concurrency And Conflict Behavior

Instruction families do not need to expand in this milestone. Instead, test
contention behavior against Wizard's transaction runtime.

Required behavior:

- read ownership and write ownership are tracked at Wizard granularity
- conflicting writes abort or wait according to the selected Wizard-compatible
  strategy
- optimistic read validation can detect a version change at commit
- transaction failure status is observable in the same way as Wizard's current
  tests expect

Tests to migrate:

- `tconflict*.wast`
- Wizard `TxnCc*.v3`
- Wizard `TxnInterleavingTest.v3`

## Test Harness Strategy

Use three layers of tests:

1. Small Rust tests for runtime transaction state and staged records.
2. Hand-authored Wasmtime `.wat` or binary tests for the milestone instruction
   subset.
3. Migrated `.wast` tests from `../wasm-persistence/test/core/simple-transactions`.

Do not wait for the full `.wast` text grammar before testing runtime semantics.
If necessary, generate binary modules with `0xfa` transactional opcodes for the
first executable tests.

When migrating proposal tests:

- keep filenames recognizable when possible
- skip or split cases that require later milestones
- annotate Wizard-specific granule expectations
- keep OCaml-specific assumptions out of the expected results

## Implementation Order

1. Add an experimental transaction feature flag.
2. Add an internal transaction configuration object with default selections for
   `VMemory`, lock-based concurrency control matching Wizard's current
   behavior, and volatile rollback-only durability.
   This configuration selects only `tmemory` behavior and must leave ordinary
   Wasmtime memory behavior unchanged.
3. Add or patch parser support for the milestone-1 binary operators.
4. Add the volatile mmap-backed `tmemory` storage type with side granule
   metadata.
5. Add local representation for transactional memories and globals.
6. Add validation rules that separate transactional and non-transactional object
   spaces.
7. Add transaction state to the store boundary.
8. Add staged records for globals, memory granules, and memory size.
9. Lower milestone-1 transactional instructions to runtime helpers or builtins
   that dispatch through the selected storage and concurrency-control
   components.
10. Add Rust abort/commit staging tests.
11. Add generated binary or `.wast` milestone-1 tests.
12. Run targeted tests before expanding to tables.

## Verification Commands

Use targeted commands while the feature is partial:

```bash
cargo test -p wasmtime --lib transaction
cargo test -p wasmtime --test all transaction
cargo test -p wasmtime-wast
cargo test --test wast simple-transactions
```

The exact test names will change as files are added. Keep each milestone's
tests narrow enough that a failure identifies the broken transaction family.

## Risks

The largest risk is parser dependency churn. Wasmtime's compiler sees
`wasmparser::Operator`, so transactional operators need to exist before normal
translation can handle them. A local fork or patch of `wasmparser` may be the
least surprising path for this research branch.

The second risk is direct memory lowering. Ordinary Wasm stores must remain
unchanged, while transactional stores must route through COW staging and access
control. Accidentally reusing ordinary store lowering for `*.tstore` would
write uncommitted data into `tmemory`.

The third risk is scope creep from the proposal tests. The proposal contains
refs, GC objects, SIMD, tables, bulk operations, and conflict tests. Milestone 1
should pass only the tests that exercise globals, memory, and transaction
lifecycle.

## Completion Criteria For Milestone 1

Milestone 1 is complete when:

- Wasmtime can compile and run a module containing milestone-1 transactional
  operators.
- `tfail` aborts and drops staged transactional globals.
- `tfail` aborts and drops every staged transaction memory granule.
- failed `tmemory.grow` leaves committed memory size unchanged; successful
  `tmemory.grow` grows committed storage immediately and is not rolled back by
  a later transaction abort.
- `tmemory` is allocated through a distinct storage path with side metadata for
  transaction granules, even if the backend is still volatile.
- successful transactions write staged final global and memory values at
  commit.
- transactional and non-transactional object spaces are rejected when mixed.
- migrated tests for `ttry`, `tglobal`, `tload`, `tstore`, and `tmemory`
  document the Wizard-first behavior.
