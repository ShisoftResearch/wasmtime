# Transactional Wasm Runtime Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the scalar transactional Wasm mock runtime with the first real Wizard-style runtime core: `tfunc` transaction boundaries, real `VMemory`-backed `tmemory`, copy-on-write workspace, and `LockBased` granule ownership. The storage direction follows Eliot Moss's June 5, 2026 clarification: `TMemory`, the object heap, and the object table are separate logical users of one shared block/chunk storage substrate.

**Architecture:** Ordinary Wasmtime memory remains unchanged. Transactional memory is a sidecar runtime object addressed by Wizard-style `GranuleId`; transaction operations go through libcalls that read committed `tmemory`, stage writes in a private workspace, and commit or abort through the configured transaction runtime. Storage backends are block/chunk region backends: `TMemoryRegion` maps a possibly discontiguous chunk list into a contiguous virtual linear memory reservation, while future object-heap and object-table regions reuse the same substrate with object-specific indexing. Parser and WAST harness cleanup happens after the runtime path is executable, so WAST files can move from normalized fixtures to the real engine without changing test cases.

**Tech Stack:** Rust, Wasmtime runtime internals, Cranelift translation hooks, local `wasm-tools-transaction` fork for `wasmparser`/`wat`, proposal WAST tests, `cargo test`.

---

## Scope

This plan implements the runtime core needed before object-table and structured
failure-handler work:

- Implement real `VMemory` storage for `tmemory`, first as the executable
  volatile backend and then as the first block/chunk region backend.
- Implement copy-on-write transaction workspace indexed by `GranuleId`.
- Implement `LockBased` optimistic-read/pessimistic-write ownership for
  memory granules and globals.
- Enforce active-transaction requirements for implemented scalar transaction
  operations.
- Implement Wizard-style transaction boundaries for exported/top-level `tfunc`
  and non-transactional-to-transactional `tcall`.
- Wire first `ttable.get/set/size/grow` runtime paths for funcref tables using
  `TTable` and `TTableSize` granule ownership.
- Move scalar memory/global proposal WAST execution toward real parser and real
  runtime paths.

This plan does not implement:

- Table-element COW, table bulk operators, and the full object table.
  `GranuleId::TTable` and `GranuleId::TTableSize` are used by the first funcref
  table runtime paths; `GranuleId::TStruct` and `GranuleId::TArray` remain
  reserved for object-table work. `ObjectId` is carried inside the struct/array
  granule variants, not used as a standalone ownership key.
- Transactional structs, arrays, refs, GC integration, or reference-control
  permissions.
- `ttry`/`tfail` structured failure handlers.
- Durable `FileBackedMemory` or `NVMemory` block-region storage.
- Full SIMD transactional memory operations.

Those parts remain enabled by the shapes introduced here: `GranuleId`,
backend traits, explicit concurrency-control hooks, and transaction boundary
metadata.

## Storage Design Amendment

The original runtime-core plan introduced `TMemoryBackendStorage` as the first
executable storage boundary. That remains acceptable as a transitional
milestone, but it is not the final abstraction boundary.

The next storage refactor must introduce a lower shared region layer:

- `BlockRegionBackend`: allocates, maps, protects, flushes, fences, and
  releases fixed-size aligned blocks.
- `ChunkList`: records a logical region as an ordered list of chunks, where
  each chunk is a contiguous run of blocks.
- `MappedLinearRegion`: maps a chunk list into a contiguous virtual reservation
  for linear-memory execution.
- `TMemoryRegion`: linear-memory frontend over `MappedLinearRegion`, with
  Wizard-style 256-byte transaction granules.
- `ObjectHeapRegion`: later object-payload frontend over the same block/chunk
  substrate.
- `ObjectTableRegion`: later object-table frontend whose persistent chunks are
  physically discontiguous but mapped contiguously so `ObjectId` indexes
  directly into the table.

Backend block size is a policy of `BlockRegionBackend`. The first
implementation should copy Wizard's x86-64 Immix region default:
`MemRegions.BlockSize = 512 KiB`. For `TMemoryRegion`, block size must remain a
multiple of the 64 KiB Wasm page size. The 64 KiB Wasm page remains the
grow/accounting unit, and the 256-byte Wizard granule remains the transaction
conflict/COW unit.

Copy Wizard's block-region layout names and roles directly:

- `PWRegionHeader`
- `MetaDataDesc`
- `BlockEntry`
- `ChunkHeader`
- `ListKind`
- `BlockLists`
- `LineMark`
- `ImmixLineSize = 256`

Do not merge Immix `LineMark` with transactional granule ownership metadata.
The first uses one byte per 256-byte line for allocation/GC state; the second
uses ownership/version/hash-style metadata for transaction conflict control.

## Active Storage Wave: Wizard Block Region Migration

This section supersedes the older flat-`TMemoryBackendStorage` task for the
next implementation wave. Keep existing `TMemory` public methods and WAST
behavior stable while moving the implementation under those methods to
Wizard-style block/chunk storage.

### Storage Wave Task 1: Wizard Layout Types

**Files:**
- Create: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`

- [ ] **Step 1: Add failing layout tests**

Add unit tests that assert the Wizard layout constants:

```rust
#[test]
fn wizard_layout_constants_match_reference() {
    assert_eq!(BLOCK_SIZE, 512 * 1024);
    assert_eq!(IMMIX_LINE_SIZE, 256);
    assert_eq!(PW_REGION_HEADER_SIZE, 56);
    assert_eq!(META_DATA_DESC_SIZE, 32);
    assert_eq!(BLOCK_ENTRY_SIZE, 40);
    assert_eq!(CHUNK_HEADER_SIZE, 32);
    assert_eq!(LINE_MARK_SIZE, 1);
}
```

- [ ] **Step 2: Run the failing test**

Run:

```bash
cargo test -p wasmtime --lib wizard_layout_constants_match_reference
```

Expected: FAIL because the constants/types do not exist.

- [ ] **Step 3: Implement layout constants and Rust structs**

Define Rust equivalents for Wizard's `PWRegionHeader`, `MetaDataDesc`,
`BlockEntry`, `ChunkHeader`, `ListKind`, `BlockLists`, and `LineMark`.

Keep this first step structural only. Do not wire allocation or `TMemory`
through it yet.

- [ ] **Step 4: Verify and commit**

Run:

```bash
cargo test -p wasmtime --lib wizard_layout_constants_match_reference
git diff --check
```

Commit:

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory.rs crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
git commit -m "Add transactional block region layout types"
```

### Storage Wave Task 2: Volatile Block Region Backend

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`

- [ ] **Step 1: Add failing backend allocation tests**

Add tests that create a `VMemoryBlockRegion`, assert 512 KiB block sizing,
allocate one-block and multi-block chunks, and observe initialized line marks.

- [ ] **Step 2: Run the failing tests**

Run:

```bash
cargo test -p wasmtime --lib vmemory_block_region
```

Expected: FAIL because the backend does not exist.

- [ ] **Step 3: Implement `BlockRegionBackend` and `VMemoryBlockRegion`**

Implement the first volatile backend with anonymous memory and Wizard-style
metadata layout. Include no-op `flush` and `fence` hooks so the API can later
support `FileBackedMemory` and `NVMemory`.

- [ ] **Step 4: Verify and commit**

Run:

```bash
cargo test -p wasmtime --lib vmemory_block_region
git diff --check
```

Commit:

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
git commit -m "Add volatile transactional block region"
```

### Storage Wave Task 3: Chunk List And Mapped Linear Region

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Create: `crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`

- [ ] **Step 1: Add failing mapping tests**

Add tests that build a chunk list with non-contiguous backend chunks and read it
back through contiguous logical offsets.

- [ ] **Step 2: Run the failing tests**

Run:

```bash
cargo test -p wasmtime --lib mapped_linear_region
```

Expected: FAIL because `MappedLinearRegion` does not exist.

- [ ] **Step 3: Implement `ChunkList` and `MappedLinearRegion`**

Implement logical-offset mapping over ordered chunks. For the volatile backend,
copy/read/write through backend chunk storage; do not require OS-level fixed
address remapping in this step.

- [ ] **Step 4: Verify and commit**

Run:

```bash
cargo test -p wasmtime --lib mapped_linear_region
git diff --check
```

Commit:

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory.rs crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs
git commit -m "Add mapped transactional linear region"
```

### Storage Wave Task 4: Migrate `TMemory` To `TMemoryRegion`

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs`

- [ ] **Step 1: Add regression tests around the public `TMemory` API**

Cover:

- `(tmemory 0)` stays cheap.
- grow by one page initializes 256 new transaction granules.
- commit/read across a chunk boundary preserves bytes.
- line marks remain separate from transaction granule metadata.

- [ ] **Step 2: Run regression tests before migration**

Run:

```bash
cargo test -p wasmtime --lib tmemory
```

Expected: existing flat `VMemory` tests pass, while new chunk-boundary tests may
fail until the migration exists.

- [ ] **Step 3: Move `TMemory` internals to `TMemoryRegion`**

Keep all existing callers working:

