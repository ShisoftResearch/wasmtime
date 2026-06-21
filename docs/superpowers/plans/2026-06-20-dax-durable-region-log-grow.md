# DAX Durable Region Log And Growth Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make DAX PMEM fsdax storage growable and route durable transaction/object/type-layout logging through one generic `DurableRegionLog<R>` instead of backend-specific durable log types.

**Architecture:** `TxDurableLog` remains the transaction protocol owner. A new `DurableRegionStorage` trait captures the block-region operations used by stream allocation, data/log publication, recovery, type-layout metadata, and object chunk retirement. `DurableRegionLog<R>` implements `TxDurableLogBackend` once for any region storage; `FileBackedMemoryBlockRegion` and `DaxPmemBlockRegion` implement the shared region trait.

**Tech Stack:** Rust, existing `TxDurableLogBackend`, `BlockRegionBackend`, fsdax-backed `DaxPmemMapping`, CLWB/SFENCE `PersistEngine`, cargo test.

---

### Task 1: Extract Generic Durable Region Log

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`

- [ ] **Step 1: Write failing compile/test target**

Run:

```bash
cargo test -p wasmtime --lib transaction::tests::durable_log_publishes_type_layout_before_object_records -- --format terse
```

Expected before implementation: pass. This is a guardrail that must remain green while refactoring from `FileBackedTxDurableLog` to `DurableRegionLog<R>`.

- [ ] **Step 2: Add `DurableRegionStorage` trait**

In `persist.rs`, add a trait whose method set matches the existing region calls made by `FileBackedTxDurableLog`:

```rust
trait DurableRegionStorage: core::fmt::Debug + Send + Sync {
    fn stream_cursor(&mut self, stream_id: u32) -> crate::runtime::vm::block_region::StreamCursor;
    fn refresh_from_image(&mut self) -> Result<()>;
    fn append_type_layout_metadata(&mut self, layout: &PersistentTypeLayout) -> Result<()>;
    fn load_type_layout_metadata(&self) -> Result<TypeLayoutRegistry>;
    fn append_data_record(
        &mut self,
        stream: crate::runtime::vm::block_region::StreamCursor,
        record: &[u8],
    ) -> Result<crate::runtime::vm::block_region::DataRecordLocation>;
    fn append_data_record_requires_allocation(
        &self,
        stream: crate::runtime::vm::block_region::StreamCursor,
        record: &[u8],
    ) -> Result<bool>;
    fn append_log_entry(
        &mut self,
        stream: crate::runtime::vm::block_region::StreamCursor,
        entry: TxLogEntry,
    ) -> Result<u32>;
    fn append_log_entry_requires_allocation(
        &self,
        stream: crate::runtime::vm::block_region::StreamCursor,
    ) -> Result<bool>;
    fn mark_last_log_entry_committed(
        &mut self,
        stream: crate::runtime::vm::block_region::StreamCursor,
        marker: PendingCommitLogEntry,
    ) -> Result<u32>;
    fn flush_data_chunk(&self, chunk_start_block: u32) -> Result<()>;
    fn flush_log_block(&self, log_block: u32) -> Result<()>;
    fn fence(&self) -> Result<()>;
    fn retire_linear_undo_chunks<I>(&mut self, chunk_starts: I) -> Result<Vec<u32>>
    where
        I: IntoIterator<Item = u32>;
    fn recover_region_snapshot(&self) -> Result<crate::runtime::vm::RecoveredRegion>;
    fn object_data_chunk_start_for_block(&self, data_block: u32) -> Result<Option<u32>>;
    fn retire_whole_dead_object_chunks(
        &mut self,
        reachable: &[PersistentRecoveredRecordLocation],
        unreachable: &[PersistentRecoveredRecordLocation],
    ) -> Result<Vec<u32>>;
    fn view(&self) -> crate::runtime::vm::block_region::BlockRegionBackendView<'_>;
}
```

- [ ] **Step 3: Replace `FileBackedTxDurableLog` with `DurableRegionLog<R>`**

Rename the struct:

```rust
#[derive(Debug)]
struct DurableRegionLog<R> {
    region: R,
    streams: BTreeMap<u32, crate::runtime::vm::block_region::StreamCursor>,
    pending_data_chunks: BTreeSet<u32>,
    pending_log_blocks: BTreeSet<u32>,
    shared_allocator_lock: Option<Arc<Mutex<()>>>,
}
```

Move the existing `FileBackedTxDurableLog` methods and `TxDurableLogBackend` implementation to `impl<R> DurableRegionLog<R> where R: DurableRegionStorage`.

- [ ] **Step 4: Implement the trait for file-backed region**

In `persist.rs`, implement `DurableRegionStorage` for `FileBackedMemoryBlockRegion` by forwarding to existing public methods.

- [ ] **Step 5: Verify file-backed behavior remains green**

Run:

```bash
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo test -p wasmtime --lib file_backed_ -- --format terse
```

Expected: both commands pass.

### Task 2: Make DAX Block Region Durable-Log Compatible

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`

