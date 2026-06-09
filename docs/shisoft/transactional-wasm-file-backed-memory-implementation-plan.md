# FileBackedMemory Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `FileBackedMemory` as the filesystem-backed transactional `tmemory` backend, using shared writable file mappings plus `msync` and `fdatasync`/`fsync` as the file-backed analogue of `CLWB` and `SFENCE`.

**Architecture:** `FileBackedMemory` follows the same storage layers as `VMemory` and `NVMemory`: `TMemoryBackendStorage -> TMemoryRegion -> MappedLinearRegion -> BlockRegionBackend`. The new block-region backend owns a shared writable mapping of a file, writes committed COW bytes into that mapping, flushes modified ranges with `msync(MS_SYNC)`, and fences with `fdatasync` by default. Temp-file and explicit-path modes are both explicit configuration paths; the default backend remains `VMemory`.

**Tech Stack:** Rust, Wasmtime runtime internals, `rustix::mm::{mmap, msync, munmap}`, `File::sync_data`, `File::sync_all`, `BlockRegionBackend`, `TMemoryBackendStorage`, existing transactional WAST harness.

---

## Design Reference

Use `docs/shisoft/transactional-wasm-file-backed-memory-design.md` as the source design. Keep the implementation behind the default-on `transaction` feature on this branch. Do not change ordinary Wasmtime memory or ordinary GC.

## File Map

- Modify: `crates/wasmtime/src/runtime/transaction.rs`
  - Add `TMemoryFileBacking`.
  - Add explicit config constructors for temp-file and path-backed `FileBackedMemory`.
  - Keep generic `with_tmemory_backend(TMemoryBackend::FileBackedMemory)` rejected.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
  - Add file-backed mapping helper.
  - Add `FileBackedMemoryBlockRegion`.
  - Add unit tests for shared mapping, flush/fence, growth remap support, and unsupported-platform diagnostics.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs`
  - Add `TMemoryRegion::new_file_backed`.
  - Add tests for file-backed line-mark metadata and flush/fence forwarding.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
  - Add `FileBackedMemory` implementing `TMemoryBackendStorage`.
  - Route config-driven `TMemoryBackend::FileBackedMemory` construction.
  - Keep direct generic backend construction rejected unless file backing config exists.
  - Add ignored recovery boundary test.
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
  - Record backend status and recovery boundary.

## Task 1: Add File-Backed Config Shape

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`

- [ ] **Step 1: Add failing config tests**

Add these tests inside the existing `#[cfg(test)] mod tests` in `crates/wasmtime/src/runtime/transaction.rs`:

```rust
#[test]
fn transaction_config_accepts_file_backed_temp_mode() {
    let config = TransactionConfig::with_file_backed_tmemory_temp().unwrap();

    assert_eq!(config.tmemory_backend(), TMemoryBackend::FileBackedMemory);
    assert_eq!(config.tmemory_file_backing(), Some(TMemoryFileBacking::Temp));
    assert!(!config.is_vmemory_only());
}

#[test]
fn transaction_config_accepts_file_backed_path_mode() {
    let path = std::path::PathBuf::from("/tmp/wasmtime-transaction-file-backed-test.tmemory");
    let config = TransactionConfig::with_file_backed_tmemory_path(path.clone()).unwrap();

    assert_eq!(config.tmemory_backend(), TMemoryBackend::FileBackedMemory);
    assert_eq!(
        config.tmemory_file_backing(),
        Some(TMemoryFileBacking::Path(path))
    );
    assert!(!config.is_vmemory_only());
}

#[test]
fn generic_file_backed_backend_selection_still_requires_file_mode() {
    let error =
        TransactionConfig::with_tmemory_backend(TMemoryBackend::FileBackedMemory).unwrap_err();
    assert!(error.to_string().contains("requires explicit file backing"));
}
```

- [ ] **Step 2: Run the config tests and confirm failure**

Run:

```bash
cargo test -p wasmtime --lib transaction_config_accepts_file_backed -- --format terse
cargo test -p wasmtime --lib generic_file_backed_backend_selection_still_requires_file_mode -- --format terse
```

Expected: compile failure because `TMemoryFileBacking` and the new config methods do not exist.

- [ ] **Step 3: Implement config shape**

In `crates/wasmtime/src/runtime/transaction.rs`, add imports near the top:

```rust
use std::path::PathBuf;
```

Add this enum after `TMemoryPersistenceMode`:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TMemoryFileBacking {
    Temp,
    Path(PathBuf),
}
```

Update `TransactionConfig`:

```rust
pub(crate) struct TransactionConfig {
    tmemory_backend: TMemoryBackend,
    tmemory_persistence_mode: TMemoryPersistenceMode,
    tmemory_file_backing: Option<TMemoryFileBacking>,
    concurrency_control: ConcurrencyControl,
    durability_policy: DurabilityPolicy,
    conflict_policy: ConflictPolicy,
}
```

Update `Default`:

```rust
tmemory_file_backing: None,
```

Add methods to `impl TransactionConfig`:

```rust
pub(crate) fn with_file_backed_tmemory_temp() -> Result<Self> {
    let mut config = Self::default();
    config.set_file_backed_tmemory(TMemoryFileBacking::Temp)?;
    Ok(config)
}