- `TMemory::new_vmemory`
- `TMemory::new_with_backend`
- `read_committed`
- `commit_range`
- `grow_to_pages`
- `granule_info`
- `set_granule_info`

- [ ] **Step 4: Verify full storage and transaction behavior**

Run:

```bash
cargo test -p wasmtime --lib tmemory
cargo test -p wasmtime --lib transaction
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tmemory.wast -- --format terse
git diff --check
```

Commit:

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory.rs crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs
git commit -m "Move tmemory onto transactional block regions"
```

## File Map

- `crates/environ/src/transaction.rs`
  - Owns transaction proposal metadata shared by validation, translation, and
    runtime planning. Add transaction function metadata and keep memory/global
    object metadata here.

- `crates/wasmtime/src/runtime/transaction.rs`
  - Owns transaction configuration, transaction state, workspace identity,
    `LockBased` concurrency control, commit/abort behavior, and tests for
    transaction state transitions.

- `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
  - Owns `TMemory`, the transitional transactional memory backend trait, and
    the first `VMemory` backend implementation. The next storage wave should
    split this file into shared block/chunk region infrastructure plus the
    `TMemoryRegion` frontend.

- `crates/wasmtime/src/runtime/vm/memory.rs`
  - Re-exports transactional memory types to the runtime VM layer while keeping
    ordinary `Memory` behavior unchanged.

- `crates/wasmtime/src/runtime/vm/instance.rs`
  - Adds per-instance transactional memory sidecar storage and accessors used
    by transaction libcalls.

- `crates/wasmtime/src/runtime/vm/libcalls.rs`
  - Routes scalar `*.tload`, `*.tstore`, `tmemory.size`, `tmemory.grow`,
    `tglobal.get`, `tglobal.set`, and funcref `ttable.get/set/size/grow` to
    the real transaction runtime.

- `crates/cranelift/src/func_environ.rs`
  - Lowers transaction memory/global operators to the typed transaction
    libcalls, and lowers the first funcref table transaction operators to
    table transaction libcalls. Removes per-operation lazy transaction starts
    after function boundary lowering is in place.

- `crates/cranelift/src/translate/code_translator.rs`
  - Keeps transaction operator dispatch explicit and prevents ordinary
    memory/global lowering from absorbing transaction-prefixed operators.

- `crates/test-util/src/wast.rs`
  - Keeps the WAST harness. Remove normalization only for files whose parser,
    validation, lowering, and runtime path are real.

- `tests/wast.rs`
  - Runs proposal WAST groups. Do not edit WAST files or test case contents.

- `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wasmparser/src/readers/core/operators.rs`
  - Local fork operator definitions for binary `0xfa` transaction operators.

- `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wat/src`
  - Local fork text parser support for transaction proposal text syntax.

- `docs/shisoft/transactional-wasm-implementation-log.md`
  - Record each mock removed and each remaining mock category.

## Current Branch Hygiene

The branch currently has pre-existing mock-tagging edits. Do not mix those edits
with runtime-core implementation commits unless executing Task 0 explicitly.

### Task 0: Protect Existing Mock-Tagging Edits

**Files:**
- Existing unstaged edits:
  - `crates/test-util/src/wast.rs`
  - `crates/wasmtime/src/runtime/transaction.rs`
  - `crates/wasmtime/src/runtime/vm/libcalls.rs`
  - `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
  - `docs/shisoft/transactional-wasm-implementation-log.md`
  - `docs/shisoft/transactional-wasm-mock-runtime-plan.md`

- [ ] **Step 1: Inspect branch state**

Run:

```bash
git branch --show-current
git status --short
```

Expected:

```text
transaction
 M crates/test-util/src/wast.rs
 M crates/wasmtime/src/runtime/transaction.rs
 M crates/wasmtime/src/runtime/vm/libcalls.rs
 M crates/wasmtime/src/runtime/vm/memory/tmemory.rs
 M docs/shisoft/transactional-wasm-implementation-log.md
 M docs/shisoft/transactional-wasm-mock-runtime-plan.md
```

- [ ] **Step 2: Review the existing mock-tagging diff**

Run:

```bash
git diff -- crates/test-util/src/wast.rs crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/libcalls.rs crates/wasmtime/src/runtime/vm/memory/tmemory.rs docs/shisoft/transactional-wasm-implementation-log.md docs/shisoft/transactional-wasm-mock-runtime-plan.md
```

Expected: the diff contains only `SHISOFT-TWASM-MOCK` comments, documentation
entries, or narrowly scoped mock-report text from the previous work.

- [ ] **Step 3: Commit the existing mock-tagging diff before runtime edits**

Run:

```bash
git add crates/test-util/src/wast.rs crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/libcalls.rs crates/wasmtime/src/runtime/vm/memory/tmemory.rs docs/shisoft/transactional-wasm-implementation-log.md docs/shisoft/transactional-wasm-mock-runtime-plan.md
git commit -m "Tag transactional Wasm mock scaffolds"
```

Expected: commit succeeds with no `Co-authored-by` trailers.

- [ ] **Step 4: Confirm a clean runtime-edit baseline**

Run:

```bash
git status --short
```

Expected: no output.

## Implementation Tasks

### Task 1: Transaction Function Metadata

**Files:**
- Modify: `crates/environ/src/transaction.rs`
- Test: `crates/environ/src/transaction.rs`

This task creates the metadata shape used by validation and lowering to know
which functions are transactional. Parser population and call-boundary lowering
use this metadata in later tasks.

- [ ] **Step 1: Add a failing metadata unit test**

Append this test to the existing test module in
`crates/environ/src/transaction.rs`, or create a `#[cfg(test)] mod tests` at the
bottom of the file if none exists:

```rust
#[test]
fn transaction_metadata_tracks_tfuncs() {
    use crate::FuncIndex;

    let mut metadata = TransactionObjectMetadata::default();
    let f0 = FuncIndex::from_u32(0);
    let f1 = FuncIndex::from_u32(1);

    assert!(!metadata.is_tfunc(f0));
    assert!(!metadata.is_tfunc(f1));

    metadata.add_tfunc(f0);
    metadata.add_tfunc(f0);

    assert!(metadata.is_tfunc(f0));
    assert!(!metadata.is_tfunc(f1));
    assert_eq!(metadata.tfuncs().collect::<Vec<_>>(), vec![f0]);
}
```

- [ ] **Step 2: Run the failing metadata test**

Run:

```bash
cargo test -p wasmtime-environ transaction_metadata_tracks_tfuncs
```

Expected: FAIL with missing `add_tfunc`, `is_tfunc`, or `tfuncs`.

- [ ] **Step 3: Implement transaction function metadata**

Modify `crates/environ/src/transaction.rs` so imports include `FuncIndex`:

```rust
use crate::{FuncIndex, GlobalIndex, MemoryIndex};
```

Extend `TransactionObjectMetadata`:

```rust
#[derive(Clone, Debug, Default)]
pub struct TransactionObjectMetadata {
    memories: Vec<MemoryIndex>,
    globals: Vec<GlobalIndex>,
    functions: Vec<FuncIndex>,
}
```

Add methods on the existing `impl TransactionObjectMetadata` block:

```rust
pub fn add_tfunc(&mut self, index: FuncIndex) {
    if !self.functions.contains(&index) {
        self.functions.push(index);
    }
}

pub fn is_tfunc(&self, index: FuncIndex) -> bool {
    self.functions.contains(&index)
}

pub fn tfuncs(&self) -> impl ExactSizeIterator<Item = FuncIndex> + '_ {
    self.functions.iter().copied()
}
```

If the file already exposes memory/global iterators, keep naming consistent with
those methods and use the same return style.

- [ ] **Step 4: Run metadata tests**

Run:

```bash
cargo test -p wasmtime-environ transaction_metadata_tracks_tfuncs
```

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```bash
git add crates/environ/src/transaction.rs
git commit -m "Track transactional function metadata"
```

Expected: commit succeeds with no unrelated files staged.

### Task 2: TMemory Backend Trait And VMemory Implementation

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory.rs`
- Test: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`

This task makes `TMemory` a real backend-backed storage object. The first
backend is `VMemory`; `FileBackedMemory` and `NVMemory` remain configuration
names that produce a deterministic unsupported-backend error when selected.

- [ ] **Step 1: Add failing backend tests**

