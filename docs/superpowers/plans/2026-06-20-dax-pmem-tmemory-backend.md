# DAX PMEM TMemory Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rename and reshape the current `NVMemory` transaction-memory backend into a DAX PMEM backend that can run on Intel DCPMM fsdax today and remain suitable for CXL persistent memory later.

**Architecture:** Use `DaxPmem` as the backend name, not `IntelDcpmm`, because the code depends on Linux DAX semantics rather than vendor-specific media. Extract the durable mapped-block-region logic into a reusable `MappedBlockRegion<M>` core, keep existing file-backed behavior on `FileBackedMapping`, and add `DaxPmemMapping` for fsdax files whose flush path is CLWB/SFENCE through `PersistEngine`. Normal tests use a research temp-file DAX-PMEM mode with no DAX requirement; real hardware tests require an fsdax path and `WASMTIME_TEST_REAL_PMEM=1`.

**Tech Stack:** Rust, Linux fsdax, XFS `dax=always`, `mmap`, `statx` DAX attribute validation through `libc`, x86_64 CLWB/SFENCE, existing `rustix` mmap/param APIs, `cargo test`.

---

## Requirements And Decisions

- Rename internal/backend-facing `NVMemory` terminology to `DaxPmem`.
- Do not use `IntelDcpmm` in core type names; reserve Intel DCPMM wording for docs/tests that describe the current hardware.
- Support fsdax files first. Raw `devdax` is out of scope for this implementation.
- Keep normal local tests independent of PMEM hardware.
- Use `/pmem0/wasmtime-dcpmm` and `/pmem1/wasmtime-dcpmm` only in ignored hardware tests.
- Do not add PMDK/libpmem in this pass.
- Validate real fsdax files on Linux with `statx` `STATX_ATTR_DAX` before allowing `RequireDaxPmem`.
- Keep the existing transaction durable-log and recovery model unchanged except where names and mapped backend plumbing require updates.

## File Structure

- Modify: `crates/wasmtime/src/runtime/transaction/config.rs`
  - Rename backend/config surface from `NVMemory` to `DaxPmem`.
  - Add fsdax backing configuration.
- Modify: `crates/wasmtime/src/runtime/transaction/mod.rs`
  - Re-export renamed config types.
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`
  - Rename config tests and add fsdax config validation tests.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
  - Rename `NVMemory` storage type to `DaxPmemMemory`.
  - Wire `DaxPmem` backend and backing configuration into `TMemory::new`.
  - Add ignored real-PMEM tests.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs`
  - Rename `new_nvmemory` to `new_dax_pmem`.
  - Construct the new `DaxPmemBlockRegion`.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
  - Extract `MappedBlockRegion<M>`.
  - Keep `FileBackedMemoryBlockRegion` as a type alias or concrete wrapper over the generic mapped core.
  - Rename `NVMemoryBlockRegion` to `DaxPmemBlockRegion` and make it use `DaxPmemMapping`.
  - Add Linux fsdax validation helpers.

## Task 1: Characterize Current Backend Behavior

**Files:**
- No source edits.

- [ ] **Step 1: Run the local tmemory backend baseline**

Run:

```bash
cargo test -p wasmtime --lib runtime::vm::memory::tmemory -- --format terse
```

Expected: all tmemory unit tests pass. Record the count in the commit message for the first code commit.

- [ ] **Step 2: Run the transaction config baseline**

Run:

```bash
cargo test -p wasmtime --lib transaction_config -- --format terse
```

Expected: all transaction config tests pass.

- [ ] **Step 3: Confirm real fsdax hardware is reachable**

Run:

```bash
ssh shisoft@192.168.10.74 'set -eu
grep -m1 -o clwb /proc/cpuinfo
findmnt -T /pmem0/wasmtime-dcpmm -o TARGET,SOURCE,FSTYPE,OPTIONS
findmnt -T /pmem1/wasmtime-dcpmm -o TARGET,SOURCE,FSTYPE,OPTIONS
python3 - <<'"'"'PY'"'"'
import mmap, os
for d in ["/pmem0/wasmtime-dcpmm", "/pmem1/wasmtime-dcpmm"]:
    path = os.path.join(d, "wasmtime-plan-smoke.bin")
    with open(path, "w+b") as f:
        f.truncate(4096)
        mm = mmap.mmap(f.fileno(), 4096, access=mmap.ACCESS_WRITE)
        mm[64:68] = b"dax!"
        mm.flush(0, 4096)
        mm.close()
        os.fsync(f.fileno())
    with open(path, "rb") as f:
        f.seek(64)
        assert f.read(4) == b"dax!"
    os.remove(path)
print("fsdax smoke ok")
PY'
```