pub(crate) fn with_file_backed_tmemory_path(path: PathBuf) -> Result<Self> {
    ensure!(
        !path.as_os_str().is_empty(),
        "file-backed tmemory path cannot be empty"
    );
    let mut config = Self::default();
    config.set_file_backed_tmemory(TMemoryFileBacking::Path(path))?;
    Ok(config)
}

pub(crate) fn tmemory_file_backing(&self) -> Option<TMemoryFileBacking> {
    self.tmemory_file_backing.clone()
}

fn set_file_backed_tmemory(&mut self, file_backing: TMemoryFileBacking) -> Result<()> {
    self.tmemory_backend = TMemoryBackend::FileBackedMemory;
    self.tmemory_file_backing = Some(file_backing);
    Ok(())
}
```

Update `set_tmemory_backend` so `FileBackedMemory` remains rejected from the generic selector:

```rust
TMemoryBackend::FileBackedMemory => {
    bail!("FileBackedMemory requires explicit file backing configuration")
}
```

Ensure `set_tmemory_backend` clears stale file backing for `VMemory` and `NVMemory`:

```rust
TMemoryBackend::VMemory | TMemoryBackend::NVMemory => {
    self.tmemory_backend = tmemory_backend;
    self.tmemory_file_backing = None;
    Ok(())
}
```

Ensure `set_nvmemory_persistence_mode` also clears file backing:

```rust
self.tmemory_file_backing = None;
```

- [ ] **Step 4: Run tests**

Run:

```bash
cargo test -p wasmtime --lib transaction_config -- --format terse
cargo check -p wasmtime --no-default-features --features transaction
```

Expected: all selected tests/checks pass.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs
git commit -m "Add FileBackedMemory transaction config"
```

## Task 2: Add Shared File Mapping Helper

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`

- [ ] **Step 1: Add failing mapping tests**

Add these tests to the `#[cfg(test)] mod tests` in `block_region.rs`:

```rust
#[test]
fn file_backed_shared_mapping_writes_through_to_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mapping.tmemory");
    let mut mapping = FileBackedMapping::new_path(path.clone(), 4096).unwrap();

    mapping.write(16, &[1, 2, 3, 4]).unwrap();
    mapping.flush(16, 4).unwrap();
    mapping.fence_data().unwrap();

    let bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[16..20], &[1, 2, 3, 4]);
}

#[test]
fn file_backed_shared_mapping_rejects_out_of_bounds_flush() {
    let mut mapping = FileBackedMapping::new_temp(4096).unwrap();

    let error = mapping.flush(4096, 1).unwrap_err().to_string();
    assert!(error.contains("file-backed mapping flush range out of bounds"));
}

#[test]
fn file_backed_shared_mapping_remap_preserves_file_contents() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("remap.tmemory");
    let mut mapping = FileBackedMapping::new_path(path.clone(), 4096).unwrap();

    mapping.write(32, &[9, 8, 7, 6]).unwrap();
    mapping.flush(32, 4).unwrap();
    mapping.fence_data().unwrap();
    mapping.remap_len(8192).unwrap();

    assert_eq!(mapping.read(32, 4).unwrap(), vec![9, 8, 7, 6]);
    assert_eq!(std::fs::metadata(path).unwrap().len(), 8192);
}
```

- [ ] **Step 2: Run mapping tests and confirm failure**

Run:

```bash
cargo test -p wasmtime --lib file_backed_shared_mapping -- --format terse
```

Expected: compile failure because `FileBackedMapping` does not exist.

- [ ] **Step 3: Add file-backed mapping types**

Add imports near the top of `block_region.rs`:

```rust
use crate::runtime::vm::SendSyncPtr;
use core::ptr::NonNull;
use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
```

If there is an existing `Ordering` import for compiler fences, alias one of the imports so the names do not collide.

Add a counter near the constants:

```rust
static FILE_BACKED_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
```

Add mode and mapping structs:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FileBackedRegionMode {
    Temp,
    Path(PathBuf),
}

#[derive(Debug)]
pub(crate) struct FileBackedMapping {
    file: File,
    path: PathBuf,
    unlink_on_drop: bool,
    memory: SendSyncPtr<[u8]>,
}
```

Add platform-independent constructor shell:

```rust
impl FileBackedMapping {
    pub(crate) fn new(mode: FileBackedRegionMode, len: usize) -> Result<Self> {
        match mode {
            FileBackedRegionMode::Temp => Self::new_temp(len),
            FileBackedRegionMode::Path(path) => Self::new_path(path, len),
        }
    }

    pub(crate) fn new_temp(len: usize) -> Result<Self> {
        let path = unique_temp_file_path();
        let mapping = Self::new_path_with_unlink(path, len, true)?;
        Ok(mapping)
    }