Add this test module to `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vmemory_commit_and_read_committed_bytes() {
        let mut memory = TMemory::new_vmemory(2).expect("create tmemory");

        assert_eq!(memory.byte_len(), 2 * wasmtime_environ::WASM_PAGE_SIZE as usize);
        assert_eq!(memory.backend_kind(), crate::runtime::transaction::TMemoryBackend::VMemory);

        memory.commit_range(8, &[1, 2, 3, 4]).expect("commit bytes");
        assert_eq!(memory.read_committed(6..12).expect("read bytes"), vec![0, 0, 1, 2, 3, 4]);
    }

    #[test]
    fn vmemory_grow_initializes_new_granule_metadata() {
        let mut memory = TMemory::new_vmemory(1).expect("create tmemory");
        let before = memory.granule_count();

        memory.grow_to_pages(2).expect("grow tmemory");

        assert!(memory.granule_count() > before);
        let info = memory.granule_info(before).expect("new granule info");
        assert_eq!(info.version, 0);
        assert!(info.owner.is_none());
    }

    #[test]
    fn file_backed_and_nv_backends_are_explicitly_unsupported() {
        let file_backed = TMemory::new_with_backend(
            crate::runtime::transaction::TMemoryBackend::FileBackedMemory,
            1,
        );
        assert!(file_backed.is_err());

        let nv = TMemory::new_with_backend(
            crate::runtime::transaction::TMemoryBackend::NVMemory,
            1,
        );
        assert!(nv.is_err());
    }
}
```

- [ ] **Step 2: Run failing backend tests**

Run:

```bash
cargo test -p wasmtime --lib vmemory_commit_and_read_committed_bytes
cargo test -p wasmtime --lib vmemory_grow_initializes_new_granule_metadata
cargo test -p wasmtime --lib file_backed_and_nv_backends_are_explicitly_unsupported
```

Expected: FAIL with missing `new_vmemory`, `new_with_backend`, `byte_len`,
`backend_kind`, `commit_range`, `read_committed`, `grow_to_pages`, or
`granule_info`.

- [ ] **Step 3: Add backend trait and VMemory implementation**

In `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`, define this trait near
the storage types:

```rust
use anyhow::{bail, ensure, Result};
use std::fmt;
use std::ops::Range;

pub(crate) trait TMemoryBackendStorage: fmt::Debug + Send + Sync {
    fn backend_kind(&self) -> crate::runtime::transaction::TMemoryBackend;
    fn byte_len(&self) -> usize;
    fn byte_capacity(&self) -> usize;
    fn granule_count(&self) -> usize;
    fn read_committed(&self, range: Range<usize>) -> Result<Vec<u8>>;
    fn commit_range(&mut self, addr: usize, bytes: &[u8]) -> Result<()>;
    fn grow_to_pages(&mut self, new_pages: u64) -> Result<()>;
    fn granule_info(&self, granule: usize) -> Result<TMemoryGranuleInfo>;
    fn set_granule_info(&mut self, granule: usize, info: TMemoryGranuleInfo) -> Result<()>;
}
```

Make `TMemoryGranuleInfo` carry explicit owner and version fields:

```rust
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TMemoryGranuleInfo {
    pub owner: Option<crate::runtime::transaction::TransactionId>,
    pub version: u64,
}
```

Represent `TMemory` as a trait object:

```rust
#[derive(Debug)]
pub(crate) struct TMemory {
    storage: Box<dyn TMemoryBackendStorage>,
}
```

Add `TMemory` constructors and forwarding methods:

```rust
impl TMemory {
    pub(crate) fn new_with_backend(
        backend: crate::runtime::transaction::TMemoryBackend,
        min_pages: u64,
    ) -> Result<Self> {
        match backend {
            crate::runtime::transaction::TMemoryBackend::VMemory => Self::new_vmemory(min_pages),
            crate::runtime::transaction::TMemoryBackend::FileBackedMemory => {
                bail!("FileBackedMemory backend is not implemented for tmemory")
            }
            crate::runtime::transaction::TMemoryBackend::NVMemory => {
                bail!("NVMemory backend is not implemented for tmemory")
            }
        }
    }

    pub(crate) fn new_vmemory(min_pages: u64) -> Result<Self> {
        Ok(Self {
            storage: Box::new(VMemory::new(min_pages)?),
        })
    }

    pub(crate) fn backend_kind(&self) -> crate::runtime::transaction::TMemoryBackend {
        self.storage.backend_kind()
    }

    pub(crate) fn byte_len(&self) -> usize {
        self.storage.byte_len()
    }

    pub(crate) fn byte_capacity(&self) -> usize {
        self.storage.byte_capacity()
    }

    pub(crate) fn granule_count(&self) -> usize {
        self.storage.granule_count()
    }

    pub(crate) fn read_committed(&self, range: Range<usize>) -> Result<Vec<u8>> {
        self.storage.read_committed(range)
    }

    pub(crate) fn commit_range(&mut self, addr: usize, bytes: &[u8]) -> Result<()> {
        self.storage.commit_range(addr, bytes)
    }

    pub(crate) fn grow_to_pages(&mut self, new_pages: u64) -> Result<()> {
        self.storage.grow_to_pages(new_pages)
    }

    pub(crate) fn granule_info(&self, granule: usize) -> Result<TMemoryGranuleInfo> {
        self.storage.granule_info(granule)
    }

    pub(crate) fn set_granule_info(
        &mut self,
        granule: usize,
        info: TMemoryGranuleInfo,
    ) -> Result<()> {
        self.storage.set_granule_info(granule, info)
    }
}
```

Implement `TMemoryBackendStorage` for the existing `VMemory` type. The methods
must use the existing mmap data buffer and granule metadata vector. The read and
commit methods must check bounds with `ensure!`:

```rust
impl TMemoryBackendStorage for VMemory {
    fn backend_kind(&self) -> crate::runtime::transaction::TMemoryBackend {
        crate::runtime::transaction::TMemoryBackend::VMemory
    }

    fn byte_len(&self) -> usize {
        self.len
    }

    fn byte_capacity(&self) -> usize {
        self.capacity
    }

    fn granule_count(&self) -> usize {
        self.granules.len()
    }

    fn read_committed(&self, range: Range<usize>) -> Result<Vec<u8>> {
        ensure!(range.start <= range.end, "invalid tmemory read range");
        ensure!(range.end <= self.len, "tmemory read out of bounds");
        Ok(self.data[range].to_vec())
    }

    fn commit_range(&mut self, addr: usize, bytes: &[u8]) -> Result<()> {
        let end = addr
            .checked_add(bytes.len())
            .ok_or_else(|| anyhow::anyhow!("tmemory write address overflow"))?;
        ensure!(end <= self.len, "tmemory write out of bounds");
        self.data[addr..end].copy_from_slice(bytes);
        Ok(())
    }

    fn grow_to_pages(&mut self, new_pages: u64) -> Result<()> {
        self.grow(new_pages)
    }

    fn granule_info(&self, granule: usize) -> Result<TMemoryGranuleInfo> {
        self.granules
            .get(granule)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("tmemory granule index out of bounds"))
    }

    fn set_granule_info(&mut self, granule: usize, info: TMemoryGranuleInfo) -> Result<()> {
        let slot = self
            .granules
            .get_mut(granule)
            .ok_or_else(|| anyhow::anyhow!("tmemory granule index out of bounds"))?;
        *slot = info;
        Ok(())
    }
}
```

If the existing field names differ, keep the same method semantics and update
the code to match the current struct fields.

- [ ] **Step 4: Re-export tmemory module types inside the VM memory module**

In `crates/wasmtime/src/runtime/vm/memory.rs`, ensure the module is accessible
to `instance.rs` and `libcalls.rs`:

```rust
#[cfg(has_virtual_memory)]
pub(crate) mod tmemory;
```

Keep ordinary `Memory` exports and direct-memory store paths unchanged.

- [ ] **Step 5: Run backend tests**

Run:

```bash
cargo test -p wasmtime --lib vmemory_commit_and_read_committed_bytes
cargo test -p wasmtime --lib vmemory_grow_initializes_new_granule_metadata
cargo test -p wasmtime --lib file_backed_and_nv_backends_are_explicitly_unsupported
```

Expected: PASS.

- [ ] **Step 6: Commit**

Run:

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory.rs crates/wasmtime/src/runtime/vm/memory.rs
git commit -m "Add VMemory tmemory backend"
```

Expected: commit succeeds with no unrelated files staged.

### Task 3: GranuleId And Copy-On-Write Workspace

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `crates/wasmtime/src/runtime/transaction.rs`

This task replaces memory/global-specific workspace keys with the Wizard-style
identity shape. `GranuleId::TTable` and `GranuleId::TTableSize` are now used by
the first funcref `ttable` runtime paths; table COW, `GranuleId::TStruct`, and
`GranuleId::TArray` remain for future table/object-table work.

- [ ] **Step 1: Add failing GranuleId and workspace tests**

Add these tests to the existing test module in
`crates/wasmtime/src/runtime/transaction.rs`:

```rust
#[test]
fn granule_id_orders_by_object_space_and_index() {
    let a = GranuleId::TMemory {
        instance: 7,
        memory_index: 0,
        granule_index: 1,
    };
    let b = GranuleId::TMemory {
        instance: 7,
        memory_index: 0,
        granule_index: 2,
    };
    let c = GranuleId::TGlobal {
        instance: 7,
        global_index: 0,
    };

    let mut ids = vec![b, c, a];
    ids.sort();

    assert_eq!(ids[0], a);
    assert_eq!(ids[1], b);
    assert_eq!(ids[2], c);
}

