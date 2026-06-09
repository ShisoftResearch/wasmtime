# NVMemory PMEM Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the first real PMEM-shaped transactional `tmemory` backend, `NVMemory`, while keeping true crash-recovery tests gated until PMEM hardware is available.

**Architecture:** `NVMemory` follows the existing `TMemoryBackendStorage` and block/chunk region interfaces used by `VMemory`. Transaction semantics stay copy-on-write: transaction workspaces stage bytes, commit copies bytes into the backend, and `NVMemory` adds cache-line flush plus fence behavior at publication boundaries.

**Tech Stack:** Rust, Wasmtime runtime internals, x86-64 `CLWB`/`SFENCE` intrinsics, `TMemoryBackendStorage`, `BlockRegionBackend`, existing transactional WAST harness.

---

## File Map

- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
  - Add PMEM cache-line constants.
  - Add `PersistenceMode`.
  - Add `PersistEngine`.
  - Add `NVMemoryBlockRegion`.
  - Keep `VMemoryBlockRegion` unchanged except for shared helpers.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs`
  - Generalize `MappedLinearRegion` over any `BlockRegionBackend`.
  - Add constructors for `VMemoryBlockRegion` and `NVMemoryBlockRegion`.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
  - Add `NVMemory` implementing `TMemoryBackendStorage`.
  - Route `TMemoryBackend::NVMemory` construction.
  - Keep `FileBackedMemory` explicitly unsupported.
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
  - Allow `TransactionConfig` to select `NVMemory` in research mode.
  - Keep default backend as `VMemory`.
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
  - Record implementation status and remaining PMEM recovery boundary.

## Task 1: Add PMEM Persistence Engine Tests

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`

- [ ] **Step 1: Add failing unit tests for persistence mode selection**

Add these tests inside the existing `#[cfg(test)] mod tests` in `block_region.rs`:

```rust
#[test]
fn pmem_research_mode_allows_non_durable_flush_engine() {
    let engine = PersistEngine::for_mode(PersistenceMode::ResearchPretendPmem).unwrap();

    assert_eq!(engine.mode(), PersistenceMode::ResearchPretendPmem);
    engine.flush(core::ptr::NonNull::<u8>::dangling(), 0).unwrap();
    engine.fence().unwrap();
}

#[test]
fn pmem_real_mode_reports_platform_availability() {
    let result = PersistEngine::for_mode(PersistenceMode::RequireHardwarePmem);

    #[cfg(target_arch = "x86_64")]
    {
        if PersistEngine::hardware_flush_available() {
            assert!(result.is_ok());
        } else {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("CLWB"));
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        let error = result.unwrap_err().to_string();
        assert!(error.contains("NVMemory is unsupported"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test -p wasmtime --lib pmem_ -- --format terse
```

Expected: compile failure because `PersistEngine` and `PersistenceMode` are not defined.

- [ ] **Step 3: Add persistence mode and engine**

Add this near the top of `block_region.rs`, after line-size constants:

```rust
pub(crate) const PMEM_CACHE_LINE_SIZE: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PersistenceMode {
    ResearchPretendPmem,
    RequireHardwarePmem,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PersistFlushKind {
    NoopResearch,
    X86Clwb,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PersistEngine {
    mode: PersistenceMode,
    flush_kind: PersistFlushKind,
}

impl PersistEngine {
    pub(crate) fn for_mode(mode: PersistenceMode) -> Result<Self> {
        let flush_kind = match mode {
            PersistenceMode::ResearchPretendPmem => PersistFlushKind::NoopResearch,
            PersistenceMode::RequireHardwarePmem => hardware_flush_kind()?,
        };
        Ok(Self { mode, flush_kind })
    }

    pub(crate) fn mode(&self) -> PersistenceMode {
        self.mode
    }

    pub(crate) fn flush(&self, ptr: core::ptr::NonNull<u8>, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        match self.flush_kind {
            PersistFlushKind::NoopResearch => Ok(()),
            PersistFlushKind::X86Clwb => unsafe { flush_clwb_range(ptr, len) },
        }
    }

    pub(crate) fn fence(&self) -> Result<()> {
        match self.flush_kind {
            PersistFlushKind::NoopResearch => Ok(()),
            PersistFlushKind::X86Clwb => unsafe { sfence() },
        }
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn hardware_flush_available() -> bool {
        let leaf = core::arch::x86_64::__cpuid_count(7, 0);
        leaf.ebx & (1 << 24) != 0
    }

    #[cfg(not(target_arch = "x86_64"))]
    pub(crate) fn hardware_flush_available() -> bool {
        false
    }
}

fn hardware_flush_kind() -> Result<PersistFlushKind> {
    #[cfg(target_arch = "x86_64")]
    {
        if PersistEngine::hardware_flush_available() {
            return Ok(PersistFlushKind::X86Clwb);
        }
        bail!("NVMemory requires CLWB support for hardware PMEM mode");
    }

    #[cfg(not(target_arch = "x86_64"))]
    bail!("NVMemory is unsupported on this target until a PMEM flush implementation exists")
}

#[cfg(target_arch = "x86_64")]
unsafe fn flush_clwb_range(ptr: core::ptr::NonNull<u8>, len: usize) -> Result<()> {
    let start = ptr.as_ptr() as usize;
    let end = start
        .checked_add(len)
        .context("pmem flush range overflow")?;
    let mut cursor = start & !(PMEM_CACHE_LINE_SIZE - 1);
    while cursor < end {
        unsafe {
            core::arch::asm!(
                "clwb [{}]",
                in(reg) cursor as *const u8,
                options(nostack, preserves_flags)
            );
        }
        cursor = cursor
            .checked_add(PMEM_CACHE_LINE_SIZE)
            .context("pmem flush cursor overflow")?;
    }
    Ok(())
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn flush_clwb_range(_ptr: core::ptr::NonNull<u8>, _len: usize) -> Result<()> {
    bail!("NVMemory CLWB flush is unsupported on this target")
}

#[cfg(target_arch = "x86_64")]
unsafe fn sfence() -> Result<()> {
    unsafe {
        core::arch::asm!("sfence", options(nostack, preserves_flags));
    }
    Ok(())
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn sfence() -> Result<()> {
    bail!("NVMemory SFENCE is unsupported on this target")
}
```

- [ ] **Step 4: Run the PMEM engine tests**

Run:

```bash
cargo test -p wasmtime --lib pmem_ -- --format terse
```

Expected: the two `pmem_*` tests pass.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
git commit -m "Add PMEM persistence engine"
```

## Task 2: Add NVMemory Block Region

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`

- [ ] **Step 1: Add failing tests for `NVMemoryBlockRegion`**

Add these tests in `block_region.rs`:

```rust
#[test]
fn nvmemory_block_region_allocates_and_persists_writes_in_research_mode() {
    let mut region =
        NVMemoryBlockRegion::new_for_test(2, PersistenceMode::ResearchPretendPmem).unwrap();
    let chunk = region.alloc_chunk(1).unwrap();
    let range = chunk.byte_range();

    region.write(range.start, &[1, 2, 3, 4]).unwrap();
    region.flush(range.start, 4).unwrap();
    region.fence().unwrap();

    assert_eq!(region.read(range.start, 4).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(region.block_size(), BLOCK_SIZE);
    assert_eq!(region.num_blocks(), 2);
}

#[test]
fn nvmemory_block_region_rejects_out_of_bounds_flush() {
    let region =
        NVMemoryBlockRegion::new_for_test(1, PersistenceMode::ResearchPretendPmem).unwrap();

    let error = region.flush(BLOCK_SIZE, 1).unwrap_err().to_string();
    assert!(error.contains("flush range out of bounds"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test -p wasmtime --lib nvmemory_block_region -- --format terse
```

Expected: compile failure because `NVMemoryBlockRegion` is not defined.

