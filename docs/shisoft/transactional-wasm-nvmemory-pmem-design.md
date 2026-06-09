# Transactional Wasm NVMemory PMEM Backend Design

Date: 2026-06-09

## Goal

Add a real persistent-memory-shaped backend for transactional `tmemory` named
`NVMemory`. The backend is for the research `transaction` branch and stays
behind the default-on `transaction` Cargo feature.

The first `NVMemory` implementation should behave like a PMEM backend even on a
machine without real persistent memory:

- use the same block/chunk region shape as `VMemory`
- commit through the existing copy-on-write transaction path
- write committed bytes into an `NVMemory` block region
- flush modified cache lines with PMEM instructions when available
- fence before publishing durable metadata
- skip or ignore tests that require power-fail persistence until real PMEM
  hardware is available

This is different from `FileBackedMemory`. `FileBackedMemory` remains useful as
a compatibility backend later, but it is not the first durable backend.

## Non-Goals

The first `NVMemory` wave does not implement full crash recovery. It should not
claim that data survives process restart or power failure until recovery
metadata and hardware-backed tests exist.

Out of scope for the first wave:

- durable object-table recovery
- durable root recovery
- redo/undo logging for interrupted commits
- post-restart validation tests
- persistent-object GC over recovered records
- automatic detection that a path is mounted on real PMEM

## Backend Shape

`NVMemory` implements the same storage contracts already used by `VMemory`:

- `BlockRegionBackend` for block/chunk allocation, read, write, flush, and fence
- `TMemoryBackendStorage` for committed linear-memory bytes and granule metadata
- `TMemoryRegion` for the logical linear view over chunk lists

The storage stack should become:

```text
TMemory
  -> TMemoryBackendStorage
       -> NVMemory
            -> TMemoryRegion
                 -> MappedLinearRegion
                      -> NVMemoryBlockRegion
                           -> mmap-backed PMEM-shaped bytes
```

The ordinary Wasmtime memory implementation remains unchanged.

## PMEM Persistence Instructions

The backend uses an explicit persistence layer:

```text
persist_range(ptr, len)
  -> align range to cache-line boundaries
  -> issue CLWB for each touched cache line on x86_64 when supported
  -> issue SFENCE after the flush loop
```

The cache-line size should be a named constant, initially 64 bytes. The
transaction granule size is also currently 64 bytes, but these are separate
concepts and must remain separate constants.

On x86_64, the implementation should use Rust core architecture intrinsics when
available. If the current CPU does not advertise `clwb`, `NVMemory` selection
should fail with a clear unsupported-platform error unless the code is running a
test that does not require persistence. Tests that require real persistence
must be gated behind an environment variable such as:

```text
WASMTIME_TEST_REAL_PMEM=1
```

Non-x86 targets should report that `NVMemory` is unsupported until a target
specific flush implementation is added.

## Commit Ordering

The transaction runtime continues to stage writes in the private COW workspace.
`NVMemory` only changes the publication path.

For each committed `tmemory` range:

1. Copy staged bytes from the transaction workspace into the committed
   `NVMemory` byte range.
2. Flush the written byte range.
3. Fence.
4. Update granule metadata such as version and owner.
5. Flush the touched metadata bytes.
6. Fence before the transaction is considered committed.

The first implementation may keep metadata in volatile Rust structures if the
current code cannot yet address persistent metadata bytes directly, but that
must be marked as a scaffold. The long-term design is that payload bytes and
metadata both live in the block/chunk region and both participate in the
flush/fence protocol.

Abort behavior is unchanged: discard the transaction workspace and release
owned granules without writing staged bytes into `NVMemory`.

## Configuration

`NVMemory` should be selectable through `TransactionConfig` without changing
ordinary Wasmtime memory configuration.

The first configuration shape should include:

- backend kind: `TMemoryBackend::NVMemory`
- path or device location for the mmap-backed PMEM-shaped region
- requested region capacity
- maximum pages for each `tmemory`
- a research/testing mode that allows running on non-PMEM storage while still
  using the PMEM-shaped flush/fence code path

The backend should reject missing paths, zero-sized regions, unsupported CPU
features, and unsupported targets with explicit errors.

## Test Policy

Normal CI-style tests may verify:

- `VMemory` remains the default
- `NVMemory` can be selected when a test path and supported flush engine exist
- `NVMemory` uses the same block/chunk logical mapping as `VMemory`
- writes go through `flush` and `fence` hooks
- transaction WAST still passes with the default `VMemory` backend
- feature-off builds still compile

Tests that require true persistence across process restart, power loss, or PMEM
device recovery must be ignored or gated with `WASMTIME_TEST_REAL_PMEM=1`.

This lets the branch grow a real PMEM backend now without pretending that an
ordinary development machine proves durability.

## Future Recovery Wave

A later recovery wave should add:

- durable region header versioning
- durable object-table roots
- commit markers or a small redo/undo protocol
- recovery scanning for committed chunks
- post-restart reconstruction of `ObjectId` and `tmemory` metadata
- PMEM-only tests that run on the actual persistent-memory machine

That recovery wave is separate from the first `NVMemory` backend wave.
