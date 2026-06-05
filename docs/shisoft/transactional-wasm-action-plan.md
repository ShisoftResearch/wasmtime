# Transactional WebAssembly Roadmap

This roadmap turns `docs/shisoft/transactional-wasm-wizard-first.md` into
parallel workstreams for the first Wizard-compatible Wasmtime milestone.

This is research-fork work. Do not open, comment on, or review Wasmtime pull
requests with automation. Keep the work local until a human decides how to
publish or integrate it.

## Target

Make Wasmtime execute a Wizard-style transactional WebAssembly slice:

- `ttry`
- `tfail`
- `tglobal.get`
- `tglobal.set`
- scalar and packed `*.tload`
- scalar and packed `*.tstore`
- `tmemory.size`
- `tmemory.grow`

Milestone 1 is complete when `tmemory` has a distinct volatile mmap-backed
storage path with 256-byte granule metadata, transactional writes use
copy-on-write staging, commit writes staged values into `tmemory`, abort drops
staged values, and migrated tests document Wizard-first behavior. The storage
path is transitional: Eliot Moss's June 5, 2026 clarification makes the
block/chunk region the long-term persistence substrate, with `TMemory`, the
object heap, and the object table as separate frontends.

## Fixed Choices

- Wizard semantics are the source of truth.
- `TMEMORY_GRANULE_SHIFT = 8`.
- `TTABLE_GRANULE_SHIFT = 4`.
- Ordinary `memory` stays on Wasmtime's normal volatile linear-memory path.
- Do not retrofit ordinary Wasmtime memory into transactional memory.
- the shared block/chunk storage layer is the future persistence boundary;
  `tmemory` is the first linear-memory frontend over it.
- `tmemory` chooses its storage backend from transaction configuration.
- Milestone 1 implements only `VMemory`, the volatile anonymous mmap backend
  for `tmemory`.
- `FileBackedMemory`, `NVMemory`, object-table durability, tables, GC objects,
  refs, SIMD, and conflict-heavy concurrency are later milestones.

## Dynamic Architecture

The runtime should be configurable even when milestone 1 only ships one
implemented option per category. The configuration applies to `tmemory` and
transaction runtime components; ordinary WebAssembly `memory` continues through
Wasmtime's existing memory configuration and allocation paths.

Initial selections:

- `TMemoryBackend::VMemory`
- `ConcurrencyControl::LockBased`
- `DurabilityPolicy::VolatileRollbackOnly`
- `ConflictPolicy::AbortOrWizardDefault`

Future selections:

- `TMemoryBackend::FileBackedMemory`
- `TMemoryBackend::NVMemory`
- `ConcurrencyControl::Optimistic`
- `ConcurrencyControl::WaitDie`
- `ConcurrencyControl::WoundWait`
- `ConcurrencyControl::Mvcc`
- `DurabilityPolicy::PersistentData`
- `DurabilityPolicy::PersistentDataAndMetadata`

Design rule: transactional instruction lowering calls through selected runtime
components. It should not hard-code storage, durability, or concurrency-control
policy into every lowered operation. For milestone 1, unimplemented selections
must be represented but rejected if requested, so the executable runtime stays
minimal and uses only `TMemoryBackend::VMemory`.

## Storage Region Direction

Following Eliot's clarification, storage backends should converge on this
shape:

- `BlockRegionBackend`: fixed-size aligned blocks with backend-specific
  map/protect/flush/fence behavior. The first implementation copies Wizard's
  x86-64 Immix default of 512 KiB blocks.
- `ChunkList`: a logical region represented as ordered chunks, where a chunk is
  a contiguous run of blocks.
- Wizard layout structures translated directly into Rust:
  `PWRegionHeader`, `MetaDataDesc`, `BlockEntry`, `ChunkHeader`, `ListKind`,
  `BlockLists`, and `LineMark`.
- `TMemoryRegion`: a linear memory whose chunk list may be physically
  discontiguous but is mapped contiguously into the running VM's virtual
  address space.
- `ObjectHeapRegion`: future persistent Wasm heap object storage over the same
  block/chunk backend.