- [ ] **Step 1: Write DAX durable-log smoke test**

Add a test that creates a research-mode DAX region log, appends a type layout plus one object publication through `TxDurableLog`, recovers the region snapshot, and verifies the object winner exists.

Run:

```bash
cargo test -p wasmtime --lib dax_pmem_durable_region_log_recovers_object_publication -- --format terse
```

Expected before implementation: fail because no `TxDurableLog::create_dax_pmem_research_for_test` exists.

- [ ] **Step 2: Add DAX region methods needed by the storage trait**

Expose DAX equivalents for methods already present on `FileBackedMemoryBlockRegion`: stream cursor allocation, append data/log record, flush chunks/log blocks, type-layout metadata append/load, recovery view, object chunk helpers, and refresh/load from image.

- [ ] **Step 3: Implement `DurableRegionStorage` for DAX**

Forward trait methods to `DaxPmemBlockRegion`.

- [ ] **Step 4: Add DAX durable-log constructors**

Add test-facing constructors first:

```rust
pub(crate) fn create_dax_pmem_research_for_test(num_blocks: usize) -> Result<Self>;
pub(crate) fn create_dax_pmem_fsdax_for_test(path: &Path, num_blocks: usize) -> Result<Self>;
pub(crate) fn open_dax_pmem_fsdax_for_test(path: &Path) -> Result<Self>;
```

These must call `Self::with_backend(DurableRegionLog::<DaxPmemBlockRegion> { ... })`.

- [ ] **Step 5: Verify DAX durable-log behavior**

Run:

```bash
cargo test -p wasmtime --lib dax_pmem_durable_region_log_recovers_object_publication -- --format terse
cargo test -p wasmtime --lib dax_pmem -- --format terse
```

Expected: both commands pass.

### Task 3: Make DAX fsdax TMemory Grow

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`

- [ ] **Step 1: Replace unsupported-growth test**

Replace `dax_pmem_fsdax_growth_is_explicitly_unsupported_for_now` with a growth test that exercises `DaxPmemMemory` through `ResearchTemp` and validates old bytes survive growth.

Run:

```bash
cargo test -p wasmtime --lib dax_pmem_research_growth_preserves_committed_bytes -- --format terse
```

Expected before implementation: fail if the DAX region cannot grow through the backend path.

- [ ] **Step 2: Implement `DaxPmemBlockRegion::grow_to_blocks`**

Mirror the file-backed metadata-layout stability check. Remap to the new byte length, resize `block_entries`, `block_metas`, and `line_marks`, write new block metas as free, write updated header, flush the new metadata range and header, then fence.

- [ ] **Step 3: Route `DaxPmemMemory::reserve_capacity_to_pages` through region growth**

Rename `TMemoryRegion::reserve_file_backed_capacity` to `reserve_capacity`, then use it from both file-backed and DAX. Remove the fsdax growth bail.

- [ ] **Step 4: Keep conservative relocation rejection**

If growth would move the metadata table/descriptor/type-layout area, return the same style of clear error as file-backed: `"DAX PMEM block region grow would require metadata table relocation"`.

- [ ] **Step 5: Verify growth**

Run:

```bash
cargo test -p wasmtime --lib dax_pmem_research_growth_preserves_committed_bytes -- --format terse
cargo test -p wasmtime --lib runtime::vm::memory::tmemory -- --format terse
```

Expected: both commands pass.

### Task 4: Real fsdax Growth And Final Verification

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`

- [ ] **Step 1: Add ignored real-fsdax growth test**

Extend the existing ignored real PMEM tests with: create 1-page fsdax DAX tmemory with max 2 pages, commit bytes before growth, grow to 2 pages, commit bytes in the new page, drop, reopen existing path, verify both byte ranges.

- [ ] **Step 2: Run local checks**

Run:

```bash
cargo test -p wasmtime --lib dax_pmem -- --format terse
cargo test -p wasmtime --lib runtime::vm::memory::tmemory -- --format terse
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo test -p wasmtime --lib file_backed_ -- --format terse
cargo fmt --all -- --check
git diff --check
cargo check -p wasmtime --lib
```

Expected: tests pass; `cargo check` may still report the existing unrelated concurrency warnings.

- [ ] **Step 3: Run remote real-fsdax checks**

Sync changed files to `/home/shisoft/wasmtime-dax-pmem-test` on `192.168.10.74`, then run:

```bash
WASMTIME_TEST_REAL_PMEM=1 WASMTIME_TEST_DAX_PMEM_DIR=/pmem0/wasmtime-dcpmm CARGO_INCREMENTAL=0 cargo test -p wasmtime --lib dax_pmem -- --ignored --format terse
WASMTIME_TEST_REAL_PMEM=1 WASMTIME_TEST_DAX_PMEM_DIR=/pmem1/wasmtime-dcpmm CARGO_INCREMENTAL=0 cargo test -p wasmtime --lib dax_pmem -- --ignored --format terse
```

Expected: ignored real PMEM tests pass on both mounts.