- [ ] **Step 3: Implement `NVMemoryBlockRegion`**

Add the struct and impl after `VMemoryBlockRegion`:

```rust
#[derive(Debug)]
pub(crate) struct NVMemoryBlockRegion {
    data: Vec<u8>,
    block_entries: Vec<BlockEntry>,
    line_marks: Vec<LineMark>,
    persist: PersistEngine,
}

impl NVMemoryBlockRegion {
    pub(crate) fn new(num_blocks: usize, mode: PersistenceMode) -> Result<Self> {
        let bytes_len = num_blocks
            .checked_mul(BLOCK_SIZE)
            .context("transactional NVMemory block region size overflow")?;
        let line_count = bytes_len / IMMIX_LINE_SIZE;
        Ok(Self {
            data: vec![0; bytes_len],
            block_entries: vec![BlockEntry::default(); num_blocks],
            line_marks: vec![
                LineMark {
                    mark: IMMIX_LINE_MARK_RESET_VALUE,
                };
                line_count
            ],
            persist: PersistEngine::for_mode(mode)?,
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(num_blocks: usize, mode: PersistenceMode) -> Result<Self> {
        Self::new(num_blocks, mode)
    }

    pub(crate) fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    pub(crate) fn num_blocks(&self) -> usize {
        self.block_entries.len()
    }

    pub(crate) fn bytes_len(&self) -> usize {
        self.data.len()
    }

    pub(crate) fn line_count(&self) -> usize {
        self.line_marks.len()
    }

    pub(crate) fn alloc_chunk(&mut self, block_count: usize) -> Result<RegionChunk> {
        ensure!(block_count > 0, "transactional NVMemory chunk must contain a block");
        ensure!(
            block_count <= self.num_blocks(),
            "transactional NVMemory chunk exceeds region size"
        );

        let last_start = self.num_blocks() - block_count;
        for start in 0..=last_start {
            let end = start + block_count;
            if self.block_entries[start..end]
                .iter()
                .all(|entry| entry.used == 0)
            {
                for entry in &mut self.block_entries[start..end] {
                    entry.used = 1;
                    entry.list_num = ListKind::Used as i16;
                }
                return Ok(RegionChunk {
                    start_block: start,
                    block_count,
                });
            }
        }

        bail!("transactional NVMemory block region is out of contiguous chunks")
    }

    pub(crate) fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        let end = offset
            .checked_add(len)
            .context("transactional NVMemory block read range overflow")?;
        ensure!(
            end <= self.data.len(),
            "transactional NVMemory block read range out of bounds"
        );
        Ok(self.data[offset..end].to_vec())
    }

    pub(crate) fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        let end = offset
            .checked_add(bytes.len())
            .context("transactional NVMemory block write range overflow")?;
        ensure!(
            end <= self.data.len(),
            "transactional NVMemory block write range out of bounds"
        );
        self.data[offset..end].copy_from_slice(bytes);
        Ok(())
    }

    pub(crate) fn flush(&self, offset: usize, len: usize) -> Result<()> {
        let end = offset
            .checked_add(len)
            .context("transactional NVMemory block flush range overflow")?;
        ensure!(
            end <= self.data.len(),
            "transactional NVMemory block flush range out of bounds"
        );
        if len == 0 {
            return Ok(());
        }
        let ptr = core::ptr::NonNull::from(&self.data[offset]).cast::<u8>();
        self.persist.flush(ptr, len)
    }

    pub(crate) fn fence(&self) -> Result<()> {
        self.persist.fence()
    }
}

impl BlockRegionBackend for NVMemoryBlockRegion {
    fn block_size(&self) -> usize {
        self.block_size()
    }

    fn num_blocks(&self) -> usize {
        self.num_blocks()
    }

    fn bytes_len(&self) -> usize {
        self.bytes_len()
    }

    fn alloc_chunk(&mut self, block_count: usize) -> Result<RegionChunk> {
        self.alloc_chunk(block_count)
    }

    fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        self.read(offset, len)
    }

    fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        self.write(offset, bytes)
    }

    fn flush(&self, offset: usize, len: usize) -> Result<()> {
        self.flush(offset, len)
    }

    fn fence(&self) -> Result<()> {
        self.fence()
    }
}
```