- `ObjectTableRegion`: future object-id table storage whose persistent chunks
  are physically discontiguous but virtually contiguous for direct `ObjectId`
  indexing.

The 64 KiB Wasm page remains the linear-memory grow unit. Wizard's 256-byte
Immix line size is copied for line marks. Wizard's 256-byte memory granule
remains the transaction ownership and COW unit, but line marks and transaction
granule metadata stay separate.

## Workstream A: Baseline And Dependency Strategy

Purpose: make the research branch reproducible and choose how transaction
syntax enters Wasmtime.

Primary owner: implementation lead.

Inputs:

- `../wizard-engine`, branch `transactions`
- `../wasm-persistence`
- `docs/shisoft/transactional-wasm-wizard-first.md`

Tasks:

- [ ] Record current branch and working tree.

```bash
git branch --show-current
git status --short
```

- [ ] Record reference revisions.

```bash
git -C ../wizard-engine branch --show-current
git -C ../wizard-engine rev-parse HEAD
git -C ../wasm-persistence rev-parse HEAD
```

- [x] Choose parser strategy.

Preferred order:

1. Local `[patch.crates-io]` fork of `wasmparser` plus any wasm-tools crates
   required by its exported operator macros.
2. Vendored minimal parser support for research-only binary tests.
3. Generated binary modules only, with text `.wast` migration deferred.

Deliverable:

- A short note in `docs/shisoft/transactional-wasm-implementation-log.md`
  recording branch, reference hashes, and parser strategy.

Checkpoint:

- The branch state and parser strategy are known before runtime code starts.

## Workstream B: Opcode, Parser, And Validation

Purpose: make Wasmtime understand milestone-1 transaction syntax and reject it
unless the transaction feature is enabled.

Primary owner: parser/validation implementer.

Dependencies:

- Workstream A parser strategy decision.

Likely files:

- `crates/wasmtime/src/config.rs`
- `crates/wasmtime/src/engine.rs`
- `crates/environ/src/module.rs`
- `crates/environ/src/types.rs`
- `crates/environ/src/compile/module_environ.rs`
- patched or forked `wasmparser`
- patched or forked `wast`

Tasks:

- [ ] Add an experimental transaction feature gate.
- [x] Add lifecycle `wasmparser::Operator` variants for `ttry` and `tfail`.
- [x] Add binary parsing for lifecycle `0xfa` prefixed operators.
- [x] Add the remaining milestone-1 `wasmparser::Operator` variants.
- [x] Add binary parsing for remaining `0xfa` prefixed milestone-1 operators.
- [ ] Add minimal text support or generated binary-module fixtures.
- [ ] Add module metadata for transactional memories and globals.
- [ ] Add validation rules separating ordinary and transactional object spaces.

Current bridge:

- [x] Add milestone-1 `0xfa` opcode constants and an internal
  `TransactionOperator` decoder in `wasmtime-environ`.
- [x] Add raw prefixed opcode-byte decoding for generated binary fixtures.
- [x] Add local parser bridge that extracts milestone-1 transaction operators
  from research fixture function-body bytes.
- [x] Add local parser bridge that extracts milestone-1 transaction operators
  from generated research module fixtures.
- [x] Add research fixture metadata and validation for feature gating,
  transactional memories, and transactional globals.
- [x] Add runtime fixture executor bridge for milestone-1 `ttry`/`tfail`
  control behavior.
- [x] Patch local `wasmparser` fork so lifecycle opcodes decode as
  first-class `wasmparser::Operator` variants.
- [x] Extend the local parser fork to the remaining milestone-1 operators.

Milestone-1 operators:

- `TTry`
- `TFail`
- `TGlobalGet`
- `TGlobalSet`
- `I32TLoad`, `I64TLoad`, `F32TLoad`, `F64TLoad`
- packed integer transactional loads
- `I32TStore`, `I64TStore`, `F32TStore`, `F64TStore`
- packed integer transactional stores
- `TMemorySize`
- `TMemoryGrow`

Validation rules:

- `tglobal.get` reads only transactional globals.
- `tglobal.set` writes only mutable transactional globals.
- `*.tload` and `*.tstore` access only transactional memories.
- `tmemory.size/grow` access only transactional memories.
- ordinary ops cannot resolve transactional objects.
- transactional ops cannot resolve ordinary objects.

Checkpoint:

- A binary module containing one milestone-1 operator parses into the expected
  operator variant.
- Validation rejects mixed ordinary/transactional object access.

## Workstream C: Dynamic Runtime Configuration

Purpose: establish the component-selection seams before transaction operations
are lowered.

Primary owner: runtime architecture implementer.

Dependencies:

- Can start after Workstream A.

Likely files:

- `crates/wasmtime/src/config.rs`
- `crates/wasmtime/src/engine.rs`
- new transaction module under `crates/wasmtime/src/runtime/`

Tasks:

- [x] Define internal transaction configuration.
- [x] Add default selections:
  - `TMemoryBackend::VMemory`
  - `ConcurrencyControl::LockBased`
  - `DurabilityPolicy::VolatileRollbackOnly`
  - `ConflictPolicy::AbortOrWizardDefault`
- [x] Decide whether research selections live on `Config`, `Engine`, or
  `Store`.
- [x] Reject unimplemented selections, including `FileBackedMemory` and
  `NVMemory`.
- [x] Pass selected configuration to transaction runtime and `tmemory` storage
  creation.

Checkpoint:

- Runtime code can query selected transaction backend and concurrency-control
  strategy without direct knowledge of CLI or embedder configuration.
- Ordinary Wasmtime memory allocation is unchanged by transaction
  configuration.

## Workstream D: `tmemory` Storage

Purpose: instantiate transactional memories through a dedicated storage path
with side metadata for 256-byte granules. The completed flat `VMemory`
implementation is the executable milestone; the next storage wave should split
the backend into shared block/chunk region infrastructure plus a `TMemoryRegion`
frontend.

Primary owner: memory/storage implementer.

Dependencies:

- Workstream C configuration shape.

Likely files:

- `crates/wasmtime/src/runtime/vm/memory.rs`
- `crates/wasmtime/src/runtime/vm/memory/mmap.rs`
- new file under `crates/wasmtime/src/runtime/vm/memory/`
- `crates/wasmtime/src/runtime/memory.rs`

Tasks:

- [x] Define the `tmemory` backend interface.
- [x] Implement `VMemory` backend.
- [x] Add `TMemory` storage type that dispatches to the configured backend.
- [x] Add side metadata reservation for 256-byte granules.
- [x] Add metadata initialization for newly live granules.
- [x] Add copy and writeback helpers.
- [x] Add grow and shrink helpers that update both data and metadata ranges.
- [x] Add storage-only tests.

Required helpers:

- `granule_index(addr: u64) -> u64`
- `txn_info(granule_index) -> ...`
- `copy_granule(granule_index) -> Vec<u8>`
- `write_granule(granule_index, bytes)`
- `grow_to(new_size)`
- `shrink_to(old_size)`

Storage-only tests:

- one Wasm page has `65536 / 256` granules
- growing by one Wasm page adds 256 metadata records
- copy and write back one 256-byte granule
- writing one granule does not modify neighboring granules
- shrinking returns visible byte length to the old size

Checkpoint:

- `tmemory` storage tests pass without Wasm instruction lowering.
- No ordinary `memory` tests require behavior changes.

## Workstream E: Transaction Runtime And Concurrency Control

Purpose: make the store own active transaction state and provide commit/abort
semantics for milestone-1 copy-on-write staged records.

Primary owner: transaction-runtime implementer.

Dependencies:

- Workstream C configuration shape.
- Workstream D metadata API for memory granules.

Likely files:

- `crates/wasmtime/src/runtime/store.rs`
- `crates/wasmtime/src/runtime/store/data.rs`
- new transaction module under `crates/wasmtime/src/runtime/`

Tasks:

- [x] Define concurrency-control interface.
- [x] Implement `LockBased` strategy.
- [x] Add transaction state to the store boundary.
- [x] Add staged record types.
- [x] Add begin, commit, abort, and fail operations.
- [x] Add read/write acquisition helpers for globals and memory granules.
- [x] Add transaction-state tests.

