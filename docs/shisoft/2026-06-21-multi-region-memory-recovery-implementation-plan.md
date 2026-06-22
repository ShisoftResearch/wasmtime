# Multi-Region Memory And Parallel Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement user-configured multi-region transactional memory with address-window placement and NUMA-local parallel recovery using 8 recovery workers per PMEM region.

**Architecture:** Add a `RegionSet` layer below one transactional memory. Each region owns one fixed virtual address window, one homogeneous backend storage object, and one local metadata/allocator/log image. Normal object and linear-memory reads and writes use mapped addresses directly; region logic exists only in allocation, mapping, recovery, GC, and tests. Recovery runs one top-level task per region and uses 8-worker region recovery inside each task.

**Tech Stack:** Rust, Wasmtime runtime transaction internals, Linux `mmap`/`sched_getcpu`/`sched_setaffinity`, fsdax DAX mappings, file-backed mappings, existing `RecoveryOptions`, `cargo test`.

---

## Source Roadmap

This plan implements the approved roadmap:

- `docs/shisoft/2026-06-21-multi-region-memory-recovery-roadmap.md`

Required invariants from that roadmap:

- Do not change durable log record typing.
- Do not add region or shard identifiers to durable log records.
- Do not branch on address for normal reads or writes.
- Use virtual address windows as the placement contract.
- Allow one Wasmtime transactional memory to contain multiple regions.
- Require all regions in one memory to use the same backend kind.
- Recover `/pmem0` with 8 workers on socket 0 and `/pmem1` with 8 workers on socket 1 for 16 total workers on the two-socket Optane machine.

## File Structure

- Create `crates/wasmtime/src/runtime/vm/memory/tmemory/region.rs`
  - Region ids, region descriptors, address-window validation, homogeneous backend validation.
- Create `crates/wasmtime/src/runtime/vm/memory/tmemory/numa.rs`
  - Linux NUMA discovery, CPU-list parsing, current CPU/node lookup, optional thread pinning.
- Modify `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
  - Export `region` and `numa` modules internally.
  - Add multi-region constructors to `TMemory`/`DaxPmemMemory` after backend support exists.
- Modify `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
  - Add fixed-window mapping support for file-backed and DAX mappings.
  - Keep existing non-fixed constructors as compatibility wrappers.
- Modify `crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs`
  - Add constructors that build linear memory from a region set while preserving direct address access.
- Modify `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
  - Add recovery merge helpers and a composite mapped source used only after region-local recovery.
- Modify `crates/wasmtime/src/runtime/transaction/config.rs`
  - Add multi-region backend configuration forms for DAX and file-backed storage.
- Modify `crates/wasmtime/src/runtime/transaction/persist.rs`
  - Expose the existing `DurableRegionLog<R>` shape to a sibling module.
  - Add constructors for multi-region DAX/file-backed durable logs.
- Create `crates/wasmtime/src/runtime/transaction/persist_region_set.rs`
  - Multi-region durable-log backend, thread-owned region selection, per-region flush/fence fanout, parallel region recovery.
- Modify `crates/wasmtime/src/runtime/transaction/mod.rs`
  - Register the new transaction module.
- Modify `crates/wasmtime/src/runtime/transaction/object_table.rs`
  - Add a dedicated rebuild entry point for composite recovered mapped sources.
- Modify `crates/wasmtime/src/runtime/transaction/state.rs`
  - Route persistent GC chunk retirement through the multi-region durable log.
- Modify `crates/wasmtime/src/runtime/transaction/persist.rs` stress tests
  - Add two-region DAX stress and reporting.

## Task 1: Region Descriptor Model

**Files:**
- Create: `crates/wasmtime/src/runtime/vm/memory/tmemory/region.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Test: `crates/wasmtime/src/runtime/vm/memory/tmemory/region.rs`

- [ ] **Step 1: Write failing descriptor-validation tests**