- [ ] **Step 4: Run block-region tests**

Run:

```bash
cargo test -p wasmtime --lib nvmemory_block_region -- --format terse
cargo test -p wasmtime --lib vmemory_block_region -- --format terse
```

Expected: all selected tests pass.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
git commit -m "Add NVMemory block region"
```

## Task 3: Generalize Linear Regions Over Block Backends

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`

- [ ] **Step 1: Add failing linear-region test for `NVMemoryBlockRegion`**

Add this test in `linear_region.rs`:

```rust
#[test]
fn mapped_linear_region_supports_nvmemory_backend() {
    let mut backend =
        NVMemoryBlockRegion::new_for_test(2, PersistenceMode::ResearchPretendPmem).unwrap();
    let chunk = backend.alloc_chunk(1).unwrap();
    let chunks = ChunkList::from_chunks(vec![chunk]).unwrap();
    let mut region = MappedLinearRegion::new(Box::new(backend), chunks);

    region.write(0, &[9, 8, 7]).unwrap();
    assert_eq!(region.read(0, 3).unwrap(), vec![9, 8, 7]);
}
```

Update the test imports to include `NVMemoryBlockRegion` and `PersistenceMode`.

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p wasmtime --lib mapped_linear_region_supports_nvmemory_backend -- --format terse
```

Expected: compile failure because `MappedLinearRegion::new` still takes `VMemoryBlockRegion`.

- [ ] **Step 3: Change `MappedLinearRegion` to own `Box<dyn BlockRegionBackend>`**

In `linear_region.rs`, change imports and fields:

```rust
use super::block_region::{
    BLOCK_SIZE, BlockRegionBackend, ChunkList, IMMIX_LINE_SIZE, PersistenceMode,
    VMemoryBlockRegion,
};
```

Change the struct:

```rust
pub(super) struct MappedLinearRegion {
    backend: Box<dyn BlockRegionBackend>,
    chunks: ChunkList,
}
```

Change the constructor:

```rust
impl MappedLinearRegion {
    pub(super) fn new(backend: Box<dyn BlockRegionBackend>, chunks: ChunkList) -> Self {
        Self { backend, chunks }
    }
}
```

Update `TMemoryRegion::new` to box the volatile backend:

```rust
linear: MappedLinearRegion::new(Box::new(backend), chunks),
```

- [ ] **Step 4: Add an `NVMemory` region constructor**

Add this to `TMemoryRegion`:

```rust
pub(super) fn new_nvmemory(byte_capacity: usize, mode: PersistenceMode) -> Result<Self> {
    let block_count = byte_capacity.div_ceil(BLOCK_SIZE);
    let mut backend = super::block_region::NVMemoryBlockRegion::new(block_count, mode)?;
    let chunks = if block_count == 0 {
        Vec::new()
    } else {
        vec![backend.alloc_chunk(block_count)?]
    };
    let chunks = ChunkList::from_chunks(chunks)?;
    Ok(Self {
        linear: MappedLinearRegion::new(Box::new(backend), chunks),
    })
}
```

- [ ] **Step 5: Run linear-region tests**

Run:

```bash
cargo test -p wasmtime --lib mapped_linear_region -- --format terse
```

Expected: all selected tests pass.

- [ ] **Step 6: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs crates/wasmtime/src/runtime/vm/memory/tmemory.rs
git commit -m "Generalize transactional linear regions"
```