Required state:

- active transaction id
- next transaction id
- owned/write granule set
- staged global map
- staged memory granule map
- staged memory size map
- simple ownership/version table for milestone 1

Staged records:

- global final value
- memory granule final bytes
- memory size final page count

Transaction-state tests:

- begin then commit clears active transaction
- begin then abort clears active transaction
- commit applies only latest staged records
- abort and fail drop staged records without writeback
- duplicate granule writes update one staged buffer

Checkpoint:

- Transaction state and COW staging behavior are testable without compiled Wasm.

## Workstream F: Runtime Helpers And Lowering

Purpose: execute milestone-1 transaction operators by dispatching through the
selected storage and concurrency-control components.

Primary owner: compiler/runtime integration implementer.

Dependencies:

- Workstream B parser/operator variants.
- Workstream C configuration.
- Workstream D `tmemory` storage.
- Workstream E transaction runtime.

Likely files:

- `crates/environ/src/builtin.rs`
- `crates/cranelift/src/translate/code_translator.rs`
- `crates/cranelift/src/func_environ.rs`
- `crates/wasmtime/src/runtime/vm/`
- `crates/wasmtime/src/runtime/`

Tasks:

- [x] Add runtime helper declarations for `ttry`/`tfail` lifecycle.
- [x] Add runtime helper declarations for `tglobal.*`, `*.tload`,
  `*.tstore`, and `tmemory.*`.
- [x] Add temporary Cranelift-local parser/lowering bridge for lifecycle `0xfa`
  operators.
- [x] Replace the temporary bridge with a local `wasmparser` fork so normal
  `Module::new` accepts lifecycle `0xfa` operators.
- [x] Lower `ttry` and `tfail` through the full validated module path.
- [x] Lower `tglobal.get/set` to runtime helper ABI stubs.
- [x] Lower scalar and packed `*.tload/*.tstore` to runtime helper ABI
  stubs.
- [x] Lower `tmemory.size/grow` to runtime helper ABI stubs.
- [ ] Ensure traps inside active transactions abort before returning to host.
- [ ] Add focused integration tests for each operator family.

Required behavior:

- `ttry` starts a transaction when no transaction is active.
- normal transaction exit commits.
- `tfail` aborts.
- `tglobal.set` stages the final transactional global value.
- `*.tstore` copies every touched 256-byte granule into the transaction write
  set before modifying staged bytes.
- stores crossing a granule boundary stage both granules.
- `*.tload` reads staged bytes first, then committed `tmemory` bytes.
- `tmemory.grow` stages the new visible size.
- commit writes staged globals, staged granules, and staged memory size into
  the concrete runtime objects.
- abort after successful staged grow leaves committed `tmemory` size unchanged.

Checkpoint:

- Generated binary tests can run the full milestone-1 operator set.

Current lifecycle bridge status:

- Wasmtime uses a local `[patch.crates-io]` wasm-tools fork for `wasmparser`,
  `wasm-encoder`, and `wasmprinter`.
- `wasmparser` decodes `0xfa 0x04` as `Operator::TTry` and `0xfa 0x0f` as
  `Operator::TFail`.
- `wasmparser` also decodes the remaining milestone-1 `tglobal.*`,
  `*.tload`, `*.tstore`, and `tmemory.*` binary operators.
- Cranelift lowers lifecycle operators through transaction begin/fail builtins.
- Cranelift lowers `tglobal.get/set` through transaction global helper builtins
  that currently trap at runtime.
- Cranelift lowers scalar and packed integer `*.tload/*.tstore` through
  transaction memory helper builtins that currently trap at runtime.
- Cranelift lowers `tmemory.size/grow` through transaction memory helper
  builtins that currently trap at runtime.
- Runtime helper ABI declarations now exist for transaction data operators, but
  their libcall implementations are trapping stubs until `tglobal` and
  `tmemory` object access is wired.
- A binary `Module::new` regression now proves the normal validated module path
  accepts lifecycle transaction opcodes.

## Workstream G: Test Migration And Verification