    pub(crate) fn new_path(path: PathBuf, len: usize) -> Result<Self> {
        Self::new_path_with_unlink(path, len, false)
    }

    fn new_path_with_unlink(path: PathBuf, len: usize, unlink_on_drop: bool) -> Result<Self> {
        new_file_backed_mapping(path, len, unlink_on_drop)
    }
}
```

Add helpers:

```rust
fn unique_temp_file_path() -> PathBuf {
    let counter = FILE_BACKED_TEMP_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    std::env::temp_dir().join(format!(
        "wasmtime-transaction-tmemory-{}-{counter}.bin",
        std::process::id()
    ))
}
```

- [ ] **Step 4: Add Unix implementation**

Add this Unix implementation. Adjust imports if rustfmt moves them.

```rust
#[cfg(unix)]
fn new_file_backed_mapping(path: PathBuf, len: usize, unlink_on_drop: bool) -> Result<FileBackedMapping> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .with_context(|| format!("failed to open file-backed tmemory file {}", path.display()))?;
    file.set_len(u64::try_from(len).context("file-backed tmemory length overflow")?)
        .with_context(|| format!("failed to resize file-backed tmemory file {}", path.display()))?;
    let memory = unsafe { map_shared_file(&file, len)? };
    Ok(FileBackedMapping {
        file,
        path,
        unlink_on_drop,
        memory,
    })
}

#[cfg(unix)]
unsafe fn map_shared_file(file: &File, len: usize) -> Result<SendSyncPtr<[u8]>> {
    if len == 0 {
        return Ok(SendSyncPtr::new(NonNull::slice_from_raw_parts(
            NonNull::<u8>::dangling(),
            0,
        )));
    }
    let ptr = unsafe {
        rustix::mm::mmap(
            core::ptr::null_mut(),
            len,
            rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
            rustix::mm::MapFlags::SHARED,
            file,
            0,
        )
        .context("failed to mmap shared file-backed tmemory region")?
    };
    let slice = core::ptr::slice_from_raw_parts_mut(ptr.cast::<u8>(), len);
    Ok(SendSyncPtr::new(NonNull::new(slice).unwrap()))
}

#[cfg(unix)]
unsafe fn unmap_shared_file(memory: SendSyncPtr<[u8]>) -> Result<()> {
    let len = memory.len();
    if len == 0 {
        return Ok(());
    }
    unsafe {
        rustix::mm::munmap(memory.as_non_null().cast::<u8>().as_ptr().cast(), len)
            .context("failed to munmap file-backed tmemory region")?;
    }
    Ok(())
}
```

Add non-Unix stubs:

```rust
#[cfg(not(unix))]
fn new_file_backed_mapping(
    _path: PathBuf,
    _len: usize,
    _unlink_on_drop: bool,
) -> Result<FileBackedMapping> {
    bail!("FileBackedMemory is unsupported on this target until shared writable mmap support exists")
}
```

Add methods:

```rust
impl FileBackedMapping {
    pub(crate) fn len(&self) -> usize {
        self.memory.len()
    }