Add this test module to the new file:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn region(id: u32, start: usize, len: usize, backend: RegionBackendKind) -> RegionDescriptor {
        RegionDescriptor {
            id: RegionId(id),
            backend,
            path: PathBuf::from(format!("/tmp/region-{id}")),
            window: RegionAddressWindow {
                base: start,
                reserved_len: len,
                mapped_len: len / 2,
            },
            numa_node: None,
            cpu_set: None,
        }
    }

    #[test]
    fn region_set_accepts_non_overlapping_homogeneous_regions() {
        let set = RegionSetDescriptor::new(vec![
            region(0, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem),
            region(1, 0x2000_0000, 0x0100_0000, RegionBackendKind::DaxPmem),
        ])
        .unwrap();
        assert_eq!(set.regions().len(), 2);
        assert_eq!(set.backend_kind(), RegionBackendKind::DaxPmem);
    }

    #[test]
    fn region_set_rejects_mixed_backend_kinds() {
        let err = RegionSetDescriptor::new(vec![
            region(0, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem),
            region(1, 0x2000_0000, 0x0100_0000, RegionBackendKind::FileBacked),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("same backend kind"));
    }

    #[test]
    fn region_set_rejects_overlapping_windows() {
        let err = RegionSetDescriptor::new(vec![
            region(0, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem),
            region(1, 0x1008_0000, 0x0100_0000, RegionBackendKind::DaxPmem),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("overlap"));
    }

    #[test]
    fn region_set_rejects_mapped_len_larger_than_reserved_len() {
        let mut bad = region(0, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem);
        bad.window.mapped_len = bad.window.reserved_len + 1;
        let err = RegionSetDescriptor::new(vec![bad]).unwrap_err();
        assert!(err.to_string().contains("mapped length"));
    }

    #[test]
    fn region_set_rejects_duplicate_region_ids() {
        let err = RegionSetDescriptor::new(vec![
            region(7, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem),
            region(7, 0x3000_0000, 0x0100_0000, RegionBackendKind::DaxPmem),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("duplicate region id"));
    }

    #[test]
    fn region_set_finds_region_for_management_address() {
        let set = RegionSetDescriptor::new(vec![
            region(0, 0x1000_0000, 0x0100_0000, RegionBackendKind::DaxPmem),
            region(1, 0x2000_0000, 0x0100_0000, RegionBackendKind::DaxPmem),
        ])
        .unwrap();
        assert_eq!(
            set.region_for_management_addr(0x2000_1234).unwrap().id,
            RegionId(1)
        );
        assert!(set.region_for_management_addr(0x3000_0000).is_none());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```sh
cargo test -p wasmtime region_set_ --lib
```

Expected: fail because `tmemory::region` and the region descriptor types do not exist.

- [ ] **Step 3: Implement region descriptors**

Add `pub(crate) mod region;` in `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`.

Implement this API in `region.rs`:

```rust
use crate::prelude::*;
use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct RegionId(pub(crate) u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegionBackendKind {
    DaxPmem,
    FileBacked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RegionAddressWindow {
    pub(crate) base: usize,
    pub(crate) reserved_len: usize,
    pub(crate) mapped_len: usize,
}

impl RegionAddressWindow {
    pub(crate) fn end(self) -> Result<usize> {
        self.base
            .checked_add(self.reserved_len)
            .context("region address window end overflow")
    }

    pub(crate) fn contains(self, addr: usize) -> Result<bool> {
        Ok(self.base <= addr && addr < self.end()?)
    }

    fn validate(self) -> Result<()> {
        ensure!(self.reserved_len > 0, "region reserved length must be nonzero");
        ensure!(
            self.mapped_len <= self.reserved_len,
            "region mapped length exceeds reserved length"
        );
        let _ = self.end()?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RegionDescriptor {
    pub(crate) id: RegionId,
    pub(crate) backend: RegionBackendKind,
    pub(crate) path: PathBuf,
    pub(crate) window: RegionAddressWindow,
    pub(crate) numa_node: Option<i32>,
    pub(crate) cpu_set: Option<Vec<usize>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RegionSetDescriptor {
    regions: Vec<RegionDescriptor>,
    backend_kind: RegionBackendKind,
}

impl RegionSetDescriptor {
    pub(crate) fn new(mut regions: Vec<RegionDescriptor>) -> Result<Self> {
        ensure!(!regions.is_empty(), "region set requires at least one region");
        regions.sort_by_key(|region| (region.window.base, region.id));
        let backend_kind = regions[0].backend;
        let mut ids = BTreeSet::new();
        for region in &regions {
            ensure!(
                region.backend == backend_kind,
                "all regions in one region set must use the same backend kind"
            );
            ensure!(ids.insert(region.id), "duplicate region id");
            region.window.validate()?;
        }
        for pair in regions.windows(2) {
            let left = &pair[0];
            let right = &pair[1];
            ensure!(
                left.window.end()? <= right.window.base,
                "region address windows overlap"
            );
        }
        Ok(Self {
            regions,
            backend_kind,
        })
    }

    pub(crate) fn regions(&self) -> &[RegionDescriptor] {
        &self.regions
    }

    pub(crate) fn backend_kind(&self) -> RegionBackendKind {
        self.backend_kind
    }

    pub(crate) fn region_for_management_addr(&self, addr: usize) -> Option<&RegionDescriptor> {
        self.regions
            .iter()
            .find(|region| region.window.contains(addr).unwrap_or(false))
    }
}
```

- [ ] **Step 4: Run tests**

Run:

```sh
cargo test -p wasmtime region_set_ --lib
```

Expected: pass.

- [ ] **Step 5: Commit**

```sh
git add crates/wasmtime/src/runtime/vm/memory/tmemory.rs \
        crates/wasmtime/src/runtime/vm/memory/tmemory/region.rs
git commit -m "feat: add transactional memory region descriptors"
```

## Task 2: Fixed Virtual Address Window Mapping

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Test: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`

- [ ] **Step 1: Write failing fixed-window mapping tests**

Add Linux-only tests near the existing mapping tests:

```rust
#[cfg(all(test, target_os = "linux"))]
fn reserve_test_window(len: usize) -> usize {
    unsafe {
        let ptr = libc::mmap(
            core::ptr::null_mut(),
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        assert_ne!(ptr, libc::MAP_FAILED);
        libc::munmap(ptr, len);
        ptr as usize
    }
}

#[test]
#[cfg(target_os = "linux")]
fn file_backed_mapping_uses_requested_fixed_window() {
    let len = 4096;
    let base = reserve_test_window(len);
    let path = unique_temp_file_path();
    let mut mapping = FileBackedMapping::new_path_in_window(
        path.clone(),
        len,
        MappedAddressWindow {
            base,
            reserved_len: len,
        },
    )
    .unwrap();
    assert_eq!(mapping.base_addr_for_test(), Some(base));
    mapping.write(0, b"fixed").unwrap();
    mapping.flush(0, 5).unwrap();
    mapping.fence_all().unwrap();
    drop(mapping);
    let _ = std::fs::remove_file(path);
}

#[test]
#[cfg(target_os = "linux")]
fn file_backed_mapping_grow_keeps_fixed_base() {
    let initial_len = 4096;
    let reserved_len = 8192;
    let base = reserve_test_window(reserved_len);
    let path = unique_temp_file_path();
    let mut mapping = FileBackedMapping::new_path_in_window(
        path.clone(),
        initial_len,
        MappedAddressWindow { base, reserved_len },
    )
    .unwrap();
    assert_eq!(mapping.base_addr_for_test(), Some(base));
    mapping.remap_len(8192).unwrap();
    assert_eq!(mapping.base_addr_for_test(), Some(base));
    mapping.write(4096, b"grow").unwrap();
    mapping.flush(4096, 4).unwrap();
    drop(mapping);
    let _ = std::fs::remove_file(path);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```sh
cargo test -p wasmtime file_backed_mapping_uses_requested_fixed_window --lib
cargo test -p wasmtime file_backed_mapping_grow_keeps_fixed_base --lib
```

Expected: fail because `MappedAddressWindow`, `new_path_in_window`, and `base_addr_for_test` do not exist.

- [ ] **Step 3: Add fixed-window mapping API**

Add this type near `FileBackedRegionMode`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MappedAddressWindow {
    pub(crate) base: usize,
    pub(crate) reserved_len: usize,
}
```

Extend `FileBackedMappingInner`:

```rust
struct FileBackedMappingInner {
    file: File,
    path: PathBuf,
    unlink_on_drop: AtomicBool,
    memory: RwLock<SendSyncPtr<[u8]>>,
    fixed_window: Option<MappedAddressWindow>,
}
```

Set `fixed_window: None` in all current non-fixed constructors.

Add constructors:

```rust
impl FileBackedMapping {
    #[cfg(target_os = "linux")]
    pub(crate) fn new_path_in_window(
        path: PathBuf,
        len: usize,
        window: MappedAddressWindow,
    ) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("failed to open file-backed tmemory file {}", path.display()))?;
        finish_file_backed_mapping_in_window(file, path, len, false, window)
    }

    #[cfg(all(test, target_os = "linux"))]
    fn base_addr_for_test(&self) -> Option<usize> {
        self.inner
            .fixed_window
            .map(|window| window.base)
    }
}
```

- [ ] **Step 4: Implement Linux fixed mapping primitives**

Use `libc::MAP_FIXED_NOREPLACE` only to reserve the whole window. Use `libc::MAP_FIXED` to replace the runtime-owned `PROT_NONE` reservation with the file mapping.

```rust
#[cfg(target_os = "linux")]
unsafe fn reserve_fixed_window(window: MappedAddressWindow) -> Result<()> {
    ensure!(window.reserved_len > 0, "fixed mapping reserved length must be nonzero");
    let ptr = unsafe {
        libc::mmap(
            window.base as *mut libc::c_void,
            window.reserved_len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
            -1,
            0,
        )
    };
    ensure!(
        ptr != libc::MAP_FAILED,
        "failed to reserve fixed file-backed mapping window at {:#x}",
        window.base
    );
    Ok(())
}

#[cfg(target_os = "linux")]
unsafe fn map_shared_file_fixed(
    file: &File,
    len: usize,
    window: MappedAddressWindow,
) -> Result<SendSyncPtr<[u8]>> {
    ensure!(
        len <= window.reserved_len,
        "fixed file mapping length exceeds reserved window"
    );
    if len == 0 {
        return Ok(empty_mapping_memory());
    }
    let ptr = unsafe {
        libc::mmap(
            window.base as *mut libc::c_void,
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_FIXED,
            std::os::fd::AsRawFd::as_raw_fd(file),
            0,
        )
    };
    ensure!(
        ptr != libc::MAP_FAILED,
        "failed to mmap file into fixed window at {:#x}",
        window.base
    );
    let slice = core::ptr::slice_from_raw_parts_mut(ptr.cast::<u8>(), len);
    Ok(SendSyncPtr::new(NonNull::new(slice).unwrap()))
}
```

`finish_file_backed_mapping_in_window` should:

1. resize the file to `len`
2. reserve the full fixed window
3. map the file at `window.base`
4. store `fixed_window: Some(window)`
5. clean up the created file and unmap the reserved window on error

`FileBackedMapping::remap_len` should branch only on whether `fixed_window` is set. For fixed mappings, resize the file and call `map_shared_file_fixed` again with the same base. For non-fixed mappings, keep the current remap behavior.

- [ ] **Step 5: Run mapping tests**

Run:

```sh
cargo test -p wasmtime file_backed_mapping_uses_requested_fixed_window --lib
cargo test -p wasmtime file_backed_mapping_grow_keeps_fixed_base --lib
```

Expected: pass on Linux.

- [ ] **Step 6: Commit**

```sh
git add crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
git commit -m "feat: support fixed address storage mappings"
```

## Task 3: Multi-Region Backend Configuration

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/config.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/region.rs`
- Test: `crates/wasmtime/src/runtime/transaction/config.rs`

- [ ] **Step 1: Write failing config tests**

Add tests in `config.rs`:

```rust
#[test]
fn dax_pmem_multi_region_config_requires_dax_backend() {
    let regions = vec![
        TMemoryRegionConfig::new_for_test("/pmem0/wasmtime-a", 0x4000_0000_0000, 1 << 30),
        TMemoryRegionConfig::new_for_test("/pmem1/wasmtime-b", 0x5000_0000_0000, 1 << 30),
    ];
    let config = TransactionConfig::with_dax_pmem_fsdax_regions(regions.clone()).unwrap();
    assert_eq!(config.tmemory_backend(), TMemoryBackend::DaxPmem);
    assert_eq!(
        config.tmemory_dax_pmem_backing(),
        Some(TMemoryDaxPmemBacking::FsDaxRegions(regions))
    );
}

#[test]
fn file_backed_multi_region_config_requires_file_backend() {
    let regions = vec![
        TMemoryRegionConfig::new_for_test("/tmp/wasmtime-a", 0x6000_0000_0000, 1 << 30),
        TMemoryRegionConfig::new_for_test("/tmp/wasmtime-b", 0x7000_0000_0000, 1 << 30),
    ];
    let config = TransactionConfig::with_file_backed_tmemory_regions(regions.clone()).unwrap();
    assert_eq!(config.tmemory_backend(), TMemoryBackend::FileBackedMemory);
    assert_eq!(
        config.tmemory_file_backing(),
        Some(TMemoryFileBacking::Regions(regions))
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```sh
cargo test -p wasmtime multi_region_config --lib
```

Expected: fail because the multi-region config variants do not exist.

- [ ] **Step 3: Add config types and constructors**

Add a `TMemoryRegionConfig` public-to-runtime shape in `config.rs`:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TMemoryRegionConfig {
    pub(crate) path: PathBuf,
    pub(crate) address_base: usize,
    pub(crate) reserved_len: usize,
    pub(crate) numa_node: Option<i32>,
}

impl TMemoryRegionConfig {
    #[cfg(test)]
    pub(crate) fn new_for_test(
        path: impl Into<PathBuf>,
        address_base: usize,
        reserved_len: usize,
    ) -> Self {
        Self {
            path: path.into(),
            address_base,
            reserved_len,
            numa_node: None,
        }
    }
}
```

Extend the backing enums:

```rust
pub(crate) enum TMemoryDaxPmemBacking {
    ResearchTemp,
    FsDaxPath(PathBuf),
    ExistingFsDaxPath(PathBuf),
    FsDaxRegions(Vec<TMemoryRegionConfig>),
    ExistingFsDaxRegions(Vec<TMemoryRegionConfig>),
}

pub(crate) enum TMemoryFileBacking {
    Temp,
    Path(PathBuf),
    ExistingPath(PathBuf),
    Regions(Vec<TMemoryRegionConfig>),
    ExistingRegions(Vec<TMemoryRegionConfig>),
}
```

Add constructors on `TransactionConfig`:

```rust
pub(crate) fn with_dax_pmem_fsdax_regions(regions: Vec<TMemoryRegionConfig>) -> Result<Self> {
    let mut config = Self::default();
    config.set_dax_pmem_backing(TMemoryDaxPmemBacking::FsDaxRegions(regions))?;
    Ok(config)
}

pub(crate) fn with_file_backed_tmemory_regions(regions: Vec<TMemoryRegionConfig>) -> Result<Self> {
    let mut config = Self::default();
    config.set_file_backing(TMemoryFileBacking::Regions(regions))?;
    Ok(config)
}
```

Update `uses_durable_storage`, `opens_existing_durable_storage`, and backing validation match arms so multi-region DAX/file modes behave like their single-region variants.

- [ ] **Step 4: Run config tests**

Run:

```sh
cargo test -p wasmtime multi_region_config --lib
```

Expected: pass.

- [ ] **Step 5: Commit**

```sh
git add crates/wasmtime/src/runtime/transaction/config.rs \
        crates/wasmtime/src/runtime/vm/memory/tmemory/region.rs
git commit -m "feat: add multi-region transaction storage config"
```

## Task 4: Multi-Region Durable Log Backend

**Files:**
- Create: `crates/wasmtime/src/runtime/transaction/persist_region_set.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mod.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Test: `crates/wasmtime/src/runtime/transaction/persist_region_set.rs`

- [ ] **Step 1: Write failing multi-region durable-log tests**

Create `persist_region_set.rs` with tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::transaction::persist::{
        PendingPublication, StreamPublisher, TxDurableLog,
    };
    use crate::runtime::transaction::type_layout::TypeLayoutId;
    use crate::runtime::transaction::{ObjectPayload, ObjectValue};
    use crate::runtime::vm::PackedGranuleDomain;

    #[test]
    fn multi_region_durable_log_routes_transaction_to_owned_region() {
        let backend = MultiRegionDurableLogBackend::new_for_test(2, 32, RegionSelection::Pinned(1))
            .unwrap();
        let mut log = TxDurableLog::with_backend(backend);
        let layout = test_struct_type_layout_for_multi_region();
        log.ensure_type_layout(&layout).unwrap();
        let record = crate::runtime::transaction::encode_object_record_for_test(
            41,
            1,
            layout.id().as_u32(),
            &ObjectPayload::Struct(vec![ObjectValue::I32(7)]),
        )
        .unwrap();
        let publication = PendingPublication::persistent_object_for_test(
            PackedGranuleDomain::TStruct,
            41,
            1,
            layout.id().as_u32(),
            &record,
        );

        let marker = {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            publisher
                .publish_object_publication_before_commit(&publication)
                .unwrap()
        };
        {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            publisher.publish_commit_lp(marker).unwrap();
        }

        assert!(log.log_entries_for_region_for_test(0, 12).is_empty());
        assert_eq!(log.log_entries_for_region_for_test(1, 12).len(), 1);
    }

    #[test]
    fn multi_region_durable_log_exposes_region_count_for_tests() {
        let backend = MultiRegionDurableLogBackend::new_for_test(2, 32, RegionSelection::Pinned(0))
            .unwrap();
        let mut log = TxDurableLog::with_backend(backend);
        log.ensure_type_layout(&test_struct_type_layout_for_multi_region()).unwrap();
        assert_eq!(log.region_count_for_test(), 2);
    }
}
```

The helper `test_struct_type_layout_for_multi_region` should mirror the existing test layout helper in `persist.rs` and use a fixed nonzero `TypeLayoutId`.

- [ ] **Step 2: Run tests to verify they fail**

Run:

```sh
cargo test -p wasmtime multi_region_durable_log_ --lib
```

Expected: fail because the module and backend do not exist.

- [ ] **Step 3: Expose single-region log building blocks**

In `persist.rs`:

1. Change `trait DurableRegionStorage` to `pub(super) trait DurableRegionStorage`.
2. Change `struct DurableRegionLog<R>` to `pub(super) struct DurableRegionLog<R>`.
3. Add:

```rust
impl<R> DurableRegionLog<R>
where
    R: DurableRegionStorage,
{
    pub(super) fn new(region: R, shared_allocator_lock: Option<Arc<Mutex<()>>>) -> Self {
        Self {
            region,
            streams: BTreeMap::new(),
            pending_data_chunks: BTreeSet::new(),
            pending_log_blocks: BTreeSet::new(),
            shared_allocator_lock,
        }
    }
}
```

4. Update current constructors to call `DurableRegionLog::new(...)`.
5. Add test-only forwarding methods on `TxDurableLog` for region-count and per-region entries after the multi-region backend is added.

- [ ] **Step 4: Implement `MultiRegionDurableLogBackend`**

Register `mod persist_region_set;` in `transaction/mod.rs`.

Implement this initial shape:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RegionSelection {
    Pinned(usize),
    ThreadOwned,
}

#[derive(Debug)]
pub(super) struct MultiRegionDurableLogBackend<R>
where
    R: super::persist::DurableRegionStorage,
{
    regions: Vec<super::persist::DurableRegionLog<R>>,
    selection: RegionSelection,
}
```

For v1, `RegionSelection::Pinned` is enough for tests. `ThreadOwned` is wired in Task 6.

Backend rules:

- `ensure_type_layout` writes the layout to every region.
- `has_type_layout` returns true only if every region has the layout.
- `append_data_record`, `append_log_entry`, and `mark_last_log_entry_committed` use the selected owned region.
- `flush_data`, `flush_log`, and `fence` call every region; each `DurableRegionLog` already tracks its own pending work.
- `retire_committed_linear_undo_chunk` uses the selected owned region for live commit cleanup.
- test helpers expose region count and per-region log entries.
- test helpers can switch `RegionSelection::Pinned(index)` between publications so tests can create object versions in different regions deterministically.

- [ ] **Step 5: Add test constructor**

In `persist_region_set.rs`, add:

```rust
#[cfg(test)]
impl MultiRegionDurableLogBackend<crate::runtime::vm::block_region::DaxPmemBlockRegion> {
    pub(super) fn new_for_test(
        region_count: usize,
        payload_blocks: usize,
        selection: RegionSelection,
    ) -> Result<Self> {
        ensure!(region_count > 0, "test multi-region durable log requires regions");
        let mut regions = Vec::new();
        for _ in 0..region_count {
            let region = crate::runtime::vm::block_region::DaxPmemBlockRegion::new_for_test(
                payload_blocks,
            )?;
            regions.push(super::persist::DurableRegionLog::new(region, None));
        }
        Ok(Self { regions, selection })
    }
}
```

- [ ] **Step 6: Run durable-log tests**

Run:

```sh
cargo test -p wasmtime multi_region_durable_log_ --lib
```

Expected: pass.

- [ ] **Step 7: Commit**

```sh
git add crates/wasmtime/src/runtime/transaction/mod.rs \
        crates/wasmtime/src/runtime/transaction/persist.rs \
        crates/wasmtime/src/runtime/transaction/persist_region_set.rs
git commit -m "feat: add multi-region durable log backend"
```

## Task 5: Multi-Region Recovery Merge

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist_region_set.rs`
- Test: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Test: `crates/wasmtime/src/runtime/transaction/persist_region_set.rs`

- [ ] **Step 1: Write failing merge tests**

In `recovery.rs`, add tests that construct two recovered regions with competing object winners:

```rust
#[test]
fn merged_recovered_regions_select_latest_object_version() {
    let first = recovered_region_for_merge_test(41, 1, 10, 0);
    let second = recovered_region_for_merge_test(41, 2, 11, 0);
    let merged = RecoveredRegion::merge_region_set_for_test(vec![first, second]).unwrap();
    let winners = merged.committed_object_winners().unwrap();
    assert_eq!(winners.len(), 1);
    assert_eq!(winners[0].object_id, 41);
    assert_eq!(winners[0].version, 2);
}

#[test]
fn merged_recovered_regions_reject_type_layout_conflicts() {
    let first = recovered_region_with_layout_fingerprint_for_test(1, 0xaaaa);
    let second = recovered_region_with_layout_fingerprint_for_test(1, 0xbbbb);
    let err = RecoveredRegion::merge_region_set_for_test(vec![first, second]).unwrap_err();
    assert!(err.to_string().contains("type layout conflict"));
}
```

Use existing test helpers in `recovery.rs` as models for constructing `RecoveredRegion` values.

- [ ] **Step 2: Run tests to verify they fail**

Run:

```sh
cargo test -p wasmtime merged_recovered_regions_ --lib
```

Expected: fail because merge helpers do not exist.

- [ ] **Step 3: Add composite mapped source**

In `recovery.rs`, add:

```rust
#[derive(Debug)]
pub(crate) struct CompositeMappedRegionSource {
    regions: Vec<CompositeMappedRegion>,
}

#[derive(Debug)]
struct CompositeMappedRegion {
    offset_base: usize,
    len: usize,
    source: Arc<dyn MappedRegionSource>,
}

impl MappedRegionSource for CompositeMappedRegionSource {
    fn with_mapped_slice(
        &self,
        offset: usize,
        len: usize,
        f: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        let end = offset
            .checked_add(len)
            .context("composite recovered source range overflow")?;
        for region in &self.regions {
            let region_end = region
                .offset_base
                .checked_add(region.len)
                .context("composite recovered source region range overflow")?;
            if region.offset_base <= offset && end <= region_end {
                return region.source.with_mapped_slice(offset - region.offset_base, len, f);
            }
        }
        bail!("composite recovered source range is outside all regions")
    }
}
```

This is recovery-only routing. It is not used on normal object or linear-memory access.

- [ ] **Step 4: Implement region merge**

Add:

```rust
pub(crate) struct RecoveredRegionInput {
    pub(crate) recovered: RecoveredRegion,
    pub(crate) mapped_source: Arc<dyn MappedRegionSource>,
    pub(crate) mapped_len: usize,
}
```

Add `RecoveredRegion::merge_region_set(inputs: Vec<RecoveredRegionInput>) -> Result<RecoveredRegion>`.

Merge rules:

- Assign each input a virtual block base: cumulative `mapped_len / BLOCK_SIZE`.
- Adjust `RecoveryWinner.data_block`, `RecoveredObjectWinner.data_block`, `RecoveredObjectWinner.data_record_offset`, and `RecoveredTMemoryUndoRollback.data_block` by the assigned region base.
- Merge object winners by `object_id`; keep the higher version; reject equal-version duplicates with different locations.
- Merge root and tmemory-size winners using the same latest-version rules already used by single-region recovery.
- Merge type-layout registries; reject same layout id with different fingerprint/body.
- Build a `CompositeMappedRegionSource` and store it in the merged `RecoveredRegion`.

- [ ] **Step 5: Wire multi-region backend recovery**

In `persist_region_set.rs`, implement `TxDurableLogBackend::with_recovered_region_snapshot` for `MultiRegionDurableLogBackend<R>`:

1. recover every region
2. collect `RecoveredRegionInput`
3. merge through `RecoveredRegion::merge_region_set`
4. call the callback once with the merged region and composite mapped source

Keep `recover_region_snapshot` returning the merged region without copying record bytes.

- [ ] **Step 6: Run merge tests**

Run:

```sh
cargo test -p wasmtime merged_recovered_regions_ --lib
cargo test -p wasmtime multi_region_durable_log_ --lib
```

Expected: pass.

- [ ] **Step 7: Commit**

```sh
git add crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs \
        crates/wasmtime/src/runtime/transaction/persist_region_set.rs
git commit -m "feat: merge recovered transactional memory regions"
```

## Task 6: NUMA Thread Ownership And Parallel Region Recovery

**Files:**
- Create: `crates/wasmtime/src/runtime/vm/memory/tmemory/numa.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist_region_set.rs`
- Test: `crates/wasmtime/src/runtime/vm/memory/tmemory/numa.rs`
- Test: `crates/wasmtime/src/runtime/transaction/persist_region_set.rs`

- [ ] **Step 1: Write failing NUMA parser tests**

Add tests in `numa.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cpu_list_accepts_ranges_and_singletons() {
        assert_eq!(parse_cpu_list("0-3,8,10-11").unwrap(), vec![0, 1, 2, 3, 8, 10, 11]);
    }

    #[test]
    fn parse_cpu_list_rejects_reversed_range() {
        let err = parse_cpu_list("4-2").unwrap_err();
        assert!(err.to_string().contains("invalid CPU range"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```sh
cargo test -p wasmtime parse_cpu_list_ --lib
```

Expected: fail because `tmemory::numa` does not exist.

- [ ] **Step 3: Implement NUMA helpers**

Add `pub(crate) mod numa;` in `tmemory.rs`.

Implement:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CpuSet {
    pub(crate) cpus: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NumaNode {
    pub(crate) id: i32,
    pub(crate) cpus: CpuSet,
}

pub(crate) fn parse_cpu_list(input: &str) -> Result<Vec<usize>> {
    let mut out = Vec::new();
    for part in input.split(',').map(str::trim).filter(|part| !part.is_empty()) {
        if let Some((start, end)) = part.split_once('-') {
            let start = start.parse::<usize>().context("invalid CPU range start")?;
            let end = end.parse::<usize>().context("invalid CPU range end")?;
            ensure!(start <= end, "invalid CPU range");
            out.extend(start..=end);
        } else {
            out.push(part.parse::<usize>().context("invalid CPU id")?);
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}
```

Add Linux-only:

- `current_cpu() -> Result<usize>` using `libc::sched_getcpu`
- `node_for_cpu(cpu: usize) -> Result<Option<i32>>` by scanning `/sys/devices/system/node/node*/cpulist`
- `pin_current_thread(cpus: &[usize]) -> Result<()>` using `libc::sched_setaffinity`

On non-Linux, these functions should return clear unsupported errors.

- [ ] **Step 4: Add thread-owned region selection**

In `persist_region_set.rs`, add a thread-local owner:

```rust
thread_local! {
    static THREAD_REGION_OWNER: std::cell::Cell<Option<usize>> = const {
        std::cell::Cell::new(None)
    };
}
```

For `RegionSelection::ThreadOwned`, selection order is:

1. return the thread-local region if already set
2. read current CPU and NUMA node
3. choose the region whose descriptor has that NUMA node
4. set the thread-local region
5. pin current thread to that region CPU set when the descriptor provides one

This is per-thread ownership, not per-stream or per-object bookkeeping.

- [ ] **Step 5: Implement top-level region-parallel recovery**

In `MultiRegionDurableLogBackend<R>::with_recovered_region_snapshot`:

- spawn one scoped thread per region
- pin each top-level recovery thread to the region CPU set when available
- call region-local recovery with `RecoveryOptions { parallelism: RecoveryParallelism::Workers(8) }`
- collect per-region timing and worker count for test/benchmark reporting
- merge recovered regions after all top-level recovery tasks complete

The two-region DAX target must report:

```text
region 0 workers: 16
region 1 workers: 16
total workers: 32
```

- [ ] **Step 6: Run tests**

Run:

```sh
cargo test -p wasmtime parse_cpu_list_ --lib
cargo test -p wasmtime multi_region_parallel_recovery_ --lib
```

Expected: pass.

- [ ] **Step 7: Commit**

```sh
git add crates/wasmtime/src/runtime/vm/memory/tmemory.rs \
        crates/wasmtime/src/runtime/vm/memory/tmemory/numa.rs \
        crates/wasmtime/src/runtime/transaction/persist_region_set.rs
git commit -m "feat: recover transactional regions in parallel"
```

## Task 7: TMemory And Backend Constructors

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Test: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`

- [ ] **Step 1: Write failing constructor tests**

Add tests in `tmemory.rs`:

```rust
#[test]
fn dax_pmem_memory_accepts_multi_region_backing() {
    let regions = vec![
        TMemoryRegionConfig::new_for_test("/tmp/wasmtime-dax-a", 0x4100_0000_0000, 1 << 30),
        TMemoryRegionConfig::new_for_test("/tmp/wasmtime-dax-b", 0x4200_0000_0000, 1 << 30),
    ];
    let config = TransactionConfig::with_dax_pmem_fsdax_regions(regions).unwrap();
    assert_eq!(config.tmemory_backend(), TMemoryBackend::DaxPmem);
}

#[test]
fn file_backed_memory_accepts_multi_region_backing() {
    let regions = vec![
        TMemoryRegionConfig::new_for_test("/tmp/wasmtime-file-a", 0x4300_0000_0000, 1 << 30),
        TMemoryRegionConfig::new_for_test("/tmp/wasmtime-file-b", 0x4400_0000_0000, 1 << 30),
    ];
    let config = TransactionConfig::with_file_backed_tmemory_regions(regions).unwrap();
    assert_eq!(config.tmemory_backend(), TMemoryBackend::FileBackedMemory);
}
```

- [ ] **Step 2: Run tests to verify current constructor gaps**

Run:

```sh
cargo test -p wasmtime multi_region_backing --lib
```

Expected: fail until config imports and constructors are wired through.

- [ ] **Step 3: Wire DAX multi-region constructors**

Add DAX constructors:

- `DaxPmemBlockRegion::create_fsdax_path_in_window`
- `DaxPmemBlockRegion::open_fsdax_path_in_window`
- `TMemoryRegion::new_dax_pmem_regions`
- `DaxPmemMemory::new_with_backing` match arms for `FsDaxRegions` and `ExistingFsDaxRegions`

Use the fixed-window mapping from Task 2. For normal single-region modes, keep the existing constructors unchanged.

- [ ] **Step 4: Wire file-backed multi-region constructors**

Add file-backed constructors:

- `FileBackedMemoryBlockRegion::create_path_in_window`
- `FileBackedMemoryBlockRegion::open_path_in_window`
- `TMemoryRegion::new_file_backed_regions`

Keep the same homogeneous RegionSet validation from Task 1.

- [ ] **Step 5: Add durable-log constructors**

In `TxDurableLog`, add:

```rust
pub(crate) fn create_dax_pmem_fsdax_regions_for_test(
    regions: &[TMemoryRegionConfig],
    payload_blocks_per_region: usize,
) -> Result<Self>
```

and the matching open-existing constructor. These constructors build `MultiRegionDurableLogBackend<DaxPmemBlockRegion>`.

Add file-backed equivalents for multi-file tests.

- [ ] **Step 6: Run constructor tests**

Run:

```sh
cargo test -p wasmtime multi_region_backing --lib
cargo test -p wasmtime multi_region_durable_log_ --lib
```

Expected: pass.

- [ ] **Step 7: Commit**

```sh
git add crates/wasmtime/src/runtime/vm/memory/tmemory.rs \
        crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs \
        crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs \
        crates/wasmtime/src/runtime/transaction/persist.rs
git commit -m "feat: wire multi-region storage constructors"
```

## Task 8: Region-Aware Persistent GC Routing

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/persist_region_set.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/object_table.rs`
- Test: `crates/wasmtime/src/runtime/transaction/persist_region_set.rs`

- [ ] **Step 1: Write failing GC retirement test**

Add a test that publishes two object versions in different regions, runs recovery, marks one unreachable, and asserts the backend retires the owning region's chunk:

```rust
#[test]
fn multi_region_gc_retires_chunks_in_owning_region() {
    let backend = MultiRegionDurableLogBackend::new_for_test(2, 64, RegionSelection::Pinned(0))
        .unwrap();
    let mut log = TxDurableLog::with_backend(backend);
    log.set_region_selection_for_test(0).unwrap();
    publish_test_object_in_region(&mut log, 0, 41, 1).unwrap();
    log.set_region_selection_for_test(1).unwrap();
    publish_test_object_in_region(&mut log, 1, 42, 1).unwrap();
    let recovered = log.recover_region_snapshot().unwrap().unwrap();
    let winners = recovered.committed_object_winners().unwrap();
    let unreachable = winners
        .iter()
        .filter(|winner| winner.object_id == 42)
        .map(crate::runtime::transaction::object_table::recovered_record_location_from_winner)
        .collect::<Vec<_>>();
    let retired = log.retire_whole_dead_object_chunks(&[], &unreachable).unwrap();
    assert!(!retired.is_empty());
    assert_eq!(log.retired_region_for_test(retired[0]), Some(1));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```sh
cargo test -p wasmtime multi_region_gc_retires_chunks_in_owning_region --lib
```

Expected: fail because virtual block decoding and region-local retirement are not implemented.

- [ ] **Step 3: Decode virtual recovered block ranges**

In `persist_region_set.rs`, store recovery merge block ranges:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RecoveredRegionBlockRange {
    region_index: usize,
    virtual_start_block: u32,
    virtual_end_block: u32,
}
```

Use these ranges to route:

- `object_data_chunk_start_for_block`
- `retire_whole_dead_object_chunks`

When forwarding to a region, subtract the virtual block base to recover the region-local block number.

- [ ] **Step 4: Keep persistent GC global and sweeping region-local**

No object graph logic should become region-specific. Only chunk lookup and retirement route to the owning region. If a copied object is published by GC, it follows the GC transaction's owned region like any other object COW publication.

- [ ] **Step 5: Run GC tests**

Run:

```sh
cargo test -p wasmtime multi_region_gc_retires_chunks_in_owning_region --lib
cargo test -p wasmtime transaction:: --lib
```

Expected: pass.

- [ ] **Step 6: Commit**

```sh
git add crates/wasmtime/src/runtime/transaction/persist_region_set.rs \
        crates/wasmtime/src/runtime/transaction/state.rs \
        crates/wasmtime/src/runtime/transaction/object_table.rs
git commit -m "feat: route persistent gc across memory regions"
```

## Task 9: Two-Region DAX Stress And Throughput Reporting

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Test: `crates/wasmtime/src/runtime/transaction/persist.rs`

- [ ] **Step 1: Write failing stress-format test**

Update the existing stress report tests to require region recovery worker reporting:

```rust
#[test]
fn dax_stress_line_reports_multi_region_recovery_workers() {
    let line = format_multi_region_dax_recovery_line_for_test(
        &[
            RegionRecoveryStatsForTest {
                root_index: 0,
                numa_node: Some(0),
                workers: 16,
                recovered_bytes: 117 * 1024 * 1024 * 1024,
                elapsed: std::time::Duration::from_millis(1800),
            },
            RegionRecoveryStatsForTest {
                root_index: 1,
                numa_node: Some(1),
                workers: 16,
                recovered_bytes: 117 * 1024 * 1024 * 1024,
                elapsed: std::time::Duration::from_millis(1900),
            },
        ],
        std::time::Duration::from_millis(1900),
    );
    assert!(line.contains("region_workers=[8,8]"));
    assert!(line.contains("total_workers=16"));
    assert!(line.contains("numa=[0,1]"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```sh
cargo test -p wasmtime dax_stress_line_reports_multi_region_recovery_workers --lib
```

Expected: fail because the multi-region stress report helper does not exist.

- [ ] **Step 3: Add multi-region stress mode**

Extend the ignored DAX stress harness:

- when `WASMTIME_DAX_STRESS_DIRS=/pmem0/...,/pmem1/...` has two or more roots, create one configured region per root
- populate each region with object and linear data
- recover all regions concurrently through the multi-region backend
- print per-region recovery throughput and total combined throughput
- report `region_workers=[8,8] total_workers=16` for the two-region Optane target

- [ ] **Step 4: Run local formatting test**

Run:

```sh
cargo test -p wasmtime dax_stress_line_reports_multi_region_recovery_workers --lib
```

Expected: pass.

- [ ] **Step 5: Run ignored Optane stress on 74**

Run from the local machine:

```sh
ssh shisoft@192.168.10.74 'cd /home/shisoft/Code/Research/wasmtime && \
  WASMTIME_DAX_STRESS_BYTES=256GiB \
  WASMTIME_DAX_STRESS_DIRS=/pmem0/wasmtime-dcpmm,/pmem1/wasmtime-dcpmm \
  cargo test -p wasmtime dax_pmem_fsdax_stress --release --lib -- --ignored --nocapture'
```

Expected:

- `/pmem0` reports 8 recovery workers
- `/pmem1` reports 8 recovery workers
- total recovery workers is 16
- total wall time trends toward the slower region time rather than the sum of both region times
- no recovered object sample validation failures

- [ ] **Step 6: Commit**

```sh
git add crates/wasmtime/src/runtime/transaction/persist.rs
git commit -m "test: add multi-region dax stress reporting"
```

## Task 10: Final Verification

**Files:**
- Modify: `docs/shisoft/2026-06-21-multi-region-memory-recovery-roadmap.md`
- Modify: `docs/shisoft/2026-06-21-multi-region-memory-recovery-implementation-plan.md`

- [ ] **Step 1: Run format and diff checks**

Run:

```sh
cargo fmt --check
git diff --check
```

Expected: both pass.

- [ ] **Step 2: Run focused tests**

Run:

```sh
cargo test -p wasmtime region_set_ --lib
cargo test -p wasmtime file_backed_mapping_uses_requested_fixed_window --lib
cargo test -p wasmtime file_backed_mapping_grow_keeps_fixed_base --lib
cargo test -p wasmtime multi_region_config --lib
cargo test -p wasmtime multi_region_durable_log_ --lib
cargo test -p wasmtime merged_recovered_regions_ --lib
cargo test -p wasmtime parse_cpu_list_ --lib
cargo test -p wasmtime multi_region_gc_retires_chunks_in_owning_region --lib
```

Expected: all pass.

- [ ] **Step 3: Run broader transaction tests**

Run:

```sh
cargo test -p wasmtime transaction:: --lib
cargo test -p wasmtime tmemory --lib
cargo check -p wasmtime
```

Expected: all pass, except pre-existing warnings unrelated to this work.

- [ ] **Step 4: Run Optane release benchmark**

Run:

```sh
ssh shisoft@192.168.10.74 'cd /home/shisoft/Code/Research/wasmtime && \
  WASMTIME_DAX_STRESS_BYTES=256GiB \
  WASMTIME_DAX_STRESS_DIRS=/pmem0/wasmtime-dcpmm,/pmem1/wasmtime-dcpmm \
  cargo test -p wasmtime dax_pmem_fsdax_stress --release --lib -- --ignored --nocapture'
```

Expected: report includes 8 workers per region and 16 total workers. Record the per-region and combined recovery throughput in the roadmap.

- [ ] **Step 5: Commit benchmark note**

```sh
git add docs/shisoft/2026-06-21-multi-region-memory-recovery-roadmap.md
git commit -m "docs: record multi-region recovery benchmark"
```

## Implementation Notes

- The fixed-window mapping work is Linux-first because DAX fsdax and NUMA pinning are Linux-specific in this research path.
- The existing non-fixed single-region constructors must remain valid to preserve current tests.
- Region identity is implied by the mapped file/backend being recovered. Durable log entries keep local block offsets.
- Composite mapped sources and virtual recovered block ranges are recovery/GC management structures only. Normal object and linear-memory access remains direct address access.
- The initial production placement policy is `ThreadOwned`: the first durable allocation on a thread binds that thread to a region. `Pinned` exists for deterministic tests.
- If one NUMA node owns multiple regions, recovery should share that node's 8-worker budget across those regions. The two-socket Optane target with one region per socket uses 8 workers per region.