Expected: `clwb`, both mounts show XFS with `dax=always`, and Python prints `fsdax smoke ok`.

## Task 2: Rename Config Surface To DAX PMEM

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/config.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mod.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Write failing config tests for the new names**

In `crates/wasmtime/src/runtime/transaction/tests.rs`, replace the old `nvmemory` config tests with:

```rust
#[test]
fn transaction_config_accepts_dax_pmem_backend() {
    let config = TransactionConfig::with_tmemory_backend(TMemoryBackend::DaxPmem).unwrap();

    assert_eq!(config.tmemory_backend(), TMemoryBackend::DaxPmem);
    assert_eq!(config.tmemory_dax_pmem_backing(), Some(TMemoryDaxPmemBacking::ResearchTemp));
    assert_eq!(
        config.tmemory_persistence_mode(),
        TMemoryPersistenceMode::ResearchPretendDaxPmem
    );
}

#[test]
fn transaction_config_accepts_dax_pmem_fsdax_path() {
    let path = std::path::PathBuf::from("/pmem0/wasmtime-dcpmm/test.tmemory");
    let config = TransactionConfig::with_dax_pmem_fsdax_path(path.clone()).unwrap();

    assert_eq!(config.tmemory_backend(), TMemoryBackend::DaxPmem);
    assert_eq!(
        config.tmemory_dax_pmem_backing(),
        Some(TMemoryDaxPmemBacking::FsDaxPath(path))
    );
    assert_eq!(
        config.tmemory_persistence_mode(),
        TMemoryPersistenceMode::RequireDaxPmem
    );
}

#[test]
fn transaction_config_rejects_empty_dax_pmem_fsdax_path() {
    let error = TransactionConfig::with_dax_pmem_fsdax_path(std::path::PathBuf::new())
        .unwrap_err()
        .to_string();

    assert!(error.contains("DAX PMEM fsdax path cannot be empty"), "{error}");
}
```

Run:

```bash
cargo test -p wasmtime --lib transaction_config_ -- --format terse
```

Expected: compile fails because the new enum variants and methods do not exist yet.

- [ ] **Step 2: Implement the renamed config API**