Purpose: capture Wizard-first behavior in Wasmtime tests and keep the partial
implementation verifiable.

Primary owner: test/harness implementer.

Dependencies:

- Workstream B fixture strategy.
- Workstream D storage tests.
- Workstream E runtime tests.
- Workstream F integration path.

Proposal files to mine:

- `../wasm-persistence/test/core/simple-transactions/ttry-basic.wast`
- `../wasm-persistence/test/core/simple-transactions/ttry-abort-commit.wast`
- `../wasm-persistence/test/core/simple-transactions/tglobal.wast`
- `../wasm-persistence/test/core/simple-transactions/tload.wast`
- `../wasm-persistence/test/core/simple-transactions/tstore.wast`
- `../wasm-persistence/test/core/simple-transactions/tmemory.wast`
- `../wasm-persistence/test/core/simple-transactions/tmemory_size.wast`
- `../wasm-persistence/test/core/simple-transactions/tmemory_grow.wast`

Tasks:

- [ ] Add storage-only tests for `tmemory`.
- [ ] Add transaction-runtime unit tests.
- [ ] Add generated binary or `.wast` tests for milestone-1 operators.
- [ ] Split out cases requiring tables, refs, GC, SIMD, or conflict behavior.
- [ ] Annotate expectations that depend on 256-byte Wizard granules.
- [ ] Maintain exact verification commands as test names stabilize.

Initial verification commands:

```bash
cargo test -p wasmtime --lib transaction
cargo test -p wasmtime --test all transaction
cargo test -p wasmtime-wast
cargo test --test wast simple-transactions
```

Checkpoint:

- One focused command runs only milestone-1 transaction tests and gives
  actionable failures.

## Milestone Map

### M0: Research Baseline

Workstreams:

- A

Exit:

- branch and reference hashes recorded
- parser strategy selected

### M1a: Parse And Model

Workstreams:

- B
- C

Exit:

- transaction feature gate exists
- milestone-1 operators parse
- transactional memory/global index spaces are represented
- runtime configuration has default selectable components

### M1b: Storage And Transaction Core

Workstreams:

- D
- E

Exit:

- volatile mmap-backed `tmemory` works under storage tests
- transaction state can commit staged records and abort without writeback
- duplicate granule writes update one staged buffer

### M1c: Executable Operators

Workstreams:

- F
- G

Exit:

- milestone-1 operators execute
- abort drops staged transactional globals
- abort drops staged 256-byte memory granules
- failed `tmemory.grow` leaves committed size unchanged
- successful `tmemory.grow` grows committed storage immediately and is not
  rolled back by a later abort
- commit writes staged values into runtime objects

### M1d: Test Migration

Workstreams:

- G

Exit:

- first migrated proposal/Wizard tests run in Wasmtime
- later-scope tests are split or skipped with clear notes

## First Week Cut

The smallest useful first-week target:

- Workstream A complete
- parser strategy selected
- milestone-1 opcode constants and encodings documented in one place
- Workstream C transaction config sketched
- Workstream D `tmemory` backend interface sketched
- first storage-only tests for granule metadata and copy/writeback written

This gets the special `tmemory` decision into code before broader transaction
control-flow work starts.

## Dependency Graph

```text
A Baseline
  -> B Parser and Validation
  -> C Dynamic Config
C Dynamic Config
  -> D TMemory Storage
  -> E Transaction Runtime
B + C + D + E
  -> F Runtime Helpers and Lowering
B + D + E + F
  -> G Test Migration and Verification
```

## Open Decisions

- Whether to add text `.wast` support in `wast` now or keep using generated
  binary modules until the remaining milestone-1 binary operators lower.
- Whether normal transaction exit should be represented by an explicit
  lifecycle opcode in the current proposal fixtures or by structured `ttry`
  block exit semantics.
- Exact representation of transaction ownership metadata in Rust.
- Exact shape and public/private visibility of the transaction configuration
  object.
- Whether selectable transaction components are configured on `Config`,
  `Engine`, or `Store` for the research prototype.
- Exact location for transaction tests once the module boundaries are clearer.