#[test]
fn workspace_merges_staged_and_committed_granules() {
    let mut tx = TransactionState::new_for_test(TransactionId::from_raw(1));
    let first = GranuleId::TMemory {
        instance: 3,
        memory_index: 0,
        granule_index: 0,
    };
    let second = GranuleId::TMemory {
        instance: 3,
        memory_index: 0,
        granule_index: 1,
    };

    tx.stage_granule_for_test(first, vec![10; TMEMORY_GRANULE_SIZE]);
    tx.stage_granule_for_test(second, vec![20; TMEMORY_GRANULE_SIZE]);

    let committed = vec![0; TMEMORY_GRANULE_SIZE * 2];
    let read = tx
        .read_tmemory_range_for_test(3, 0, 250, 12, &committed)
        .expect("merged read");

    assert_eq!(read, vec![10, 10, 10, 10, 10, 10, 20, 20, 20, 20, 20, 20]);
}
```

- [ ] **Step 2: Run failing workspace tests**

Run:

```bash
cargo test -p wasmtime --lib granule_id_orders_by_object_space_and_index
cargo test -p wasmtime --lib workspace_merges_staged_and_committed_granules
```

Expected: FAIL with missing `GranuleId`, `new_for_test`,
`stage_granule_for_test`, or `read_tmemory_range_for_test`.

- [ ] **Step 3: Define TransactionId raw helpers and GranuleId**

In `crates/wasmtime/src/runtime/transaction.rs`, make `TransactionId` usable by
storage metadata without exposing store internals:

```rust
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct TransactionId(u64);

impl TransactionId {
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub(crate) fn as_raw(self) -> u64 {
        self.0
    }
}
```

Add `GranuleId` near the existing transaction object key definitions:

```rust
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum GranuleId {
    TMemory {
        instance: u32,
        memory_index: u32,
        granule_index: u64,
    },
    TMemorySize {
        instance: u32,
        memory_index: u32,
    },
    TGlobal {
        instance: u32,
        global_index: u32,
    },
}
```

Use `u32` instance identity in this phase because the runtime already stores
Wasmtime instance identity in integer form for the current mock path.

- [ ] **Step 4: Convert staged workspace maps to GranuleId**

In `TransactionState`, replace memory-specific staged maps with:

```rust
staged_granules: BTreeMap<GranuleId, Vec<u8>>,
read_versions: BTreeMap<GranuleId, u64>,
write_owned: BTreeSet<GranuleId>,
```

Keep scalar global snapshots if they already exist, but index transactional
globals with `GranuleId::TGlobal` for read/write ownership. If the existing
code has `staged_memory_granules`, migrate all reads and writes to
`staged_granules`.

- [ ] **Step 5: Implement workspace range helpers**

Add these private helpers on `TransactionState`:

```rust
fn tmemory_granule(instance: u32, memory_index: u32, addr: usize) -> GranuleId {
    GranuleId::TMemory {
        instance,
        memory_index,
        granule_index: (addr / TMEMORY_GRANULE_SIZE) as u64,
    }
}

fn stage_tmemory_granule(&mut self, id: GranuleId, bytes: Vec<u8>) {
    debug_assert_eq!(bytes.len(), TMEMORY_GRANULE_SIZE);
    self.staged_granules.insert(id, bytes);
}

fn read_staged_tmemory_range(
    &self,
    instance: u32,
    memory_index: u32,
    addr: usize,
    len: usize,
    committed: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let end = addr
        .checked_add(len)
        .ok_or_else(|| anyhow::anyhow!("transactional memory read address overflow"))?;
    anyhow::ensure!(end <= committed.len(), "transactional memory read out of bounds");

    let mut out = Vec::with_capacity(len);
    let mut cursor = addr;
    while cursor < end {
        let granule_id = Self::tmemory_granule(instance, memory_index, cursor);
        let granule_start = cursor / TMEMORY_GRANULE_SIZE * TMEMORY_GRANULE_SIZE;
        let offset = cursor - granule_start;
        let chunk = (TMEMORY_GRANULE_SIZE - offset).min(end - cursor);

        if let Some(bytes) = self.staged_granules.get(&granule_id) {
            out.extend_from_slice(&bytes[offset..offset + chunk]);
        } else {
            out.extend_from_slice(&committed[cursor..cursor + chunk]);
        }
        cursor += chunk;
    }
    Ok(out)
}
```

Add test-only wrappers matching the test names:

```rust
#[cfg(test)]
impl TransactionState {
    fn new_for_test(id: TransactionId) -> Self {
        Self::new(id)
    }

    fn stage_granule_for_test(&mut self, id: GranuleId, bytes: Vec<u8>) {
        self.stage_tmemory_granule(id, bytes);
    }

    fn read_tmemory_range_for_test(
        &self,
        instance: u32,
        memory_index: u32,
        addr: usize,
        len: usize,
        committed: &[u8],
    ) -> anyhow::Result<Vec<u8>> {
        self.read_staged_tmemory_range(instance, memory_index, addr, len, committed)
    }
}
```

- [ ] **Step 6: Update existing transaction code to use GranuleId**

Replace existing `MemoryGranuleKey`, `MemoryObjectKey`, and `GlobalObjectKey`
usage in `crates/wasmtime/src/runtime/transaction.rs` with `GranuleId`. Keep
compatibility helper constructors during the refactor if they reduce churn, but
their return type must be `GranuleId`.

- [ ] **Step 7: Run transaction workspace tests**

Run:

```bash
cargo test -p wasmtime --lib granule_id_orders_by_object_space_and_index
cargo test -p wasmtime --lib workspace_merges_staged_and_committed_granules
cargo test -p wasmtime --lib mock_transaction_
```

Expected: PASS. Existing mock transaction tests should keep passing until the
libcall path is replaced by real `TMemory`.

- [ ] **Step 8: Commit**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs
git commit -m "Index transaction workspace by granule id"
```

Expected: commit succeeds with no unrelated files staged.

### Task 4: LockBased Ownership And Version Validation

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Test: `crates/wasmtime/src/runtime/transaction.rs`

This task makes the first concurrency-control implementation real for granules:
optimistic reads record a version, writes take ownership, commit validates
reads, and abort releases ownership without committing staged bytes.

- [ ] **Step 1: Add failing LockBased tests**

Add these tests to `crates/wasmtime/src/runtime/transaction.rs`:

```rust
#[test]
fn lock_based_write_conflict_aborts_current_transaction() {
    let mut cc = LockBased::default();
    let id = GranuleId::TMemory {
        instance: 1,
        memory_index: 0,
        granule_index: 0,
    };
    let first = TransactionId::from_raw(1);
    let second = TransactionId::from_raw(2);

    cc.acquire_write_for_test(first, id, 0).expect("first writer");
    let err = cc.acquire_write_for_test(second, id, 0).expect_err("second writer conflicts");

    assert!(err.to_string().contains("transaction write conflict"));
    assert_eq!(cc.owner_for_test(id), Some(first));
}

#[test]
fn lock_based_validates_optimistic_reads_at_commit() {
    let mut cc = LockBased::default();
    let id = GranuleId::TMemory {
        instance: 1,
        memory_index: 0,
        granule_index: 0,
    };
    let tx = TransactionId::from_raw(1);

    cc.record_read_for_test(tx, id, 4).expect("record read");
    assert!(cc.validate_read_for_test(tx, id, 4).is_ok());

    let err = cc.validate_read_for_test(tx, id, 5).expect_err("version changed");
    assert!(err.to_string().contains("transaction read conflict"));
}

#[test]
fn lock_based_abort_releases_owned_granules() {
    let mut cc = LockBased::default();
    let id = GranuleId::TMemory {
        instance: 1,
        memory_index: 0,
        granule_index: 0,
    };
    let tx = TransactionId::from_raw(1);

    cc.acquire_write_for_test(tx, id, 0).expect("writer");
    cc.abort_for_test(tx);

    assert_eq!(cc.owner_for_test(id), None);
}
```

- [ ] **Step 2: Run failing LockBased tests**

Run:

```bash
cargo test -p wasmtime --lib lock_based_write_conflict_aborts_current_transaction
cargo test -p wasmtime --lib lock_based_validates_optimistic_reads_at_commit
cargo test -p wasmtime --lib lock_based_abort_releases_owned_granules
```

Expected: FAIL with missing `acquire_write_for_test`, `record_read_for_test`,
`validate_read_for_test`, `abort_for_test`, or `owner_for_test`.

- [ ] **Step 3: Implement LockBased state**

In `crates/wasmtime/src/runtime/transaction.rs`, make `LockBased` own maps by
`GranuleId`:

```rust
#[derive(Debug, Default)]
pub(crate) struct LockBased {
    owners: BTreeMap<GranuleId, TransactionId>,
    read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
}
```

Add methods:

```rust
impl LockBased {
    pub(crate) fn record_read(
        &mut self,
        tx: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> anyhow::Result<()> {
        if let Some(owner) = self.owners.get(&granule) {
            anyhow::ensure!(
                *owner == tx,
                "transaction read conflict on granule {granule:?}"
            );
        }
        self.read_versions.insert((tx, granule), version);
        Ok(())
    }

    pub(crate) fn acquire_write(
        &mut self,
        tx: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> anyhow::Result<()> {
        if let Some(owner) = self.owners.get(&granule) {
            anyhow::ensure!(
                *owner == tx,
                "transaction write conflict on granule {granule:?}"
            );
            return Ok(());
        }

        if let Some(recorded) = self.read_versions.get(&(tx, granule)) {
            anyhow::ensure!(
                *recorded == current_version,
                "transaction read conflict on granule {granule:?}"
            );
        }

        self.owners.insert(granule, tx);
        Ok(())
    }

    pub(crate) fn validate_read(
        &self,
        tx: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> anyhow::Result<()> {
        if self.owners.get(&granule).copied() == Some(tx) {
            return Ok(());
        }

        if let Some(recorded) = self.read_versions.get(&(tx, granule)) {
            anyhow::ensure!(
                *recorded == current_version,
                "transaction read conflict on granule {granule:?}"
            );
        }
        Ok(())
    }

    pub(crate) fn release_transaction(&mut self, tx: TransactionId) {
        self.owners.retain(|_, owner| *owner != tx);
        self.read_versions.retain(|(reader, _), _| *reader != tx);
    }
}
```

Add test-only wrappers:

```rust
#[cfg(test)]
impl LockBased {
    fn record_read_for_test(
        &mut self,
        tx: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> anyhow::Result<()> {
        self.record_read(tx, granule, version)
    }

    fn acquire_write_for_test(
        &mut self,
        tx: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> anyhow::Result<()> {
        self.acquire_write(tx, granule, version)
    }

    fn validate_read_for_test(
        &self,
        tx: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> anyhow::Result<()> {
        self.validate_read(tx, granule, version)
    }

    fn abort_for_test(&mut self, tx: TransactionId) {
        self.release_transaction(tx);
    }

    fn owner_for_test(&self, granule: GranuleId) -> Option<TransactionId> {
        self.owners.get(&granule).copied()
    }
}
```

- [ ] **Step 4: Connect LockBased to TransactionState commit and abort**

In transaction commit and abort paths, call:

```rust
self.lock_based.release_transaction(transaction_id);
```

after commit copies staged data and after abort discards staged data. If
`LockBased` is stored behind a runtime enum, put the release call in the enum
method for `ConcurrencyControl::LockBased`.

- [ ] **Step 5: Increment granule versions after committed writes**

In `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`, after a commit writes
bytes that touch a granule, increment that granule's `TMemoryGranuleInfo.version`
exactly once per committed transaction for that granule:

```rust
let mut info = self.granule_info(granule)?;
info.version = info.version.wrapping_add(1);
info.owner = None;
self.set_granule_info(granule, info)?;
```

Keep ownership release centralized through `LockBased`; the metadata owner is a
mirror for backend inspection and future backend integration.

- [ ] **Step 6: Run LockBased tests**

Run:

```bash
cargo test -p wasmtime --lib lock_based_write_conflict_aborts_current_transaction
cargo test -p wasmtime --lib lock_based_validates_optimistic_reads_at_commit
cargo test -p wasmtime --lib lock_based_abort_releases_owned_granules
cargo test -p wasmtime --lib mock_transaction_
```

Expected: PASS.

- [ ] **Step 7: Commit**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/memory/tmemory.rs
git commit -m "Implement lock based transaction ownership"
```

Expected: commit succeeds with no unrelated files staged.

### Task 5: Instance TMemory Sidecar

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/instance.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `crates/wasmtime/src/runtime/vm/instance.rs` or the closest existing
  runtime VM test module for instance construction.

This task wires `tmemory` into Wasmtime instances without changing ordinary
linear-memory behavior. Transaction libcalls will consume these accessors in
Task 6.

- [ ] **Step 1: Inspect current instance memory allocation paths**

Run:

```bash
rg -n "struct Instance|defined_memories|get_defined_memory|MemoryIndex|memories" crates/wasmtime/src/runtime/vm/instance.rs crates/wasmtime/src/runtime/vm -g '*.rs'
```

Expected: output identifies the instance struct fields and existing memory
accessors. Use those exact locations for the next steps.

- [ ] **Step 2: Add failing sidecar access test**

Add a unit test in the instance test module if one exists. If `instance.rs` has
no test module, add a focused test to `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
that validates the sidecar container type introduced in Step 3:

```rust
#[test]
fn transaction_memory_sidecar_resolves_by_memory_index() {
    let mut sidecar = TMemorySidecar::default();
    sidecar.insert(0, TMemory::new_vmemory(1).expect("tmemory 0"));
    sidecar.insert(2, TMemory::new_vmemory(1).expect("tmemory 2"));

    assert!(sidecar.get(0).is_some());
    assert!(sidecar.get(1).is_none());
    assert!(sidecar.get(2).is_some());
}
```

- [ ] **Step 3: Implement the sidecar container**

In `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`, add:

```rust
#[derive(Debug, Default)]
pub(crate) struct TMemorySidecar {
    memories: BTreeMap<u32, TMemory>,
}

impl TMemorySidecar {
    pub(crate) fn insert(&mut self, memory_index: u32, memory: TMemory) -> Option<TMemory> {
        self.memories.insert(memory_index, memory)
    }

    pub(crate) fn get(&self, memory_index: u32) -> Option<&TMemory> {
        self.memories.get(&memory_index)
    }

    pub(crate) fn get_mut(&mut self, memory_index: u32) -> Option<&mut TMemory> {
        self.memories.get_mut(&memory_index)
    }

    pub(crate) fn contains(&self, memory_index: u32) -> bool {
        self.memories.contains_key(&memory_index)
    }
}
```

Add `use std::collections::BTreeMap;` if the file does not already import it.

- [ ] **Step 4: Add sidecar field to Instance**

In `crates/wasmtime/src/runtime/vm/instance.rs`, add a field to the instance
state that is stored alongside existing defined memories:

```rust
#[cfg(has_virtual_memory)]
transaction_memories: crate::runtime::vm::memory::tmemory::TMemorySidecar,
```

Initialize it with `TMemorySidecar::default()` in the existing instance
constructor. Do not remove or alter ordinary memory fields.

- [ ] **Step 5: Populate sidecar from transaction metadata**

In the instance construction path, after ordinary memories are allocated and
after module transaction metadata is available, insert `TMemory` objects for
transactional memories:

```rust
#[cfg(has_virtual_memory)]
for memory_index in module.translation().transaction_objects.memories() {
    let index = memory_index.as_u32();
    let initial_pages = module
        .memory(memory_index)
        .minimum_byte_size()
        / wasmtime_environ::WASM_PAGE_SIZE as u64;
    let tmemory = crate::runtime::vm::memory::tmemory::TMemory::new_with_backend(
        store.transaction_config().tmemory_backend(),
        initial_pages,
    )?;
    instance.transaction_memories.insert(index, tmemory);
}
```

Adjust access to `module.translation()`, `module.memory`, and
`store.transaction_config()` to match the actual types discovered in Step 1. The
behavior must be: create a sidecar `TMemory` for every metadata-listed
transactional memory and leave ordinary memories unchanged.

- [ ] **Step 6: Add instance accessors**

Add methods on `Instance`:

```rust
#[cfg(has_virtual_memory)]
pub(crate) fn get_tmemory(
    &self,
    memory_index: wasmtime_environ::MemoryIndex,
) -> Option<&crate::runtime::vm::memory::tmemory::TMemory> {
    self.transaction_memories.get(memory_index.as_u32())
}

#[cfg(has_virtual_memory)]
pub(crate) fn get_tmemory_mut(
    &mut self,
    memory_index: wasmtime_environ::MemoryIndex,
) -> Option<&mut crate::runtime::vm::memory::tmemory::TMemory> {
    self.transaction_memories.get_mut(memory_index.as_u32())
}
```

If mutable instance access is unavailable where libcalls run, store the sidecar
behind the same interior-mutability primitive used by existing runtime VM state.

- [ ] **Step 7: Run sidecar tests and build runtime**

Run:

```bash
cargo test -p wasmtime --lib transaction_memory_sidecar_resolves_by_memory_index
cargo check -p wasmtime
```

Expected: PASS.

- [ ] **Step 8: Commit**

Run:

```bash
git add crates/wasmtime/src/runtime/vm/instance.rs crates/wasmtime/src/runtime/vm/memory/tmemory.rs crates/wasmtime/src/runtime/vm/memory.rs crates/wasmtime/src/runtime/transaction.rs
git commit -m "Wire tmemory sidecar into instances"
```

Expected: commit succeeds with no unrelated files staged.

### Task 6: Real TMemory Libcalls

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Test: `crates/wasmtime/src/runtime/vm/libcalls.rs` or existing runtime
  transaction test module.

This task removes the ordinary-memory stand-in for scalar transactional memory
operators. Loads read staged granules first and committed `tmemory` second.
Stores acquire write ownership and stage bytes. Commit copies staged granules
into `tmemory`; abort discards them.

- [ ] **Step 1: Add failing real-tmemory transaction tests**