    pub(crate) fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        let end = offset
            .checked_add(len)
            .context("file-backed mapping read range overflow")?;
        ensure!(end <= self.len(), "file-backed mapping read range out of bounds");
        let slice = unsafe { self.memory.as_ref() };
        Ok(slice[offset..end].to_vec())
    }

    pub(crate) fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        let end = offset
            .checked_add(bytes.len())
            .context("file-backed mapping write range overflow")?;
        ensure!(end <= self.len(), "file-backed mapping write range out of bounds");
        let slice = unsafe { self.memory.as_mut() };
        slice[offset..end].copy_from_slice(bytes);
        Ok(())
    }

    pub(crate) fn flush(&self, offset: usize, len: usize) -> Result<()> {
        let end = offset
            .checked_add(len)
            .context("file-backed mapping flush range overflow")?;
        ensure!(end <= self.len(), "file-backed mapping flush range out of bounds");
        if len == 0 {
            return Ok(());
        }
        flush_file_backed_mapping(self.memory, offset, len)
    }

    pub(crate) fn fence_data(&self) -> Result<()> {
        self.file
            .sync_data()
            .context("failed to sync_data file-backed tmemory")
    }

    pub(crate) fn fence_all(&self) -> Result<()> {
        self.file
            .sync_all()
            .context("failed to sync_all file-backed tmemory")
    }

    pub(crate) fn remap_len(&mut self, new_len: usize) -> Result<()> {
        #[cfg(unix)]
        {
            self.file
                .set_len(u64::try_from(new_len).context("file-backed tmemory length overflow")?)
                .with_context(|| format!("failed to resize file-backed tmemory file {}", self.path.display()))?;
            unsafe { unmap_shared_file(self.memory)? };
            self.memory = unsafe { map_shared_file(&self.file, new_len)? };
            self.fence_all()
        }
        #[cfg(not(unix))]
        {
            let _ = new_len;
            bail!("FileBackedMemory remap is unsupported on this target")
        }
    }
}
```

Add Unix flush helper:

```rust
#[cfg(unix)]
fn flush_file_backed_mapping(memory: SendSyncPtr<[u8]>, offset: usize, len: usize) -> Result<()> {
    let page_size = rustix::param::page_size();
    let aligned_start = offset & !(page_size - 1);
    let end = offset
        .checked_add(len)
        .context("file-backed mapping flush range overflow")?;
    let aligned_end = end
        .checked_add(page_size - 1)
        .context("file-backed mapping flush alignment overflow")?
        & !(page_size - 1);
    let aligned_len = aligned_end - aligned_start;
    unsafe {
        rustix::mm::msync(
            memory
                .as_non_null()
                .cast::<u8>()
                .as_ptr()
                .add(aligned_start)
                .cast(),
            aligned_len,
            rustix::mm::MsyncFlags::SYNC,
        )
        .context("failed to msync file-backed tmemory range")?;
    }
    Ok(())
}
```

Add `Drop`:

```rust
impl Drop for FileBackedMapping {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            let _ = unmap_shared_file(self.memory);
        }
        if self.unlink_on_drop {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
```

- [ ] **Step 5: Run mapping tests**

Run:

```bash
cargo test -p wasmtime --lib file_backed_shared_mapping -- --format terse
```

Expected on Unix: all mapping tests pass. On non-Unix: adjust the tests with `#[cfg(unix)]` and add a non-Unix unsupported test.

- [ ] **Step 6: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
git commit -m "Add shared file mapping for tmemory"
```

## Task 3: Add FileBackedMemory Block Region

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`

- [ ] **Step 1: Add failing block-region tests**

Add these tests in `block_region.rs`:

```rust
#[test]
fn file_backed_block_region_allocates_and_flushes_writes() {
    let mut region =
        FileBackedMemoryBlockRegion::new_for_test(2, FileBackedRegionMode::Temp).unwrap();
    let chunk = region.alloc_chunk(1).unwrap();
    let range = chunk.byte_range();

    region.write(range.start + 8, &[5, 6, 7, 8]).unwrap();
    region.flush(range.start + 8, 4).unwrap();
    region.fence().unwrap();

    assert_eq!(region.read(range.start + 8, 4).unwrap(), vec![5, 6, 7, 8]);
    assert_eq!(region.block_size(), BLOCK_SIZE);
    assert_eq!(region.num_blocks(), 2);
}

#[test]
fn file_backed_block_region_rejects_out_of_bounds_flush() {
    let region =
        FileBackedMemoryBlockRegion::new_for_test(1, FileBackedRegionMode::Temp).unwrap();

    let error = region.flush(BLOCK_SIZE, 1).unwrap_err().to_string();
    assert!(error.contains("flush range out of bounds"));
}
```

- [ ] **Step 2: Run tests and confirm failure**

Run:

```bash
cargo test -p wasmtime --lib file_backed_block_region -- --format terse
```

Expected: compile failure because `FileBackedMemoryBlockRegion` does not exist.

- [ ] **Step 3: Implement `FileBackedMemoryBlockRegion`**

Add this struct after `NVMemoryBlockRegion`:

```rust
#[derive(Debug)]
pub(crate) struct FileBackedMemoryBlockRegion {
    mapping: FileBackedMapping,
    block_entries: Vec<BlockEntry>,
    line_marks: Vec<LineMark>,
}
```

Add implementation:

```rust
impl FileBackedMemoryBlockRegion {
    pub(crate) fn new(num_blocks: usize, mode: FileBackedRegionMode) -> Result<Self> {
        let bytes_len = num_blocks
            .checked_mul(BLOCK_SIZE)
            .context("transactional FileBackedMemory block region size overflow")?;
        let line_count = bytes_len / IMMIX_LINE_SIZE;
        Ok(Self {
            mapping: FileBackedMapping::new(mode, bytes_len)?,
            block_entries: vec![BlockEntry::default(); num_blocks],
            line_marks: vec![
                LineMark {
                    mark: IMMIX_LINE_MARK_RESET_VALUE,
                };
                line_count
            ],
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(num_blocks: usize, mode: FileBackedRegionMode) -> Result<Self> {
        Self::new(num_blocks, mode)
    }

    pub(crate) fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    pub(crate) fn num_blocks(&self) -> usize {
        self.block_entries.len()
    }

    pub(crate) fn bytes_len(&self) -> usize {
        self.mapping.len()
    }

    pub(crate) fn line_count(&self) -> usize {
        self.line_marks.len()
    }

    pub(crate) fn alloc_chunk(&mut self, block_count: usize) -> Result<RegionChunk> {
        ensure!(
            block_count > 0,
            "transactional FileBackedMemory chunk must contain a block"
        );
        ensure!(
            block_count <= self.num_blocks(),
            "transactional FileBackedMemory chunk exceeds region size"
        );
        let last_start = self.num_blocks() - block_count;
        for start in 0..=last_start {
            let end = start + block_count;
            if self.block_entries[start..end].iter().all(|entry| entry.used == 0) {
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
        bail!("transactional FileBackedMemory block region is out of contiguous chunks")
    }

    pub(crate) fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        self.mapping.read(offset, len)
    }

    pub(crate) fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        self.mapping.write(offset, bytes)
    }

    pub(crate) fn flush(&self, offset: usize, len: usize) -> Result<()> {
        let end = offset
            .checked_add(len)
            .context("transactional FileBackedMemory block flush range overflow")?;
        ensure!(
            end <= self.mapping.len(),
            "transactional FileBackedMemory block flush range out of bounds"
        );
        self.mapping.flush(offset, len)
    }

    pub(crate) fn fence(&self) -> Result<()> {
        self.mapping.fence_data()
    }
}
```

Add `BlockRegionBackend for FileBackedMemoryBlockRegion`, mirroring `NVMemoryBlockRegion`:

```rust
impl BlockRegionBackend for FileBackedMemoryBlockRegion {
    fn block_size(&self) -> usize { self.block_size() }
    fn num_blocks(&self) -> usize { self.num_blocks() }
    fn bytes_len(&self) -> usize { self.bytes_len() }
    fn line_mark_count(&self) -> usize { self.line_count() }
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
cargo test -p wasmtime --lib file_backed_block_region -- --format terse
```

Expected: tests pass on Unix. Keep non-Unix unsupported behavior explicit.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
git commit -m "Add FileBackedMemory block region"
```

## Task 4: Add FileBackedMemory Linear Region

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs`

- [ ] **Step 1: Add failing linear-region tests**

Add these tests to `linear_region.rs`:

```rust
#[test]
fn file_backed_region_reports_backend_line_mark_count() {
    let empty = TMemoryRegion::new_file_backed(0, FileBackedRegionMode::Temp).unwrap();
    assert_eq!(empty.line_mark_count_for_test(), 0);

    let one_byte = TMemoryRegion::new_file_backed(1, FileBackedRegionMode::Temp).unwrap();
    assert_eq!(
        one_byte.line_mark_count_for_test(),
        BLOCK_SIZE / IMMIX_LINE_SIZE
    );
}

#[test]
fn file_backed_region_flush_and_fence_paths_are_supported() {
    let mut region = TMemoryRegion::new_file_backed(BLOCK_SIZE, FileBackedRegionMode::Temp)
        .unwrap();

    region.write(0, &[1, 2, 3, 4]).unwrap();
    region.flush(0, 4).unwrap();
    region.fence().unwrap();

    assert_eq!(region.read(0, 4).unwrap(), vec![1, 2, 3, 4]);
}
```

- [ ] **Step 2: Run tests and confirm failure**

Run:

```bash
cargo test -p wasmtime --lib file_backed_region -- --format terse
```

Expected: compile failure because `new_file_backed` does not exist.

- [ ] **Step 3: Add constructor**

Update imports in `linear_region.rs`:

```rust
use super::block_region::{
    BLOCK_SIZE, BlockRegionBackend, ChunkList, FileBackedMemoryBlockRegion,
    FileBackedRegionMode, IMMIX_LINE_SIZE, NVMemoryBlockRegion, PersistenceMode,
    VMemoryBlockRegion,
};
```

Add constructor to `impl TMemoryRegion`:

```rust
pub(super) fn new_file_backed(byte_capacity: usize, mode: FileBackedRegionMode) -> Result<Self> {
    let block_count = byte_capacity.div_ceil(BLOCK_SIZE);
    let mut backend = FileBackedMemoryBlockRegion::new(block_count, mode)?;
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

- [ ] **Step 4: Run linear-region tests**

Run:

```bash
cargo test -p wasmtime --lib file_backed_region -- --format terse
```

Expected: tests pass on Unix.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs
git commit -m "Add FileBackedMemory linear region"
```

## Task 5: Add FileBackedMemory Storage Backend

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`

- [ ] **Step 1: Add failing storage tests**

Add tests to `tmemory.rs`:

```rust
#[test]
fn tmemory_can_construct_file_backed_temp_backend() {
    let config = TransactionConfig::with_file_backed_tmemory_temp().unwrap();
    let memory = TMemory::new(config, 1, Some(1)).unwrap();

    assert_eq!(memory.backend(), TMemoryBackend::FileBackedMemory);
    assert_eq!(memory.byte_len(), WASM_PAGE_SIZE);
    assert_eq!(memory.block_region_block_size_for_test(), BLOCK_SIZE);
    assert_eq!(memory.immix_line_size_for_test(), IMMIX_LINE_SIZE);
}

#[test]
fn file_backed_commit_range_writes_and_versions_granules() {
    let config = TransactionConfig::with_file_backed_tmemory_temp().unwrap();
    let mut memory = TMemory::new(config, 1, Some(1)).unwrap();

    memory.commit_range(4, &[10, 11, 12, 13]).unwrap();

    assert_eq!(memory.read_committed(4..8).unwrap(), vec![10, 11, 12, 13]);
    assert_eq!(memory.granule_version(0).unwrap(), 1);
}

#[test]
fn file_backed_grow_beyond_capacity_preserves_bytes_and_metadata() {
    let config = TransactionConfig::with_file_backed_tmemory_temp().unwrap();
    let mut memory = TMemory::new(config, 1, None).unwrap();
    memory.commit_range(8, &[1, 2, 3, 4]).unwrap();
    let mut info = memory.granule_info(0).unwrap();
    info.owner = 7;
    info.hash = 99;
    memory.set_granule_info(0, info).unwrap();

    memory.grow_to_pages(2).unwrap();

    assert_eq!(memory.byte_len(), 2 * WASM_PAGE_SIZE);
    assert_eq!(memory.byte_capacity(), 2 * WASM_PAGE_SIZE);
    assert_eq!(memory.read_committed(8..12).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(memory.granule_info(0).unwrap(), info);
    assert_eq!(
        memory.granule_info(GRANULES_PER_WASM_PAGE).unwrap(),
        TMemoryGranuleInfo::default()
    );
}

#[test]
fn generic_file_backed_constructor_remains_explicitly_unsupported() {
    let error = TMemory::new_with_backend(TMemoryBackend::FileBackedMemory, 1)
        .unwrap_err()
        .to_string();
    assert!(error.contains("FileBackedMemory requires explicit file backing"));
}
```

- [ ] **Step 2: Run storage tests and confirm failure**

Run:

```bash
cargo test -p wasmtime --lib file_backed -- --format terse
```

Expected: compile failure because `FileBackedMemory` storage is not implemented.

- [ ] **Step 3: Route config through `TMemory::new`**

Update imports:

```rust
use crate::runtime::transaction::{
    TMEMORY_GRANULE_SHIFT, TMEMORY_GRANULE_SIZE, TMemoryBackend, TMemoryFileBacking,
    TMemoryPersistenceMode, TransactionConfig,
};
```

Change `TMemory::new` to pass file backing:

```rust
Self::new_for_backend(
    config.tmemory_backend(),
    config.tmemory_persistence_mode(),
    config.tmemory_file_backing(),
    min_pages,
    max_pages,
)
```

Change direct `new_with_backend_limits` to pass `None`:

```rust
Self::new_for_backend(
    backend,
    TMemoryPersistenceMode::ResearchPretendPmem,
    None,
    min_pages,
    max_pages,
)
```

Change `new_for_backend` signature:

```rust
fn new_for_backend(
    backend: TMemoryBackend,
    persistence_mode: TMemoryPersistenceMode,
    file_backing: Option<TMemoryFileBacking>,
    min_pages: u64,
    max_pages: Option<u64>,
) -> Result<Self>
```

Route `FileBackedMemory`:

```rust
TMemoryBackend::FileBackedMemory => {
    let file_backing = file_backing
        .context("FileBackedMemory requires explicit file backing configuration")?;
    Box::new(FileBackedMemory::new(min_pages, max_pages, file_backing)?)
}
```

- [ ] **Step 4: Add `FileBackedMemory` storage**

Add this struct after `NVMemory`:

```rust
#[derive(Debug)]
pub(crate) struct FileBackedMemory {
    region: TMemoryRegion,
    file_backing: TMemoryFileBacking,
    granules: Vec<TMemoryGranuleInfo>,
    byte_len: usize,
    byte_capacity: usize,
    max_pages: u64,
}
```

Add constructor and helpers:

```rust
impl FileBackedMemory {
    pub(crate) fn new(
        min_pages: u64,
        max_pages: Option<u64>,
        file_backing: TMemoryFileBacking,
    ) -> Result<Self> {
        let requested_max_pages = max_pages;
        let max_pages = max_pages.unwrap_or(DEFAULT_MAX_WASM_PAGES);
        ensure!(min_pages <= max_pages, "tmemory minimum exceeds maximum");
        let byte_len = pages_to_bytes(min_pages)?;
        let byte_capacity = match requested_max_pages {
            Some(max_pages) => pages_to_bytes(max_pages)?,
            None => byte_len,
        };
        let granule_capacity = granules_for_bytes(byte_capacity);
        let region_mode = file_backed_region_mode(&file_backing);

        Ok(Self {
            region: TMemoryRegion::new_file_backed(byte_capacity, region_mode)?,
            file_backing,
            granules: vec![TMemoryGranuleInfo::default(); granule_capacity],
            byte_len,
            byte_capacity,
            max_pages,
        })
    }

    pub(crate) fn byte_len(&self) -> usize { self.byte_len }
    pub(crate) fn granule_len(&self) -> usize { granules_for_bytes(self.byte_len) }

    pub(crate) fn read(&self, range: core::ops::Range<usize>) -> Result<Vec<u8>> {
        ensure!(range.start <= range.end, "tmemory read invalid range");
        ensure!(range.end <= self.byte_len, "tmemory read out of bounds");
        self.region.read(range.start, range.end - range.start)
    }

    pub(crate) fn commit_range(&mut self, addr: usize, bytes: &[u8]) -> Result<()> {
        ensure!(addr <= self.byte_len, "tmemory write out of bounds");
        let end = addr
            .checked_add(bytes.len())
            .context("tmemory write address overflow")?;
        ensure!(end <= self.byte_len, "tmemory write out of bounds");
        if bytes.is_empty() {
            return Ok(());
        }
        self.region.write(addr, bytes)?;
        self.region.flush(addr, bytes.len())?;
        self.region.fence()
    }

    pub(crate) fn grow_to_pages(&mut self, new_pages: u64) -> Result<()> {
        ensure!(new_pages <= self.max_pages, "tmemory growth exceeds maximum size");
        let new_byte_len = pages_to_bytes(new_pages)?;
        if new_byte_len < self.byte_len {
            bail!("tmemory grow cannot shrink");
        }
        if new_byte_len == self.byte_len {
            return Ok(());
        }
        if new_byte_len > self.byte_capacity {
            self.reserve_capacity_to_pages(new_pages)?;
        }
        self.byte_len = new_byte_len;
        Ok(())
    }

    pub(crate) fn can_grow_to_pages(&self, new_pages: u64) -> bool {
        if new_pages > self.max_pages {
            return false;
        }
        let Ok(new_byte_len) = pages_to_bytes(new_pages) else {
            return false;
        };
        new_byte_len >= self.byte_len
    }

    fn reserve_capacity_to_pages(&mut self, new_capacity_pages: u64) -> Result<()> {
        let new_byte_capacity = pages_to_bytes(new_capacity_pages)?;
        ensure!(
            new_byte_capacity >= self.byte_len,
            "tmemory capacity cannot shrink below live size"
        );
        if new_byte_capacity <= self.byte_capacity {
            return Ok(());
        }
        let old = if self.byte_len > 0 {
            Some(self.region.read(0, self.byte_len)?)
        } else {
            None
        };
        let mut region =
            TMemoryRegion::new_file_backed(new_byte_capacity, file_backed_region_mode(&self.file_backing))?;
        if let Some(old) = old {
            region.write(0, &old)?;
            region.flush(0, self.byte_len)?;
            region.fence()?;
        }
        self.granules
            .resize(granules_for_bytes(new_byte_capacity), TMemoryGranuleInfo::default());
        self.region = region;
        self.byte_capacity = new_byte_capacity;
        Ok(())
    }
}
```

Add mode conversion:

```rust
fn file_backed_region_mode(file_backing: &TMemoryFileBacking) -> block_region::FileBackedRegionMode {
    match file_backing {
        TMemoryFileBacking::Temp => block_region::FileBackedRegionMode::Temp,
        TMemoryFileBacking::Path(path) => block_region::FileBackedRegionMode::Path(path.clone()),
    }
}
```

Add `txn_info`, `set_txn_info`, and test-only block/line helpers by copying the `NVMemory` method shape and changing the receiver to `FileBackedMemory`.

Add `impl TMemoryBackendStorage for FileBackedMemory`, mirroring `NVMemory`:

```rust
impl TMemoryBackendStorage for FileBackedMemory {
    fn backend_kind(&self) -> TMemoryBackend {
        TMemoryBackend::FileBackedMemory
    }
    fn byte_len(&self) -> usize { self.byte_len() }
    fn byte_capacity(&self) -> usize { self.byte_capacity }
    fn granule_count(&self) -> usize { self.granule_len() }
    fn read_committed(&self, range: core::ops::Range<usize>) -> Result<Vec<u8>> {
        self.read(range)
    }
    fn commit_range(&mut self, addr: usize, bytes: &[u8]) -> Result<()> {
        FileBackedMemory::commit_range(self, addr, bytes)
    }
    fn can_grow_to_pages(&self, new_pages: u64) -> bool {
        FileBackedMemory::can_grow_to_pages(self, new_pages)
    }
    fn grow_to_pages(&mut self, new_pages: u64) -> Result<()> {
        FileBackedMemory::grow_to_pages(self, new_pages)
    }
    fn granule_info(&self, granule: usize) -> Result<TMemoryGranuleInfo> {
        self.txn_info(granule)
    }
    fn set_granule_info(&mut self, granule: usize, info: TMemoryGranuleInfo) -> Result<()> {
        self.set_txn_info(granule, info)
    }
    #[cfg(test)]
    fn block_region_block_size_for_test(&self) -> usize {
        self.region.block_size_for_test()
    }
    #[cfg(test)]
    fn immix_line_size_for_test(&self) -> usize {
        self.region.immix_line_size_for_test()
    }
    #[cfg(test)]
    fn line_mark_count_for_test(&self) -> usize {
        self.region.line_mark_count_for_test()
    }
}
```

- [ ] **Step 5: Run storage tests**

Run:

```bash
cargo test -p wasmtime --lib file_backed -- --format terse
cargo test -p wasmtime --lib tmemory -- --format terse
```

Expected: file-backed and tmemory tests pass on Unix. The direct generic constructor test should still reject `FileBackedMemory`.

- [ ] **Step 6: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/vm/memory/tmemory.rs
git commit -m "Add FileBackedMemory tmemory backend"
```

## Task 6: Add Explicit Path Test And Recovery Boundary

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`

- [ ] **Step 1: Add explicit-path and ignored recovery tests**

Add tests to `tmemory.rs`:

```rust
#[test]
fn file_backed_explicit_path_publishes_committed_bytes_to_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("explicit.tmemory");
    let config = TransactionConfig::with_file_backed_tmemory_path(path.clone()).unwrap();
    let mut memory = TMemory::new(config, 1, Some(1)).unwrap();

    memory.commit_range(64, &[42, 43, 44, 45]).unwrap();

    let bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[64..68], &[42, 43, 44, 45]);
}

