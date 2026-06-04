# Transactional Wasm Mock Runtime Design

Date: 2026-06-04

## Goal

Pass the simple-transactions proposal WAST tests through the real Wasmtime
engine before building the final persistent `tmemory` runtime. This mock
runtime uses ordinary Wasmtime linear memory as temporary backing storage, but
all transactional instructions still go through transaction-specific parser,
metadata, validation, lowering, and libcall paths.

## Current Baseline

The transaction branch already has:

- Wizard-style `0xfa` transaction opcode parsing in the local wasm-tools fork.
- Text support for `tmemory`, `tglobal`, `tfunc`, `tinvoke`, and milestone-1
  scalar transaction operators.
- A `shisoft.transaction.objects` custom section emitted by the forked WAT
  encoder.
- Wasmtime decoding of that custom section into `Module::transaction_objects`.
- Cranelift lowering checks that require transaction memory/global operators to
  target `tmemory`/`tglobal` metadata.
- Runtime transaction libcalls that still trap for memory/global operations.

## Design

The mock runtime treats `tmemory` as a marked ordinary Wasmtime memory. The
allocation path stays unchanged for this phase. The transaction object metadata
is the authority for whether transaction operations may target a given memory
or global, and the lowered transaction instructions call helper libcalls
instead of using direct memory/global accesses.

The helper libcalls implement copy-on-write behavior:

- `ttry` starts a transaction frame in `TransactionState`.
- `tfail` aborts the active transaction, drops staged writes, and releases
  locks.
- `tglobal.get` reads the latest staged global value if present, otherwise the
  backing Wasmtime global.
- `tglobal.set` stages a new global value while a transaction is active.
- `*.tload` reads staged bytes first, falling back to backing memory.
- `*.tstore` stages bytes while a transaction is active.
- `tmemory.size` delegates to backing memory size.
- `tmemory.grow` delegates to backing memory growth and records enough state to
  abort if the proposal tests require rollback of growth.
- Commit applies staged memory/global writes to the backing Wasmtime storage and
  releases locks.

For the first pass, transaction stores outside an active transaction should trap
with a clear error. This keeps the mock backend honest: transaction writes are
transactional operations, not aliases for ordinary stores.

## Conflict Behavior

The mock runtime uses deterministic `LockBased` conflict checks, not real
parallel execution. This is sufficient for WAST tests that model conflict
through interleaved calls.

- Transaction memory reads acquire read ownership for each touched 256-byte
  granule.
- Transaction memory writes acquire write ownership for each touched 256-byte
  granule.
- Conflicting write/write or read/write ownership traps or aborts according to
  the test expectation we observe in the proposal suite.
- `tmemory.grow` uses a coarse write-like lock for the memory size.
- Commit and abort always release all locks owned by the active transaction.

This is not final concurrency control. It is a deterministic semantic model
that lets spec tests exercise conflict behavior without threading.

## Scope Boundaries

Included:

- Runtime implementation of existing transaction libcalls.
- Mock staged byte/global storage in `TransactionState`.
- Deterministic lock checks using the existing `LockBased` type.
- Real-engine WAST enablement in small tranches.
- Tests that prove transaction writes are staged and commit/abort work.

Deferred:

- Persistent `VMemory` instance allocation.
- `FileBackedMemory` and `NVMemory`.
- Full transaction object table.
- Cross-store or real multi-threaded conflict handling.
- SIMD transaction operators.
- Bulk transactional memory operators such as `tmemory.copy/fill/init`.
- Durable recovery semantics.

## WAST Migration Strategy

Move tests from normalized/ignored status to real parser and real engine only
when the runtime supports their behavior.

Suggested order:

1. `tmemory_size.wast`
2. `tmemory_grow.wast`
3. `tload.wast`
4. `tstore.wast`
5. `tmemory_trap.wast`
6. `tglobal` coverage from proposal tests
7. conflict tests such as `tconflict-tmemory.wast`

Each tranche should be enabled with a focused verification command before
moving to the next tranche.

## Success Criteria

- Existing transaction parser/lowering tests continue to pass.
- Transaction memory/global helpers no longer trap with "not implemented" for
  supported scalar cases.
- Real-engine proposal WAST tranches pass without normalization for the
  supported files.
- Unsupported features still fail clearly and remain disabled in the WAST
  harness.