Add focused runtime tests near the existing mock transaction tests in
`crates/wasmtime/src/runtime/transaction.rs`:

```rust
#[test]
fn transaction_commit_copies_staged_granules_to_tmemory() {
    let mut tmemory = crate::runtime::vm::memory::tmemory::TMemory::new_vmemory(1)
        .expect("tmemory");
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(1));

    state
        .stage_tmemory_write_for_test(0, 0, 4, &[9, 8, 7, 6], &tmemory)
        .expect("stage write");
    state
        .commit_tmemory_for_test(&mut tmemory)
        .expect("commit write");

    assert_eq!(tmemory.read_committed(4..8).expect("read committed"), vec![9, 8, 7, 6]);
}

#[test]
fn transaction_abort_discards_staged_tmemory_bytes() {
    let mut tmemory = crate::runtime::vm::memory::tmemory::TMemory::new_vmemory(1)
        .expect("tmemory");
    let mut state = TransactionState::new_for_test(TransactionId::from_raw(1));

    state
        .stage_tmemory_write_for_test(0, 0, 4, &[9, 8, 7, 6], &tmemory)
        .expect("stage write");
    state.abort_for_test();

    assert_eq!(tmemory.read_committed(4..8).expect("read committed"), vec![0, 0, 0, 0]);
}
```

- [ ] **Step 2: Run failing real-tmemory tests**

Run:

```bash
cargo test -p wasmtime --lib transaction_commit_copies_staged_granules_to_tmemory
cargo test -p wasmtime --lib transaction_abort_discards_staged_tmemory_bytes
```

Expected: FAIL with missing test helpers or missing commit behavior.

- [ ] **Step 3: Implement staging from committed tmemory granules**

In `TransactionState`, add a method that stages writes by complete granule:

```rust
pub(crate) fn stage_tmemory_write(
    &mut self,
    instance: u32,
    memory_index: u32,
    addr: usize,
    bytes: &[u8],
    tmemory: &crate::runtime::vm::memory::tmemory::TMemory,
) -> anyhow::Result<()> {
    let end = addr
        .checked_add(bytes.len())
        .ok_or_else(|| anyhow::anyhow!("transactional memory write address overflow"))?;
    anyhow::ensure!(end <= tmemory.byte_len(), "transactional memory write out of bounds");

    let mut cursor = addr;
    let mut input = 0;
    while cursor < end {
        let granule_start = cursor / TMEMORY_GRANULE_SIZE * TMEMORY_GRANULE_SIZE;
        let granule_end = (granule_start + TMEMORY_GRANULE_SIZE).min(tmemory.byte_len());
        let granule_id = GranuleId::TMemory {
            instance,
            memory_index,
            granule_index: (granule_start / TMEMORY_GRANULE_SIZE) as u64,
        };
        let offset = cursor - granule_start;
        let chunk = (granule_end - cursor).min(end - cursor);

        let staged = self.staged_granules.entry(granule_id).or_insert_with(|| {
            let mut granule = vec![0; TMEMORY_GRANULE_SIZE];
            let committed = tmemory
                .read_committed(granule_start..granule_end)
                .expect("tmemory bounds were checked");
            granule[..committed.len()].copy_from_slice(&committed);
            granule
        });
        staged[offset..offset + chunk].copy_from_slice(&bytes[input..input + chunk]);

        cursor += chunk;
        input += chunk;
    }
    Ok(())
}
```

Wire `LockBased::acquire_write` into this method before mutating each staged
granule. Use the granule's current `TMemoryGranuleInfo.version`.

- [ ] **Step 4: Implement commit to tmemory**

Add a commit method that copies staged `TMemory` granules into a supplied
`TMemory`:

```rust
pub(crate) fn commit_tmemory(
    &mut self,
    instance: u32,
    memory_index: u32,
    tmemory: &mut crate::runtime::vm::memory::tmemory::TMemory,
) -> anyhow::Result<()> {
    let mut committed = Vec::new();
    for (id, bytes) in self.staged_granules.iter() {
        if let GranuleId::TMemory {
            instance: id_instance,
            memory_index: id_memory,
            granule_index,
        } = *id
        {
            if id_instance == instance && id_memory == memory_index {
                let addr = granule_index as usize * TMEMORY_GRANULE_SIZE;
                let end = (addr + TMEMORY_GRANULE_SIZE).min(tmemory.byte_len());
                tmemory.commit_range(addr, &bytes[..end - addr])?;
                committed.push(*id);
            }
        }
    }

    for id in committed {
        self.staged_granules.remove(&id);
    }
    Ok(())
}
```

After each granule commit, increment its version and clear its backend owner as
defined in Task 4.

- [ ] **Step 5: Add test-only wrappers**

Add wrappers matching the tests:

```rust
#[cfg(test)]
impl TransactionState {
    fn stage_tmemory_write_for_test(
        &mut self,
        instance: u32,
        memory_index: u32,
        addr: usize,
        bytes: &[u8],
        tmemory: &crate::runtime::vm::memory::tmemory::TMemory,
    ) -> anyhow::Result<()> {
        self.stage_tmemory_write(instance, memory_index, addr, bytes, tmemory)
    }

    fn commit_tmemory_for_test(
        &mut self,
        tmemory: &mut crate::runtime::vm::memory::tmemory::TMemory,
    ) -> anyhow::Result<()> {
        self.commit_tmemory(0, 0, tmemory)
    }

    fn abort_for_test(&mut self) {
        self.staged_granules.clear();
        self.read_versions.clear();
        self.write_owned.clear();
    }
}
```

- [ ] **Step 6: Replace ordinary-memory tmemory libcalls**

In `crates/wasmtime/src/runtime/vm/libcalls.rs`, update scalar transaction
memory libcalls so they:

1. Require an active transaction.
2. Resolve the target with `Instance::get_tmemory` or `Instance::get_tmemory_mut`.
3. For loads, call a transaction-state read method that merges staged bytes and
   committed `tmemory` bytes by `GranuleId`.
4. For stores, call `stage_tmemory_write`.
5. For `tmemory.size`, return committed `tmemory.byte_len() / WASM_PAGE_SIZE`.
6. For `tmemory.grow`, grow committed `tmemory` immediately and keep it
   non-rollbackable.

The active-transaction error text should include:

```text
transaction operation requires an active transaction
```

The wrong-memory-space error text should include:

```text
transactional memory operation targeted non-transactional memory
```

- [ ] **Step 7: Remove the ordinary-memory mock tag from scalar tmemory libcalls**

In `crates/wasmtime/src/runtime/vm/libcalls.rs`, remove
`SHISOFT-TWASM-MOCK` comments only from the scalar `tmemory` load/store/size/grow
path that now uses real `TMemory`. Keep mock tags on unrelated paths such as
transactional refs, object table, or `ttry`/`tfail`.

- [ ] **Step 8: Run real-tmemory tests and focused WAST smoke**

Run:

```bash
cargo test -p wasmtime --lib transaction_commit_copies_staged_granules_to_tmemory
cargo test -p wasmtime --lib transaction_abort_discards_staged_tmemory_bytes
cargo test -p wasmtime --lib mock_transaction_
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
```

Expected: unit tests PASS. WAST may still have ignored files or parser-normalized
files until Tasks 8 and 9; failures in scalar memory/global files should be
investigated before proceeding.

- [ ] **Step 9: Commit**

Run:

```bash
git add crates/wasmtime/src/runtime/vm/libcalls.rs crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/memory/tmemory.rs
git commit -m "Use real tmemory for scalar transaction ops"
```

Expected: commit succeeds with no unrelated files staged.

### Task 7: Transaction Boundary Semantics