#[test]
#[ignore = "requires restart recovery metadata and WASMTIME_TEST_FILE_BACKED_TMEMORY_RECOVERY=1"]
fn file_backed_memory_restart_recovery_is_not_implemented_yet() {
    if std::env::var_os("WASMTIME_TEST_FILE_BACKED_TMEMORY_RECOVERY").is_none() {
        eprintln!(
            "set WASMTIME_TEST_FILE_BACKED_TMEMORY_RECOVERY=1 after durable recovery metadata exists"
        );
        return;
    }

    panic!("file-backed tmemory restart recovery is not implemented in this backend wave");
}
```

- [ ] **Step 2: Run explicit-path test**

Run:

```bash
cargo test -p wasmtime --lib file_backed_explicit_path_publishes_committed_bytes_to_file -- --format terse
```

Expected: test passes on Unix.

- [ ] **Step 3: Run ignored recovery marker explicitly**

Run:

```bash
cargo test -p wasmtime --lib file_backed_memory_restart_recovery_is_not_implemented_yet -- --ignored --format terse
```

Expected with env var unset: ignored test runs, prints the guidance message, and passes. If `WASMTIME_TEST_FILE_BACKED_TMEMORY_RECOVERY=1` is set, it should panic with the explicit not-implemented message.

- [ ] **Step 4: Run normal file-backed filter**

Run:

```bash
cargo test -p wasmtime --lib file_backed -- --format terse
```

Expected: all non-ignored file-backed tests pass; recovery marker remains ignored in normal runs.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/vm/memory/tmemory.rs
git commit -m "Document FileBackedMemory recovery boundary"
```