In `crates/wasmtime/src/runtime/transaction/config.rs`, add the new backend and persistence names while temporarily keeping old `NVMemory` names until Task 3 removes all old callers:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TMemoryBackend {
    VMemory,
    FileBackedMemory,
    DaxPmem,
    NVMemory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TMemoryPersistenceMode {
    ResearchPretendDaxPmem,
    RequireDaxPmem,
    ResearchPretendPmem,
    RequireHardwarePmem,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TMemoryDaxPmemBacking {
    ResearchTemp,
    FsDaxPath(PathBuf),
    ExistingFsDaxPath(PathBuf),
}
```

Add the field to `TransactionConfig`:

```rust
tmemory_dax_pmem_backing: Option<TMemoryDaxPmemBacking>,
```

Set defaults:

```rust
tmemory_persistence_mode: TMemoryPersistenceMode::ResearchPretendDaxPmem,
tmemory_file_backing: None,
tmemory_dax_pmem_backing: None,
```

Add accessors and constructors:

```rust
pub(crate) fn with_dax_pmem_fsdax_path(path: PathBuf) -> Result<Self> {
    ensure!(
        !path.as_os_str().is_empty(),
        "DAX PMEM fsdax path cannot be empty"
    );
    let mut config = Self::default();
    config.set_dax_pmem_backing(TMemoryDaxPmemBacking::FsDaxPath(path))?;
    Ok(config)
}

pub(crate) fn with_dax_pmem_existing_fsdax_path(path: PathBuf) -> Result<Self> {
    ensure!(
        !path.as_os_str().is_empty(),
        "DAX PMEM fsdax path cannot be empty"
    );
    let mut config = Self::default();
    config.set_dax_pmem_backing(TMemoryDaxPmemBacking::ExistingFsDaxPath(path))?;
    Ok(config)
}

pub(crate) fn tmemory_dax_pmem_backing(&self) -> Option<TMemoryDaxPmemBacking> {
    self.tmemory_dax_pmem_backing.clone()
}

fn set_dax_pmem_backing(&mut self, backing: TMemoryDaxPmemBacking) -> Result<()> {
    self.tmemory_backend = TMemoryBackend::DaxPmem;
    self.tmemory_persistence_mode = match backing {
        TMemoryDaxPmemBacking::ResearchTemp => TMemoryPersistenceMode::ResearchPretendDaxPmem,
        TMemoryDaxPmemBacking::FsDaxPath(_) | TMemoryDaxPmemBacking::ExistingFsDaxPath(_) => {
            TMemoryPersistenceMode::RequireDaxPmem
        }
    };
    self.tmemory_file_backing = None;
    self.tmemory_dax_pmem_backing = Some(backing);
    Ok(())
}
```

Update `set_tmemory_backend` so `DaxPmem` selects research temp mode:

```rust
TMemoryBackend::DaxPmem => {
    self.set_dax_pmem_backing(TMemoryDaxPmemBacking::ResearchTemp)
}
```

Keep `with_nvmemory_persistence_mode` unchanged until Task 3 migrates existing callers. Do not add new callers to it.

- [ ] **Step 3: Update re-exports**

In `crates/wasmtime/src/runtime/transaction/mod.rs`, export the new backing type:

```rust
pub(crate) use config::{
    TMemoryBackend, TMemoryDaxPmemBacking, TMemoryFileBacking, TMemoryPersistenceMode,
    TransactionConfig,
};
```

- [ ] **Step 4: Run config tests**

Run:

```bash
cargo test -p wasmtime --lib transaction_config -- --format terse
```

Expected: transaction config tests pass after updating old `NVMemory` assertions to `DaxPmem`.

- [ ] **Step 5: Commit config rename**

```bash
git add crates/wasmtime/src/runtime/transaction/config.rs \
        crates/wasmtime/src/runtime/transaction/mod.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Rename transaction memory backend to DAX PMEM"
```

## Task 3: Rename TMemory Storage Types And Tests

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`

- [ ] **Step 1: Write failing search check for old names**

Run:

```bash
rg -n "NVMemory|nvmemory|RequireHardwarePmem|ResearchPretendPmem" \
  crates/wasmtime/src/runtime/vm/memory/tmemory.rs \
  crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs \
  crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs \
  crates/wasmtime/src/runtime/transaction
```

Expected before edits: matches exist. After this task: no matches except migration comments intentionally retained in docs; source code should use `DaxPmem`.

- [ ] **Step 2: Rename storage types and constructors**

Apply these source-level renames:

```text
NVMemory -> DaxPmemMemory
NVMemoryBlockRegion -> DaxPmemBlockRegion
new_nvmemory -> new_dax_pmem
nvmemory_ test names -> dax_pmem_ test names
PersistenceMode::ResearchPretendPmem -> PersistenceMode::ResearchPretendDaxPmem
PersistenceMode::RequireHardwarePmem -> PersistenceMode::RequireDaxPmem
```

Update `TMemory::new_for_backend` in `tmemory.rs` to match:

```rust
TMemoryBackend::DaxPmem => {
    let backing = dax_pmem_backing.context("DaxPmem requires DAX PMEM backing configuration")?;
    Box::new(DaxPmemMemory::new_with_backing(
        min_pages,
        max_pages,
        persistence_mode,
        backing,
    )?)
}
```

This step will not compile until Task 4 adds the backing plumbing.

- [ ] **Step 3: Keep research constructor compatibility for tests**

In `TMemory::new_with_backend_limits`, pass `TMemoryDaxPmemBacking::ResearchTemp` for `DaxPmem`:

```rust
let dax_pmem_backing = (backend == TMemoryBackend::DaxPmem)
    .then_some(TMemoryDaxPmemBacking::ResearchTemp);
Self::new_for_backend(
    backend,
    TMemoryPersistenceMode::ResearchPretendDaxPmem,
    None,
    dax_pmem_backing,
    min_pages,
    max_pages,
)
```

- [ ] **Step 4: Run the old-name search**

Run:

```bash
rg -n "NVMemory|nvmemory|RequireHardwarePmem|ResearchPretendPmem" \
  crates/wasmtime/src/runtime/vm/memory/tmemory.rs \
  crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs \
  crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs \
  crates/wasmtime/src/runtime/transaction
```

Expected: no matches in Rust source.

## Task 4: Extract A Generic Mapped Block Region Core

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`

- [ ] **Step 1: Add the mapped-region trait**

In `block_region.rs`, define this trait near `FileBackedMapping`:

```rust
pub(crate) trait DurableMappedBytes: core::fmt::Debug {
    fn len(&self) -> usize;
    fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>>;
    fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()>;
    fn flush(&self, offset: usize, len: usize) -> Result<()>;
    fn fence(&self) -> Result<()>;
    fn remap_len(&mut self, new_len: usize) -> Result<()>;
}
```

Implement it for `FileBackedMapping`:

```rust
impl DurableMappedBytes for FileBackedMapping {
    fn len(&self) -> usize { self.len() }
    fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> { self.read(offset, len) }
    fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> { self.write(offset, bytes) }
    fn flush(&self, offset: usize, len: usize) -> Result<()> { self.flush(offset, len) }
    fn fence(&self) -> Result<()> { self.fence_data() }
    fn remap_len(&mut self, new_len: usize) -> Result<()> { self.remap_len(new_len) }
}
```

- [ ] **Step 2: Rename `FileBackedMemoryBlockRegion` internals into `MappedBlockRegion<M>`**

Replace:

```rust
pub(crate) struct FileBackedMemoryBlockRegion {
    mapping: FileBackedMapping,
    block_entries: Vec<BlockEntry>,
    block_metas: Vec<BlockMeta>,
    line_marks: Vec<LineMark>,
    streams: BTreeMap<u32, StreamState>,
}
```

With:

```rust
#[derive(Debug)]
pub(crate) struct MappedBlockRegion<M: DurableMappedBytes> {
    mapping: M,
    block_entries: Vec<BlockEntry>,
    block_metas: Vec<BlockMeta>,
    line_marks: Vec<LineMark>,
    streams: BTreeMap<u32, StreamState>,
}

pub(crate) type FileBackedMemoryBlockRegion = MappedBlockRegion<FileBackedMapping>;
```

Move methods that only depend on `mapping`, metadata, streams, and block layout into:

```rust
impl<M: DurableMappedBytes> MappedBlockRegion<M> {
    fn with_mapping(mapping: M, num_blocks: usize) -> Result<Self> {
        let bytes_len = num_blocks
            .checked_mul(BLOCK_SIZE)
            .context("transactional mapped block region size overflow")?;
        ensure!(
            mapping.len() >= bytes_len,
            "transactional mapped block region mapping is smaller than requested size"
        );
        let line_count = bytes_len / IMMIX_LINE_SIZE;
        Ok(Self {
            mapping,
            block_entries: vec![BlockEntry::default(); num_blocks],
            block_metas: vec![BlockMeta::free(); num_blocks],
            line_marks: vec![
                LineMark {
                    mark: IMMIX_LINE_MARK_RESET_VALUE,
                };
                line_count
            ],
            streams: BTreeMap::new(),
        })
    }
}
```

Keep file-backed constructors as concrete inherent impls:

```rust
impl MappedBlockRegion<FileBackedMapping> {
    pub(crate) fn new(num_blocks: usize, mode: FileBackedRegionMode) -> Result<Self> {
        let bytes_len = num_blocks
            .checked_mul(BLOCK_SIZE)
            .context("transactional FileBackedMemory block region size overflow")?;
        Self::with_mapping(FileBackedMapping::new(mode, bytes_len)?, num_blocks)
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(num_blocks: usize, mode: FileBackedRegionMode) -> Result<Self> {
        Self::new(num_blocks, mode)
    }
}
```

- [ ] **Step 3: Run file-backed block-region tests**

Run:

```bash
cargo test -p wasmtime --lib file_backed_ -- --format terse
```

Expected: all file-backed block-region and mapping tests pass. This proves the generic extraction did not change existing file-backed behavior.

- [ ] **Step 4: Commit the mapped core extraction**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
git commit -m "Extract mapped transaction block region core"
```

## Task 5: Add FSDAX Mapping And Validation

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`

- [ ] **Step 1: Write failing fsdax validation unit tests**

Add these tests to `block_region.rs`:

```rust
#[cfg(target_os = "linux")]
#[test]
fn dax_pmem_mapping_rejects_non_dax_path_in_real_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not-dax.tmemory");

    let error = DaxPmemMapping::new_fsdax_path(path, 4096)
        .unwrap_err()
        .to_string();

    assert!(
        error.contains("does not have the DAX inode attribute")
            || error.contains("failed to statx DAX PMEM fsdax path"),
        "{error}"
    );
}

#[test]
fn dax_pmem_mapping_research_temp_allows_non_dax_storage() {
    let mut mapping = DaxPmemMapping::new_research_temp(4096).unwrap();

    mapping.write(32, &[1, 2, 3, 4]).unwrap();
    mapping.flush(32, 4).unwrap();
    mapping.fence().unwrap();

    assert_eq!(mapping.read(32, 4).unwrap(), vec![1, 2, 3, 4]);
}
```

Run:

```bash
cargo test -p wasmtime --lib dax_pmem_mapping -- --format terse
```

Expected: compile fails because `DaxPmemMapping` does not exist.

- [ ] **Step 2: Add `DaxPmemMapping`**

Add this struct near `FileBackedMapping`:

```rust
#[derive(Debug)]
pub(crate) struct DaxPmemMapping {
    mapping: FileBackedMapping,
    persist: PersistEngine,
    require_fsdax: bool,
}
```

Add constructors:

```rust
impl DaxPmemMapping {
    pub(crate) fn new_research_temp(len: usize) -> Result<Self> {
        Ok(Self {
            mapping: FileBackedMapping::new_temp(len)?,
            persist: PersistEngine::for_mode(PersistenceMode::ResearchPretendDaxPmem)?,
            require_fsdax: false,
        })
    }

    pub(crate) fn new_fsdax_path(path: PathBuf, len: usize) -> Result<Self> {
        let mapping = FileBackedMapping::new_path(path.clone(), len)?;
        ensure_fsdax_path(&path)?;
        Ok(Self {
            mapping,
            persist: PersistEngine::for_mode(PersistenceMode::RequireDaxPmem)?,
            require_fsdax: true,
        })
    }

    pub(crate) fn open_existing_fsdax_path(path: PathBuf) -> Result<Self> {
        let mapping = FileBackedMapping::open_existing_path(path.clone())?;
        ensure_fsdax_path(&path)?;
        Ok(Self {
            mapping,
            persist: PersistEngine::for_mode(PersistenceMode::RequireDaxPmem)?,
            require_fsdax: true,
        })
    }
}
```

Implement `DurableMappedBytes`:

```rust
impl DurableMappedBytes for DaxPmemMapping {
    fn len(&self) -> usize { self.mapping.len() }
    fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> { self.mapping.read(offset, len) }
    fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> { self.mapping.write(offset, bytes) }
    fn flush(&self, offset: usize, len: usize) -> Result<()> {
        let ptr = self.mapping.ptr_at(offset)?;
        self.persist.flush(ptr, len)
    }
    fn fence(&self) -> Result<()> { self.persist.fence() }
    fn remap_len(&mut self, new_len: usize) -> Result<()> {
        self.mapping.remap_len(new_len)?;
        if self.require_fsdax {
            ensure_fsdax_path(self.mapping.path())?;
        }
        Ok(())
    }
}
```

Add these accessors to `FileBackedMapping`:

```rust
pub(crate) fn path(&self) -> &Path {
    &self.path
}

pub(crate) fn ptr_at(&self, offset: usize) -> Result<NonNull<u8>> {
    ensure!(offset <= self.len(), "file-backed mapping pointer offset out of bounds");
    if self.len() == 0 {
        return Ok(NonNull::dangling());
    }
    let ptr = unsafe {
        NonNull::new_unchecked(self.memory.as_non_null().cast::<u8>().as_ptr().add(offset))
    };
    Ok(ptr)
}
```

- [ ] **Step 3: Add Linux fsdax validation**

Add:

```rust
#[cfg(target_os = "linux")]
fn ensure_fsdax_path(path: &Path) -> Result<()> {
    ensure!(
        statx_has_dax_attr(path)?,
        "DAX PMEM fsdax path {} does not have the DAX inode attribute",
        path.display()
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn ensure_fsdax_path(path: &Path) -> Result<()> {
    bail!(
        "DAX PMEM fsdax path {} is unsupported on this target",
        path.display()
    )
}
```

Add Linux `statx` helper:

```rust
#[cfg(target_os = "linux")]
fn statx_has_dax_attr(path: &Path) -> Result<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    const STATX_ATTR_DAX_FALLBACK: u64 = 0x0020_0000;
    let c_path = CString::new(path.as_os_str().as_bytes())
        .context("DAX PMEM fsdax path contains interior NUL")?;
    let mut statx = core::mem::MaybeUninit::<libc::statx>::zeroed();
    let rc = unsafe {
        libc::statx(
            libc::AT_FDCWD,
            c_path.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_BASIC_STATS,
            statx.as_mut_ptr(),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to statx DAX PMEM fsdax path {}", path.display()));
    }
    let statx = unsafe { statx.assume_init() };
    let dax = libc::STATX_ATTR_DAX as u64;
    let dax = if dax == 0 { STATX_ATTR_DAX_FALLBACK } else { dax };
    Ok((statx.stx_attributes_mask & dax) != 0 && (statx.stx_attributes & dax) != 0)
}
```

If `libc::STATX_ATTR_DAX` is not available in this libc version, replace the `let dax = libc::STATX_ATTR_DAX as u64;` line with:

```rust
let dax = STATX_ATTR_DAX_FALLBACK;
```

- [ ] **Step 4: Run mapping tests**

Run:

```bash
cargo test -p wasmtime --lib dax_pmem_mapping -- --format terse
```

Expected: local tests pass. The real-mode non-DAX path test should fail by returning an error, which is the expected behavior.

## Task 6: Implement `DaxPmemBlockRegion`

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs`

- [ ] **Step 1: Add `DaxPmemBlockRegion` type alias and constructors**

In `block_region.rs`:

```rust
pub(crate) type DaxPmemBlockRegion = MappedBlockRegion<DaxPmemMapping>;

impl MappedBlockRegion<DaxPmemMapping> {
    pub(crate) fn new_research(num_blocks: usize) -> Result<Self> {
        let bytes_len = num_blocks
            .checked_mul(BLOCK_SIZE)
            .context("transactional DAX PMEM block region size overflow")?;
        Self::with_mapping(DaxPmemMapping::new_research_temp(bytes_len)?, num_blocks)
    }

    pub(crate) fn create_fsdax_path(path: PathBuf, num_blocks: usize) -> Result<Self> {
        let bytes_len = num_blocks
            .checked_mul(BLOCK_SIZE)
            .context("transactional DAX PMEM block region size overflow")?;
        let mut region = Self::with_mapping(DaxPmemMapping::new_fsdax_path(path, bytes_len)?, num_blocks)?;
        region.initialize_region_image()?;
        Ok(region)
    }

    pub(crate) fn open_fsdax_path(path: PathBuf) -> Result<Self> {
        let mapping = DaxPmemMapping::open_existing_fsdax_path(path)?;
        let header = read_region_header_from_mapping(&mapping)?;
        let num_blocks = usize::try_from(header.num_blocks)
            .context("DAX PMEM region block count overflow")?;
        let mut region = Self::with_mapping(mapping, num_blocks)?;
        region.load_region_image()?;
        Ok(region)
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(num_blocks: usize) -> Result<Self> {
        Self::new_research(num_blocks)
    }
}
```

If `read_region_header_from_mapping` does not exist after extraction, add a private helper that reads `size_of::<RegionHeader>()` bytes from offset `0` and calls `RegionHeader::from_bytes`.

- [ ] **Step 2: Update linear region construction**

In `linear_region.rs`, replace `new_nvmemory` with:

```rust
pub(super) fn new_dax_pmem(
    byte_capacity: usize,
    backing: TMemoryDaxPmemBacking,
) -> Result<Self> {
    let block_count = byte_capacity.div_ceil(BLOCK_SIZE);
    let mut backend = match backing {
        TMemoryDaxPmemBacking::ResearchTemp => DaxPmemBlockRegion::new_research(block_count)?,
        TMemoryDaxPmemBacking::FsDaxPath(path) => {
            DaxPmemBlockRegion::create_fsdax_path(path, block_count)?
        }
        TMemoryDaxPmemBacking::ExistingFsDaxPath(path) => {
            DaxPmemBlockRegion::open_fsdax_path(path)?
        }
    };
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

Import `TMemoryDaxPmemBacking` from transaction config in `linear_region.rs`.

- [ ] **Step 3: Run mapped linear-region tests**

Run:

```bash
cargo test -p wasmtime --lib mapped_linear_region -- --format terse
cargo test -p wasmtime --lib dax_pmem_region -- --format terse
```

Expected: local tests pass after renaming `nvmemory_region_*` tests to `dax_pmem_region_*`.

## Task 7: Wire `DaxPmemMemory` Through `TMemory`

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`

- [ ] **Step 1: Rename the storage type**

Rename:

```rust
pub(crate) struct NVMemory
```

To:

```rust
pub(crate) struct DaxPmemMemory {
    region: TMemoryRegion,
    backing: TMemoryDaxPmemBacking,
    granules: Vec<TMemoryGranuleInfo>,
    byte_len: usize,
    byte_capacity: usize,
    max_pages: u64,
}
```

Add constructor:

```rust
pub(crate) fn new_with_backing(
    min_pages: u64,
    max_pages: Option<u64>,
    persistence_mode: TMemoryPersistenceMode,
    backing: TMemoryDaxPmemBacking,
) -> Result<Self> {
    ensure!(
        matches!(
            (&persistence_mode, &backing),
            (TMemoryPersistenceMode::ResearchPretendDaxPmem, TMemoryDaxPmemBacking::ResearchTemp)
                | (TMemoryPersistenceMode::RequireDaxPmem, TMemoryDaxPmemBacking::FsDaxPath(_))
                | (TMemoryPersistenceMode::RequireDaxPmem, TMemoryDaxPmemBacking::ExistingFsDaxPath(_))
        ),
        "DAX PMEM persistence mode does not match backing"
    );
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
        region: TMemoryRegion::new_dax_pmem(byte_capacity, backing.clone())?,
        backing,
        granules: vec![TMemoryGranuleInfo::default(); granule_capacity],
        byte_len,
        byte_capacity,
        max_pages,
    })
}
```

- [ ] **Step 2: Update growth behavior**

In `reserve_capacity_to_pages`, keep research temp growth supported, but reject fsdax growth that requires moving metadata tables until the region can safely remap and preserve DAX validation:

```rust
match &self.backing {
    TMemoryDaxPmemBacking::ResearchTemp => {
        let mut region = TMemoryRegion::new_dax_pmem(new_byte_capacity, self.backing.clone())?;
        if self.byte_len > 0 {
            let old = self.region.read(0, self.byte_len)?;
            region.write(0, &old)?;
            region.flush(0, self.byte_len)?;
            region.fence()?;
        }
        self.region = region;
    }
    TMemoryDaxPmemBacking::FsDaxPath(_) | TMemoryDaxPmemBacking::ExistingFsDaxPath(_) => {
        self.region.grow_to_bytes(new_byte_capacity)?;
    }
}
```

If `TMemoryRegion::grow_to_bytes` does not exist, add it as a thin wrapper over `MappedLinearRegion` and `BlockRegionBackend::grow_to_blocks`. Keep the same metadata-relocation guard already used by `FileBackedMemoryBlockRegion`.

- [ ] **Step 3: Update trait implementation**

Rename the trait impl:

```rust
impl TMemoryBackendStorage for DaxPmemMemory {
    fn backend_kind(&self) -> TMemoryBackend {
        TMemoryBackend::DaxPmem
    }

    fn commit_range(&mut self, addr: usize, bytes: &[u8]) -> Result<()> {
        let end = addr.checked_add(bytes.len()).context("tmemory write address overflow")?;
        ensure!(end <= self.byte_len, "out of bounds tmemory access");
        self.region.write(addr, bytes)?;
        self.region.flush(addr, bytes.len())?;
        self.region.fence()
    }
}
```

Keep the existing read/grow/granule metadata methods with the renamed type.

- [ ] **Step 4: Run local DAX PMEM tmemory tests**

Run:

```bash
cargo test -p wasmtime --lib dax_pmem -- --format terse
cargo test -p wasmtime --lib runtime::vm::memory::tmemory -- --format terse
```

Expected: all local DAX PMEM research-mode and tmemory tests pass.

- [ ] **Step 5: Commit DAX PMEM backend wiring**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory.rs \
        crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs \
        crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
git commit -m "Wire DAX PMEM transaction memory backend"
```

## Task 8: Add Real FSDAX Hardware Tests

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`

- [ ] **Step 1: Add hardware-path helper**

In `tmemory.rs` test module:

```rust
fn real_pmem_test_path(file_name: &str) -> Option<std::path::PathBuf> {
    if std::env::var_os("WASMTIME_TEST_REAL_PMEM").is_none() {
        return None;
    }
    let root = std::env::var_os("WASMTIME_TEST_DAX_PMEM_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/pmem0/wasmtime-dcpmm"));
    Some(root.join(file_name))
}
```

- [ ] **Step 2: Replace the old ignored PMEM placeholder**

Replace `nvmemory_requires_real_pmem_for_restart_persistence` with:

```rust
#[test]
#[ignore = "requires real fsdax PMEM and WASMTIME_TEST_REAL_PMEM=1"]
fn dax_pmem_fsdax_commit_survives_reopen() {
    let Some(path) = real_pmem_test_path("dax-pmem-reopen.tmemory") else {
        eprintln!("set WASMTIME_TEST_REAL_PMEM=1 to run this test");
        return;
    };
    let _ = std::fs::remove_file(&path);

    {
        let config = TransactionConfig::with_dax_pmem_fsdax_path(path.clone()).unwrap();
        let mut memory = TMemory::new(config, 1, Some(1)).unwrap();
        memory.commit_staged_tmemory_granule(0, &[0x5a; TMEMORY_GRANULE_SIZE]).unwrap();
    }

    {
        let config = TransactionConfig::with_dax_pmem_existing_fsdax_path(path.clone()).unwrap();
        let memory = TMemory::new(config, 1, Some(1)).unwrap();
        assert_eq!(memory.read_committed(0..4).unwrap(), vec![0x5a; 4]);
    }

    let _ = std::fs::remove_file(path);
}
```

- [ ] **Step 3: Add real mapping validation test**

In `block_region.rs` test module:

```rust
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires real fsdax PMEM and WASMTIME_TEST_REAL_PMEM=1"]
fn dax_pmem_mapping_accepts_real_fsdax_path() {
    if std::env::var_os("WASMTIME_TEST_REAL_PMEM").is_none() {
        eprintln!("set WASMTIME_TEST_REAL_PMEM=1 to run this test");
        return;
    }
    let root = std::env::var_os("WASMTIME_TEST_DAX_PMEM_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/pmem0/wasmtime-dcpmm"));
    let path = root.join("dax-pmem-mapping.tmemory");
    let _ = std::fs::remove_file(&path);

    let mut mapping = DaxPmemMapping::new_fsdax_path(path.clone(), 4096).unwrap();
    mapping.write(128, b"dax!").unwrap();
    mapping.flush(128, 4).unwrap();
    mapping.fence().unwrap();
    drop(mapping);

    assert_eq!(&std::fs::read(&path).unwrap()[128..132], b"dax!");
    let _ = std::fs::remove_file(path);
}
```

- [ ] **Step 4: Run ignored hardware tests on the PMEM machine**

From the repository on the PMEM host, run:

```bash
WASMTIME_TEST_REAL_PMEM=1 \
WASMTIME_TEST_DAX_PMEM_DIR=/pmem0/wasmtime-dcpmm \
cargo test -p wasmtime --lib dax_pmem -- --ignored --format terse
```

Expected: ignored DAX PMEM tests pass on `/pmem0`.

Then run:

```bash
WASMTIME_TEST_REAL_PMEM=1 \
WASMTIME_TEST_DAX_PMEM_DIR=/pmem1/wasmtime-dcpmm \
cargo test -p wasmtime --lib dax_pmem -- --ignored --format terse
```

Expected: ignored DAX PMEM tests pass on `/pmem1`.

Run these hardware commands only from a checkout of this branch on the PMEM host. If the checkout is missing, stop this task and create/sync the checkout before running the tests. Do not use sudo for tests; the directories are already owned by `shisoft`.

## Task 9: Full Verification And Cleanup

**Files:**
- All files touched above.

- [ ] **Step 1: Run source-name cleanup**

Run:

```bash
rg -n "NVMemory|nvmemory|RequireHardwarePmem|ResearchPretendPmem" \
  crates/wasmtime/src/runtime/transaction \
  crates/wasmtime/src/runtime/vm/memory/tmemory.rs \
  crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs \
  crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
```

Expected: no matches.

- [ ] **Step 2: Run local transaction and tmemory suites**

Run:

```bash
cargo test -p wasmtime --lib runtime::vm::memory::tmemory -- --format terse
cargo test -p wasmtime --lib transaction:: -- --format terse
```

Expected: both suites pass.

- [ ] **Step 3: Run default formatting and whitespace checks**

Run:

```bash
cargo fmt --all -- --check
git diff --check
```

Expected: both commands exit successfully with no output.

- [ ] **Step 4: Run the transaction concurrency feature matrix**

Run:

```bash
features_base="anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser"
for feature in \
  transaction-cc-nowait-abort \
  transaction-cc-wound-wait \
  transaction-cc-wait-die \
  transaction-cc-strict-2pl \
  transaction-cc-optimistic-validation \
  transaction-cc-timestamp-ordering \
  transaction-cc-lockbased
do
  printf '\n== %s ==\n' "$feature"
  CARGO_INCREMENTAL=0 cargo test -p wasmtime --no-default-features \
    --features "${features_base},${feature}" \
    --lib transaction:: -- --format terse || exit 1
done
```

Expected: every selected concurrency-control build passes.

- [ ] **Step 5: Commit final cleanup**

```bash
git status --short
git add crates/wasmtime/src/runtime/transaction/config.rs \
        crates/wasmtime/src/runtime/transaction/mod.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs \
        crates/wasmtime/src/runtime/vm/memory/tmemory.rs \
        crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs \
        crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
git commit -m "Add fsdax DAX PMEM transaction memory backend"
```

Before committing, confirm `git status --short` does not stage unrelated `docs/shisoft` files.

## Self-Review Checklist

- Spec coverage: The plan covers rename away from `NVMemory`, fsdax-first DAX PMEM backing, no Intel-specific core type names, local research tests, and real hardware tests on `/pmem0` and `/pmem1`.
- Placeholder scan: The plan has no placeholder markers or unspecified implementation steps.
- Type consistency: The plan consistently uses `TMemoryBackend::DaxPmem`, `TMemoryDaxPmemBacking`, `DaxPmemMemory`, `DaxPmemBlockRegion`, and `DaxPmemMapping`.
- Scope check: Raw devdax and PMDK integration are explicitly out of scope for this implementation.