## Task 4: Add `NVMemory` Storage Backend

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs`

- [ ] **Step 1: Add failing `NVMemory` backend tests**

Add these tests to `tmemory.rs`:

```rust
#[test]
fn tmemory_can_construct_nvmemory_in_research_mode() {
    let memory = TMemory::new_with_backend_limits(TMemoryBackend::NVMemory, 1, Some(1)).unwrap();

    assert_eq!(memory.backend(), TMemoryBackend::NVMemory);
    assert_eq!(memory.byte_len(), WASM_PAGE_SIZE);
    assert_eq!(memory.granule_len(), GRANULES_PER_WASM_PAGE);
}

#[test]
fn nvmemory_commit_range_writes_and_versions_granules() {
    let mut memory =
        TMemory::new_with_backend_limits(TMemoryBackend::NVMemory, 1, Some(1)).unwrap();

    memory.commit_range(4, &[10, 11, 12, 13]).unwrap();

    assert_eq!(memory.read_committed(4..8).unwrap(), vec![10, 11, 12, 13]);
    assert_eq!(memory.granule_version(0).unwrap(), 1);
}

#[test]
fn file_backed_memory_remains_explicitly_unsupported() {
    let error = TMemory::new_with_backend(TMemoryBackend::FileBackedMemory, 1)
        .unwrap_err()
        .to_string();

    assert!(error.contains("FileBackedMemory"));
    assert!(error.contains("not implemented"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test -p wasmtime --lib nvmemory -- --format terse
```

Expected: `NVMemory` construction still returns unsupported backend errors.

- [ ] **Step 3: Implement `NVMemory` mirroring `VMemory`**

Add `PersistenceMode` to the imports from `block_region` through `linear_region`.
Then add this struct after `VMemory`:

```rust
#[derive(Debug)]
pub(crate) struct NVMemory {
    region: TMemoryRegion,
    granules: Vec<TMemoryGranuleInfo>,
    byte_len: usize,
    byte_capacity: usize,
    max_pages: u64,
}
```

Implement the same public methods as `VMemory`, with `TMemoryRegion::new_nvmemory`
and `PersistenceMode::ResearchPretendPmem`:

```rust
impl NVMemory {
    pub(crate) fn new(min_pages: u64, max_pages: Option<u64>) -> Result<Self> {
        let requested_max_pages = max_pages;
        let max_pages = max_pages.unwrap_or(DEFAULT_MAX_WASM_PAGES);
        ensure!(min_pages <= max_pages, "tmemory minimum exceeds maximum");
        let byte_len = pages_to_bytes(min_pages)?;
        let byte_capacity = match requested_max_pages {
            Some(max_pages) => pages_to_bytes(max_pages)?,
            None => byte_len,
        };
        let granule_capacity = granules_for_bytes(byte_capacity);
        Ok(Self {
            region: TMemoryRegion::new_nvmemory(
                byte_capacity,
                block_region::PersistenceMode::ResearchPretendPmem,
            )?,
            granules: vec![TMemoryGranuleInfo::default(); granule_capacity],
            byte_len,
            byte_capacity,
            max_pages,
        })
    }
}
```

For the remaining methods, copy the `VMemory` implementation and change error
strings from `tmemory` to `NVMemory tmemory` only where the string names storage
rather than user-facing Wasm behavior. Keep user-facing bounds diagnostics
compatible with existing `tmemory` behavior.

- [ ] **Step 4: Implement `TMemoryBackendStorage for NVMemory`**

Add:

```rust
impl TMemoryBackendStorage for NVMemory {
    fn backend_kind(&self) -> TMemoryBackend {
        TMemoryBackend::NVMemory
    }

    fn byte_len(&self) -> usize {
        self.byte_len()
    }

    fn byte_capacity(&self) -> usize {
        self.byte_capacity
    }

    fn granule_count(&self) -> usize {
        self.granule_len()
    }

    fn read_committed(&self, range: core::ops::Range<usize>) -> Result<Vec<u8>> {
        self.read(range)
    }

    fn commit_range(&mut self, addr: usize, bytes: &[u8]) -> Result<()> {
        let end = addr
            .checked_add(bytes.len())
            .context("tmemory write address overflow")?;
        ensure!(end <= self.byte_len, "out of bounds tmemory access");
        self.region.write(addr, bytes)?;
        self.region.flush(addr, bytes.len())?;
        self.region.fence()
    }

    fn can_grow_to_pages(&self, new_pages: u64) -> bool {
        NVMemory::can_grow_to_pages(self, new_pages)
    }

    fn grow_to_pages(&mut self, new_pages: u64) -> Result<()> {
        NVMemory::grow_to_pages(self, new_pages)
    }

    fn granule_info(&self, granule: usize) -> Result<TMemoryGranuleInfo> {
        self.txn_info(granule)
    }

    fn set_granule_info(&mut self, granule: usize, info: TMemoryGranuleInfo) -> Result<()> {
        self.set_txn_info(granule, info)
    }
}
```

Add `flush` and `fence` forwarding methods to `TMemoryRegion`:

```rust
pub(super) fn flush(&self, offset: usize, len: usize) -> Result<()> {
    self.linear.flush(offset, len)
}

pub(super) fn fence(&self) -> Result<()> {
    self.linear.fence()
}
```

Add matching methods to `MappedLinearRegion`:

```rust
pub(super) fn flush(&self, offset: usize, len: usize) -> Result<()> {
    self.check_range(offset, len, "flush")?;
    let mut cursor = offset;
    let end = offset + len;

    while cursor < end {
        let segment = self
            .logical_segment(cursor)
            .context("transactional linear flush segment missing")?;
        let take = segment.available.min(end - cursor);
        self.backend.flush(segment.backend_offset, take)?;
        cursor += take;
    }

    Ok(())
}

pub(super) fn fence(&self) -> Result<()> {
    self.backend.fence()
}
```

- [ ] **Step 5: Route backend construction**

Change `TMemory::new_for_backend`:

```rust
let storage: Box<dyn TMemoryBackendStorage> = match backend {
    TMemoryBackend::VMemory => Box::new(VMemory::new(min_pages, max_pages)?),
    TMemoryBackend::NVMemory => Box::new(NVMemory::new(min_pages, max_pages)?),
    TMemoryBackend::FileBackedMemory => {
        return Err(unsupported_backend_error(backend));
    }
};
```

- [ ] **Step 6: Run backend tests**

Run:

```bash
cargo test -p wasmtime --lib nvmemory -- --format terse
cargo test -p wasmtime --lib tmemory -- --format terse
```

Expected: all selected tests pass.

- [ ] **Step 7: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/vm/memory/tmemory.rs crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs
git commit -m "Add NVMemory tmemory backend"
```

## Task 5: Wire `TransactionConfig` Backend Selection

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`

- [ ] **Step 1: Add failing config tests**

Add this test in `transaction.rs`:

```rust
#[test]
fn transaction_config_accepts_nvmemory_backend() {
    let config = TransactionConfig::with_tmemory_backend(TMemoryBackend::NVMemory).unwrap();

    assert_eq!(config.tmemory_backend(), TMemoryBackend::NVMemory);
    assert!(!config.is_vmemory_only());
}
```

Keep the existing test coverage that rejects `FileBackedMemory`; update expected loops so only `FileBackedMemory` is rejected.

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test -p wasmtime --lib transaction_config_accepts_nvmemory_backend -- --format terse
```

Expected: the test fails because `set_tmemory_backend` still rejects `NVMemory`.

- [ ] **Step 3: Allow `NVMemory` in config**

Change `TransactionConfig::set_tmemory_backend`:

```rust
fn set_tmemory_backend(&mut self, tmemory_backend: TMemoryBackend) -> Result<()> {
    match tmemory_backend {
        TMemoryBackend::VMemory | TMemoryBackend::NVMemory => {
            self.tmemory_backend = tmemory_backend;
            Ok(())
        }
        TMemoryBackend::FileBackedMemory => {
            bail!("tmemory backend is not implemented: {tmemory_backend:?}")
        }
    }
}
```

- [ ] **Step 4: Run config and tmemory tests**

Run:

```bash
cargo test -p wasmtime --lib transaction_config -- --format terse
cargo test -p wasmtime --lib tmemory -- --format terse
```

Expected: selected tests pass.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/memory/tmemory.rs
git commit -m "Enable NVMemory backend selection"
```

## Task 6: Add Ignored Real-PMEM Recovery Tests

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`

- [ ] **Step 1: Add ignored tests documenting hardware boundary**

Add this test in `tmemory.rs`:

```rust
#[test]
#[ignore = "requires real PMEM hardware and WASMTIME_TEST_REAL_PMEM=1"]
fn nvmemory_requires_real_pmem_for_restart_persistence() {
    if std::env::var_os("WASMTIME_TEST_REAL_PMEM").is_none() {
        eprintln!("set WASMTIME_TEST_REAL_PMEM=1 on a PMEM machine to run this test");
        return;
    }

    panic!("real PMEM restart recovery test is not implemented in this backend wave");
}
```

This ignored test is intentionally a boundary marker. It must not be enabled
until durable region headers and restart recovery exist.

- [ ] **Step 2: Run ignored-test listing**

Run:

```bash
cargo test -p wasmtime --lib nvmemory_requires_real_pmem_for_restart_persistence -- --ignored --format terse
```

Expected: the ignored test runs and panics with the explicit recovery-not-implemented message if forced. Do not run this command in normal verification after this step.

- [ ] **Step 3: Run normal tests and confirm ignored test is skipped**

Run:

```bash
cargo test -p wasmtime --lib nvmemory -- --format terse
```

Expected: normal `nvmemory` tests pass; the real-PMEM restart test is ignored unless explicitly selected.

- [ ] **Step 4: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/vm/memory/tmemory.rs
git commit -m "Document real PMEM recovery test boundary"
```

## Task 7: Verify WAST and Feature Boundaries

**Files:**
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Run focused backend tests**

Run:

```bash
cargo test -p wasmtime --lib pmem_ -- --format terse
cargo test -p wasmtime --lib nvmemory -- --format terse
cargo test -p wasmtime --lib tmemory -- --format terse
```

Expected: all selected non-ignored tests pass.

- [ ] **Step 2: Run transaction proposal WAST**

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```

Expected: 173 transaction-proposal tests pass with 0 ignored.

- [ ] **Step 3: Run feature-boundary checks**

Run:

```bash
cargo check -p wasmtime --no-default-features --features transaction
cargo check -p wasmtime --no-default-features --features runtime,std,gc
```

Expected: both commands pass.

- [ ] **Step 4: Update implementation log**

Add this section near the top of `docs/shisoft/transactional-wasm-implementation-log.md`:

```markdown
## NVMemory Backend Implementation

Date: 2026-06-09

`NVMemory` now exists as the first PMEM-shaped transactional memory backend.
It shares the block/chunk storage abstraction with `VMemory`, uses the normal
copy-on-write transaction commit path, and flushes committed ranges through the
PMEM persistence engine. The default backend remains `VMemory`.

Real restart/power-fail persistence tests remain gated behind
`WASMTIME_TEST_REAL_PMEM=1` until durable recovery metadata and PMEM hardware
are available.
```

- [ ] **Step 5: Commit final log update**

Run:

```bash
git diff --check
git add docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Record NVMemory backend status"
```

## Self-Review

- Spec coverage: The plan covers the PMEM instruction layer, `NVMemoryBlockRegion`, `NVMemory`, backend selection, default `VMemory`, gated real-PMEM tests, WAST verification, and feature-boundary checks.
- Deferred by design: crash recovery, durable object-table recovery, persistent roots, and post-restart validation stay outside this backend wave.
- Type consistency: `PersistenceMode`, `PersistEngine`, `NVMemoryBlockRegion`, `NVMemory`, and `TMemoryBackend::NVMemory` are introduced before later tasks use them.
- Test policy: normal tests do not require PMEM hardware; the only real-PMEM test is explicitly ignored and env-gated.