## Task 7: Update Status Docs And Verify Feature Boundaries

**Files:**
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Add implementation log section**

Add this section near the top of `docs/shisoft/transactional-wasm-implementation-log.md`, after the `NVMemory Backend Implementation` section:

```markdown
## FileBackedMemory Backend Implementation

Date: 2026-06-09

`FileBackedMemory` now exists as the filesystem-backed transactional memory
backend. It uses the same block/chunk and copy-on-write commit path as
`VMemory` and `NVMemory`, publishes committed bytes into a shared writable file
mapping, and flushes/fences through filesystem durability APIs. The default
backend remains `VMemory`; file-backed temp-file and explicit-path modes are
both opt-in transaction configuration choices.

Restart/power-fail recovery remains gated behind
`WASMTIME_TEST_FILE_BACKED_TMEMORY_RECOVERY=1` until durable region headers,
commit metadata, and recovery loading are implemented.
```

- [ ] **Step 2: Run focused backend tests**

Run:

```bash
cargo test -p wasmtime --lib transaction_config -- --format terse
cargo test -p wasmtime --lib file_backed -- --format terse
cargo test -p wasmtime --lib nvmemory -- --format terse
cargo test -p wasmtime --lib tmemory -- --format terse
```

Expected: all selected non-ignored tests pass.

- [ ] **Step 3: Run transaction proposal WAST**

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```

Expected: 173 transaction-proposal tests pass with 0 ignored.

- [ ] **Step 4: Run feature-boundary checks**

Run:

```bash
cargo check -p wasmtime --no-default-features --features transaction
cargo check -p wasmtime --no-default-features --features runtime,std,gc
```

Expected: both commands pass.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Record FileBackedMemory backend status"
```

## Self-Review

- Spec coverage: The plan covers temp-file mode, explicit-path mode, shared writable mappings, `msync`, `fdatasync`/`fsync`, config selection, backend storage, tests, docs, and recovery gating.
- Scope control: The plan does not add public Wasmtime API, restart recovery, durable object-table recovery, durable object heap recovery, or ordinary-memory changes.
- Type consistency: `TMemoryFileBacking`, `FileBackedRegionMode`, `FileBackedMapping`, `FileBackedMemoryBlockRegion`, and `FileBackedMemory` are introduced before later tasks use them.
- Platform boundary: Unix gets the first implementation; non-Unix must fail clearly instead of silently pretending to persist.
- Test policy: Normal tests use temp files and do not require crash recovery. Restart recovery remains ignored/env-gated.