**Files:**
- Modify: `crates/cranelift/src/func_environ.rs`
- Modify: `crates/cranelift/src/translate/code_translator.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Test: `crates/wasmtime/tests/all/transaction.rs` if that directory has
  runtime integration tests; otherwise use the existing transaction runtime test
  module and focused WAST runs.

This task replaces per-operation lazy transaction starts with Wizard boundaries:
top-level/exported `tfunc` starts a transaction; non-transactional-to-transactional
`tcall` starts a transaction around the callee; nested transactional calls reuse
the current transaction.

- [ ] **Step 1: Add failing active-transaction tests**

Create `crates/wasmtime/tests/all/transaction.rs` if runtime integration tests
are enabled from that directory. If that path is not compiled by the workspace,
place these tests in the nearest existing `crates/wasmtime/tests/all/*.rs`
transaction test module:

```rust
use anyhow::Result;
use wasmtime::{Config, Engine, Instance, Module, Store};

fn transaction_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_transaction(true);
    Engine::new(&config)
}

#[test]
fn transaction_op_without_boundary_traps() -> Result<()> {
    let engine = transaction_engine()?;
    let wat = r#"
        (module
          (memory 1)
          (func (export "bad") (result i32)
            (i32.const 0)
            (i32.tload)))
    "#;

    let module = Module::new(&engine, wat)?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[])?;
    let bad = instance.get_typed_func::<(), i32>(&mut store, "bad")?;
    let trap = bad.call(&mut store, ()).expect_err("missing transaction");

    assert!(trap.to_string().contains("transaction operation requires an active transaction"));
    Ok(())
}
```

Add a second test using a top-level exported transactional function once text
parser support accepts the branch syntax:

```rust
#[test]
fn exported_tfunc_starts_and_commits_transaction() -> Result<()> {
    let engine = transaction_engine()?;
    let wat = r#"
        (module
          (tmemory 1)
          (tfunc (export "write")
            (i32.const 0)
            (i32.const 42)
            (i32.tstore))
          (tfunc (export "read") (result i32)
            (i32.const 0)
            (i32.tload)))
    "#;

    let module = Module::new(&engine, wat)?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[])?;
    let write = instance.get_typed_func::<(), ()>(&mut store, "write")?;
    let read = instance.get_typed_func::<(), i32>(&mut store, "read")?;

    write.call(&mut store, ())?;
    assert_eq!(read.call(&mut store, ())?, 42);
    Ok(())
}
```

- [ ] **Step 2: Run active-transaction tests**

Run:

```bash
cargo test -p wasmtime transaction_op_without_boundary_traps
cargo test -p wasmtime exported_tfunc_starts_and_commits_transaction
```

Expected: first test FAILS if per-operation lazy begin is still present. Second
test FAILS until parser and boundary metadata are wired.

- [ ] **Step 3: Add runtime boundary APIs**

In `crates/wasmtime/src/runtime/transaction.rs`, add explicit boundary helpers:

```rust
pub(crate) enum TransactionBoundary {
    TopLevelTFunc,
    NonTransactionalToTransactionalCall,
}

pub(crate) fn begin_transaction_boundary(
    store: &mut dyn crate::runtime::vm::StoreContextMut,
    boundary: TransactionBoundary,
) -> anyhow::Result<TransactionId> {
    let id = store.transaction_runtime_mut().begin_transaction(boundary)?;
    Ok(id)
}

pub(crate) fn commit_transaction_boundary(
    store: &mut dyn crate::runtime::vm::StoreContextMut,
    id: TransactionId,
) -> anyhow::Result<()> {
    store.transaction_runtime_mut().commit_transaction(id)
}

pub(crate) fn abort_transaction_boundary(
    store: &mut dyn crate::runtime::vm::StoreContextMut,
    id: TransactionId,
) {
    store.transaction_runtime_mut().abort_transaction(id);
}
```

Adjust `StoreContextMut` and runtime access names to match existing Wasmtime
store internals. The API shape must provide separate begin, commit, and abort
calls and return the boundary transaction id.

- [ ] **Step 4: Add boundary libcalls**

In `crates/wasmtime/src/runtime/vm/libcalls.rs`, add libcalls for:

```rust
pub unsafe fn transaction_begin_tfunc_boundary(vmctx: *mut VMContext) -> u64
pub unsafe fn transaction_commit_tfunc_boundary(vmctx: *mut VMContext, tx: u64)
pub unsafe fn transaction_abort_tfunc_boundary(vmctx: *mut VMContext, tx: u64)
pub unsafe fn transaction_begin_tcall_boundary(vmctx: *mut VMContext) -> u64
pub unsafe fn transaction_commit_tcall_boundary(vmctx: *mut VMContext, tx: u64)
pub unsafe fn transaction_abort_tcall_boundary(vmctx: *mut VMContext, tx: u64)
```

Each begin returns `TransactionId::as_raw()`. Commit converts raw id back with
`TransactionId::from_raw`. Abort must not panic if the transaction is already
inactive because trap cleanup can run after partial failure.

- [ ] **Step 5: Lower exported tfunc prologue and normal-return commit**

In `crates/cranelift/src/func_environ.rs`, when translating a function marked by
`TransactionObjectMetadata::is_tfunc`, insert at function entry:

```rust
let tx = self.call_builtin_transaction_begin_tfunc_boundary(builder)?;
self.set_current_transaction_boundary_id(builder, tx);
```

Before each translated return in a boundary `tfunc`, emit:

```rust
let tx = self.current_transaction_boundary_id(builder)?;
self.call_builtin_transaction_commit_tfunc_boundary(builder, tx)?;
```

The exact state storage should follow existing Cranelift translation state
patterns. Use a single SSA value for the boundary id in functions with one entry
block and pass it through helper methods for return translation.

- [ ] **Step 6: Lower non-transactional-to-transactional tcall boundary**

In `crates/cranelift/src/translate/code_translator.rs`, for transaction call
operators where the caller is not transactional and the callee is transactional,
emit:

```rust
let tx = environ.call_builtin_transaction_begin_tcall_boundary(builder)?;
let call_result = environ.translate_transactional_call(builder, callee, args);
environ.call_builtin_transaction_commit_tcall_boundary(builder, tx)?;
call_result
```

If call lowering returns a trap, Wasmtime trap cleanup must call the abort
libcall. Implement the normal-return path in this task and keep trap abort
covered by the runtime trap cleanup hook in Step 7.

- [ ] **Step 7: Abort active boundary on trap**

Find the existing trap cleanup path:

```bash
rg -n "trap|Trap|unwind|catch|call_hook" crates/wasmtime/src/runtime crates/wasmtime/src/store.rs crates/wasmtime/src/instance.rs -g '*.rs'
```

At the store boundary where a guest trap is converted to a host error, add:

```rust
if let Some(tx) = store.transaction_runtime_mut().active_boundary_transaction() {
    store.transaction_runtime_mut().abort_transaction(tx);
}
```

This must run before the trap is returned to the host. It must not catch or
suppress the original trap.

- [ ] **Step 8: Remove lazy begin from transaction memory/global lowering**

In `crates/cranelift/src/func_environ.rs`, remove calls equivalent to:

```rust
translate_transaction_begin_if_needed(builder)
```

from scalar transaction memory/global operators. The active transaction check
now belongs in runtime libcalls and boundary lowering.

- [ ] **Step 9: Run boundary tests and focused WAST**

Run:

```bash
cargo test -p wasmtime transaction_op_without_boundary_traps
cargo test -p wasmtime exported_tfunc_starts_and_commits_transaction
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
```

Expected: PASS for boundary integration tests. Focused WAST passes for files
whose parser support is real and remains ignored for explicitly deferred files.

- [ ] **Step 10: Commit**

Run:

```bash
git add crates/cranelift/src/func_environ.rs crates/cranelift/src/translate/code_translator.rs crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/libcalls.rs crates/wasmtime/src
git commit -m "Implement transactional function boundaries"
```

Expected: commit succeeds with no unrelated files staged.

### Task 8: Real Scalar Transaction Parser And Validation Bridge

**Files:**
- Modify: `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wasmparser/src/readers/core/operators.rs`
- Modify: `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wat/src`
- Modify: Wasmtime workspace dependency patch files if present in `Cargo.toml`
  or `.cargo/config.toml`
- Modify: `crates/environ/src/transaction.rs`
- Modify: `crates/test-util/src/wast.rs`
- Test: existing proposal WAST files under `tests/wast/transaction-proposal`

This task removes normalization for scalar transaction syntax only after the
local parser fork accepts the text and binary forms and Wasmtime validation
receives metadata.

- [ ] **Step 1: Inspect parser fork status**

Run:

```bash
git -C /home/shisoft/Code/Research/wasm-tools-transaction branch --show-current
git -C /home/shisoft/Code/Research/wasm-tools-transaction status --short
rg -n "TMemory|TGlobal|TLoad|TStore|0xfa|transaction" /home/shisoft/Code/Research/wasm-tools-transaction/crates/wasmparser/src /home/shisoft/Code/Research/wasm-tools-transaction/crates/wat/src
```

Expected: output shows existing binary operator parsing and any text parser
gaps.

- [ ] **Step 2: Add parser fork tests for scalar transaction text**

In the relevant `wat` crate test module, add tests that parse:

```wat
(module
  (tmemory 1)
  (tglobal (mut i32) (i32.const 0))
  (tfunc (export "write")
    (i32.const 0)
    (i32.const 1)
    (i32.tstore)
    (i32.const 0)
    (i32.tload)
    (drop)
    (i32.const 2)
    (tglobal.set 0)
    (tglobal.get 0)
    (drop)))
```

The test must call the same parser entry point used by existing `wat` tests and
assert that parsing succeeds.

- [ ] **Step 3: Run parser fork tests**

Run:

```bash
cargo test --manifest-path /home/shisoft/Code/Research/wasm-tools-transaction/Cargo.toml -p wat transaction
cargo test --manifest-path /home/shisoft/Code/Research/wasm-tools-transaction/Cargo.toml -p wasmparser transaction
```

Expected: FAIL if text transaction forms are not accepted.

- [ ] **Step 4: Implement text parser forms**

In `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wat/src`, wire
the token forms to the existing binary operators:

- `(tmemory ...)` emits transactional memory metadata and a memory declaration
  that Wasmtime can map to `TransactionObjectMetadata::memories`.
- `(tglobal ...)` emits transactional global metadata and a global declaration
  that Wasmtime can map to `TransactionObjectMetadata::globals`.
- `(tfunc ...)` emits function metadata for
  `TransactionObjectMetadata::functions`.
- `i32.tload`, `i64.tload`, `f32.tload`, `f64.tload`, packed scalar tloads,
  scalar tstores, `tmemory.size`, `tmemory.grow`, `tglobal.get`, and
  `tglobal.set` emit existing `TransactionOperator` variants.

Use the same opcode names already added in
`crates/wasmparser/src/readers/core/operators.rs`.

- [ ] **Step 5: Populate Wasmtime transaction metadata from parser output**

In Wasmtime's translation path, map parser fork transaction metadata into
`TransactionObjectMetadata`:

```rust
module.translation_mut().transaction_objects.add_tmemory(memory_index);
module.translation_mut().transaction_objects.add_tglobal(global_index);
module.translation_mut().transaction_objects.add_tfunc(func_index);
```

Use the existing memory/global metadata methods for memory/global names and the
Task 1 `add_tfunc` method for functions.

- [ ] **Step 6: Enforce scalar validation rules**

In `crates/environ/src/transaction.rs` or the existing validation hook:

- `*.tload`, `*.tstore`, `tmemory.size`, and `tmemory.grow` require a metadata
  `tmemory` target.
- `tglobal.get` and `tglobal.set` require a metadata `tglobal` target.
- Transaction operators inside an ordinary function are rejected unless the
  operator is a valid transaction boundary form.
- Ordinary calls to `tfunc` and transaction calls to ordinary functions are
  rejected.

Diagnostic text must contain stable Wasmtime-style substrings:

```text
transactional memory operation targeted non-transactional memory
transactional global operation targeted non-transactional global
transaction operation requires transactional function context
transaction call target kind mismatch
```

- [ ] **Step 7: Remove scalar normalization from WAST harness**

In `crates/test-util/src/wast.rs`, remove normalization for scalar memory/global
WAST files whose parser and runtime path are now real. Keep ignored status for
object-table, refs, SIMD transactional memory, and `ttry`/`tfail` files.

Do not edit files under `tests/wast/transaction-proposal`.

- [ ] **Step 8: Run parser and scalar WAST tests**

Run:

```bash
cargo test --manifest-path /home/shisoft/Code/Research/wasm-tools-transaction/Cargo.toml -p wat transaction
cargo test --manifest-path /home/shisoft/Code/Research/wasm-tools-transaction/Cargo.toml -p wasmparser transaction
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/tmemory -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/tglobal -- --format terse
```

Expected: parser fork tests PASS. Scalar memory/global WAST files PASS through
the real Wasmtime engine path with no fixture replacement.

- [ ] **Step 9: Commit parser fork**

Run:

```bash
git -C /home/shisoft/Code/Research/wasm-tools-transaction add crates/wasmparser/src/readers/core/operators.rs crates/wat/src
git -C /home/shisoft/Code/Research/wasm-tools-transaction commit -m "Parse scalar transactional Wasm text"
```

Expected: parser fork commit succeeds with no unrelated files staged.

- [ ] **Step 10: Commit Wasmtime parser bridge**

Run:

```bash
git add crates/environ/src/transaction.rs crates/test-util/src/wast.rs Cargo.toml .cargo/config.toml
git commit -m "Use real parser for scalar transaction tests"
```

Expected: commit succeeds. If `Cargo.toml` or `.cargo/config.toml` were not
modified, omit them from `git add`.

### Task 9: Mock Tag Audit And WAST Unlock

**Files:**
- Modify: `crates/test-util/src/wast.rs`
- Modify: `tests/wast.rs`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
- Read: `docs/shisoft/transactional-wasm-mock-runtime-plan.md`
- Test: proposal WAST commands

This task updates ignored-test boundaries to match the real runtime core and
keeps remaining mocks visible for future object-table, SIMD, and `ttry`/`tfail`
work.

- [ ] **Step 1: Audit remaining mock tags**

Run:

```bash
rg -n "SHISOFT-TWASM-MOCK|transaction_proposal_adapter_mock|normaliz" crates tests docs/shisoft
```

Expected: output only references remaining non-core categories:

- object-table and transactional refs
- SIMD transactional memory
- `ttry`/`tfail`
- files still explicitly ignored because they require one of those categories

- [ ] **Step 2: Remove ignores for scalar memory/global WAST files**

In `tests/wast.rs` and `crates/test-util/src/wast.rs`, remove ignore or adapter
entries for files covered by Tasks 6 through 8:

- `simple-transactions`
- scalar `tmemory`
- scalar `tglobal`
- scalar numeric transaction load/store files

Keep ignored entries for:

- transactional refs and branch-on-ref
- `tstruct`, `tarray`, `ti31`, `textern`
- `ttry`/`tfail`
- SIMD `v128.tload*` and `v128.tstore*`
- object-table permission tests

- [ ] **Step 3: Run focused unlocked WAST tests**

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/tmemory -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/tglobal -- --format terse
```

Expected: PASS with no fixture replacement for those files.

- [ ] **Step 4: Run full proposal WAST suite**

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```

Expected: PASS for unlocked scalar files. Output may still report ignored tests
for object-table, refs, SIMD, and `ttry`/`tfail`.

- [ ] **Step 5: Update implementation log**

Append an entry to `docs/shisoft/transactional-wasm-implementation-log.md`:

```markdown
## Runtime Core Scalar TMemory/TGlobal Unlock

- Removed ordinary-memory backing for scalar `tmemory` libcalls.
- Added `VMemory`-backed `tmemory` sidecar storage.
- Added COW transaction workspace indexed by `GranuleId`.
- Added `LockBased` optimistic-read/pessimistic-write ownership for runtime
  granules.
- Unlocked scalar memory/global proposal WAST files through the real parser and
  real Wasmtime runtime path.
- Remaining mocks are limited to object-table/ref permissions, SIMD
  transactional memory, and `ttry`/`tfail`.
```

- [ ] **Step 6: Commit**

Run:

```bash
git add crates/test-util/src/wast.rs tests/wast.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Unlock scalar transaction WAST runtime path"
```

Expected: commit succeeds with no unrelated files staged.

## Verification Matrix

Run these after Task 9 before claiming the runtime core slice is complete:

```bash
cargo test -p wasmtime-environ transaction_metadata_tracks_tfuncs
cargo test -p wasmtime --lib vmemory_commit_and_read_committed_bytes
cargo test -p wasmtime --lib vmemory_grow_initializes_new_granule_metadata
cargo test -p wasmtime --lib file_backed_and_nv_backends_are_explicitly_unsupported
cargo test -p wasmtime --lib granule_id_orders_by_object_space_and_index
cargo test -p wasmtime --lib workspace_merges_staged_and_committed_granules
cargo test -p wasmtime --lib lock_based_write_conflict_aborts_current_transaction
cargo test -p wasmtime --lib lock_based_validates_optimistic_reads_at_commit
cargo test -p wasmtime --lib lock_based_abort_releases_owned_granules
cargo test -p wasmtime --lib transaction_commit_copies_staged_granules_to_tmemory
cargo test -p wasmtime --lib transaction_abort_discards_staged_tmemory_bytes
cargo test -p wasmtime transaction_op_without_boundary_traps
cargo test -p wasmtime exported_tfunc_starts_and_commits_transaction
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/tmemory -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/tglobal -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
rg -n "SHISOFT-TWASM-MOCK|transaction_proposal_adapter_mock|normaliz" crates tests docs/shisoft
```

Expected final state for this plan:

- Unit tests pass.
- Scalar transaction WAST tests pass through the real parser and real Wasmtime
  runtime path.
- Full proposal WAST command passes with ignores only for object-table/refs,
  SIMD transactional memory, and `ttry`/`tfail`.
- `rg` output contains no scalar `tmemory`, scalar `tglobal`, transaction
  boundary, or ordinary-memory-backend mock tags.

## Execution Strategy With Subagents

Use `superpowers:subagent-driven-development` for execution. Dispatch one
fresh worker per task group and review each result before merging into the main
branch:

- Worker A: Tasks 1, 3, and 4 (`transaction.rs` and environ metadata).
- Worker B: Tasks 2 and 5 (`tmemory.rs`, VM memory exports, instance sidecar).
- Worker C: Task 6 (`libcalls.rs` and real scalar tmemory runtime path).
- Worker D: Task 7 (Cranelift lowering and transaction boundary behavior).
- Worker E: Tasks 8 and 9 (parser fork bridge, WAST harness unlock, docs).

Review order:

1. Merge Worker A after metadata, `GranuleId`, and `LockBased` tests pass.
2. Merge Worker B after backend and sidecar tests pass.
3. Merge Worker C after real-tmemory unit tests pass and scalar WAST smoke is
   no worse than before.
4. Merge Worker D after boundary tests pass and per-operation lazy begin is
   removed.
5. Merge Worker E after scalar WAST files run through real parser/runtime
   without normalization or fixture replacement.

Stop before wiring object-space granules, the object table, `ttry`/`tfail`, or
SIMD transactional memory. Those are separate workstreams that build on this
runtime core.
