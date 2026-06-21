# Zero-Copy Recovered Object Records Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop recovery and recovered object-table rebuild from copying persistent object record bytes into DRAM; both file-backed and DAX backends must read recovered records directly from mapped storage.

**Architecture:** Add a common mapped-byte contract at the block-region layer, plus a cloneable mapped-source handle that can outlive a temporary `BlockRegionBackendView`. Recovery keeps the required block-discovery and log-replay passes, then performs one fused post-log classification pass that produces object winners, tmemory size winners, and root ids without repeatedly scanning the winner list. Recovered object winners become metadata-only, and object-table rebuild installs persistent record locations backed by the mapped-source handle, so persistent object payload and trace operations decode from mapped bytes on demand instead of storing serialized record bytes or decoded payloads for all recovered objects.

**Tech Stack:** Rust, Wasmtime transaction runtime internals, mmap-backed file storage, fsdax DAX PMEM mappings, persistent object recovery/GC tests, ignored real-PMEM stress test.

---

### Task 1: Add a common mapped-slice contract to mappings and block-region views

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`

- [ ] **Step 1: Write the failing mapped-slice unit tests**

Add these tests to the existing `#[cfg(test)]` module in `block_region.rs`:

```rust
#[test]
fn file_backed_mapping_exposes_with_mapped_slice() {
    let mut mapping = FileBackedMapping::new_temp(4096).unwrap();
    mapping.write(128, b"mapped-file").unwrap();

    let mut observed = Vec::new();
    mapping
        .with_mapped_slice(128, b"mapped-file".len(), &mut |bytes| {
            observed.extend_from_slice(bytes);
            Ok(())
        })
        .unwrap();

    assert_eq!(observed, b"mapped-file");
}

#[test]
fn dax_research_mapping_exposes_with_mapped_slice() {
    let mut mapping = DaxPmemMapping::new_research_temp(4096).unwrap();
    mapping.write(256, b"mapped-dax").unwrap();

    let mut observed = Vec::new();
    mapping
        .with_mapped_slice(256, b"mapped-dax".len(), &mut |bytes| {
            observed.extend_from_slice(bytes);
            Ok(())
        })
        .unwrap();

    assert_eq!(observed, b"mapped-dax");
}

#[test]
fn block_region_view_exposes_with_mapped_slice() {
    let mut region = VMemoryBlockRegion::new_for_test(1).unwrap();
    region.write(512, b"mapped-view").unwrap();

    let mut observed = Vec::new();
    region
        .view()
        .with_mapped_slice(512, b"mapped-view".len(), &mut |bytes| {
            observed.extend_from_slice(bytes);
            Ok(())
        })
        .unwrap();

    assert_eq!(observed, b"mapped-view");
}
```

- [ ] **Step 2: Verify the RED path**

Run:

```bash
cargo test -p wasmtime --lib with_mapped_slice -- --nocapture
```

Expected: each named test fails to compile because `with_mapped_slice` does not exist.

- [ ] **Step 3: Add `with_mapped_slice` to `FileBackedMapping`, `DaxPmemMapping`, and `DurableMappedBytes`**

Add this method to `impl FileBackedMapping` beside `read`:

```rust
pub(crate) fn with_mapped_slice(
    &self,
    offset: usize,
    len: usize,
    f: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let end = offset
        .checked_add(len)
        .context("file-backed mapping slice range overflow")?;
    ensure!(
        end <= self.len(),
        "file-backed mapping slice range out of bounds"
    );
    let slice = unsafe { self.memory.as_ref() };
    f(&slice[offset..end])
}
```

Extend `DurableMappedBytes`:

```rust
fn with_mapped_slice(
    &self,
    offset: usize,
    len: usize,
    f: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()>;
```

Implement it for both mapping types:

```rust
fn with_mapped_slice(
    &self,
    offset: usize,
    len: usize,
    f: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    FileBackedMapping::with_mapped_slice(self, offset, len, f)
}
```

```rust
fn with_mapped_slice(
    &self,
    offset: usize,
    len: usize,
    f: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    self.mapping.with_mapped_slice(offset, len, f)
}
```

- [ ] **Step 4: Add `with_mapped_slice` to `BlockRegionBackend` and `BlockRegionBackendView`**

Extend the trait:

```rust
fn with_mapped_slice(
    &self,
    offset: usize,
    len: usize,
    f: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()>;
```

Add this forwarding method to `BlockRegionBackendView<'a>`:

```rust
pub(crate) fn with_mapped_slice(
    &self,
    offset: usize,
    len: usize,
    f: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    self.backend.with_mapped_slice(offset, len, f)
}
```

Implement it for the current backends:

```rust
fn with_mapped_slice(
    &self,
    offset: usize,
    len: usize,
    f: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let end = offset
        .checked_add(len)
        .context("transactional block region slice range overflow")?;
    ensure!(end <= self.data.len(), "transactional block region slice range out of bounds");
    f(&self.data[offset..end])
}
```

```rust
fn with_mapped_slice(
    &self,
    offset: usize,
    len: usize,
    f: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    self.mapping.with_mapped_slice(offset, len, f)
}
```

Use the `self.data` implementation for `VMemoryBlockRegion`. Use the `self.mapping` implementation for `DaxPmemBlockRegion` and `FileBackedMemoryBlockRegion`.

- [ ] **Step 5: Add cloneable mapped-source handles for long-lived recovered objects**

Add this trait near `BlockRegionBackend`:

```rust
pub(crate) trait MappedRegionSource: core::fmt::Debug + Send + Sync {
    fn with_mapped_slice(
        &self,
        offset: usize,
        len: usize,
        f: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<()>;
}
```

Add this default method to `BlockRegionBackend`:

```rust
fn mapped_region_source(&self) -> Option<Arc<dyn MappedRegionSource>> {
    None
}
```

Add this forwarding method to `BlockRegionBackendView<'_>`:

```rust
pub(crate) fn mapped_region_source(&self) -> Option<Arc<dyn MappedRegionSource>> {
    self.backend.mapped_region_source()
}
```

Refactor `FileBackedMapping` so clones share the same mmap owner:

```rust
#[derive(Clone, Debug)]
pub(crate) struct FileBackedMapping {
    inner: Arc<FileBackedMappingInner>,
}

#[derive(Debug)]
struct FileBackedMappingInner {
    file: File,
    path: PathBuf,
    unlink_on_drop: AtomicBool,
    memory: RwLock<SendSyncPtr<[u8]>>,
}
```

Move the current `Drop` implementation to `FileBackedMappingInner`. Convert `len`, `ptr_at`, `with_mapped_slice`, `read`, `write`, `flush`, and `remap_len` to lock `inner.memory`. Do not expose a bare `&[u8]` from file-backed or DAX mappings after this refactor; all mapped reads must run inside the `with_mapped_slice` closure while the read lock is held.

Implement `MappedRegionSource` for `FileBackedMapping`:

```rust
impl MappedRegionSource for FileBackedMapping {
    fn with_mapped_slice(
        &self,
        offset: usize,
        len: usize,
        f: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        let guard = self.inner.memory.read().expect("file-backed mapping lock poisoned");
        let end = offset
            .checked_add(len)
            .context("mapped region source slice range overflow")?;
        ensure!(end <= guard.len(), "mapped region source slice range out of bounds");
        let slice = unsafe { guard.as_ref() };
        f(&slice[offset..end])
    }
}
```

Return source handles from durable region backends:

```rust
fn mapped_region_source(&self) -> Option<Arc<dyn MappedRegionSource>> {
    Some(Arc::new(self.mapping.clone()))
}
```

For `DaxPmemBlockRegion`, return a clone of the underlying `FileBackedMapping` wrapped by `DaxPmemMapping`; the source is read-only and does not need to clone `PersistEngine`.

- [ ] **Step 6: Verify mapped-slice tests**

Run:

```bash
cargo test -p wasmtime --lib with_mapped_slice -- --nocapture
```

Expected: all three tests pass.

### Task 2: Make recovered object winners metadata-only and fuse winner classification

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`
- Modify: `crates/wasmtime/tests/transaction_persistence.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`

- [ ] **Step 1: Write the failing metadata-only recovery test**

Add this test to `recovery.rs` tests:

```rust
#[test]
fn recovered_object_winner_maps_record_bytes_from_region() {
    let mut region = VMemoryBlockRegion::new(8).unwrap();
    let stream = region.alloc_stream(7).unwrap();
    let record = crate::runtime::transaction::encode_object_record_for_recovery(
        41,
        3,
        ObjectKind::Struct as u16,
        TypeLayoutId::DEFAULT_STRUCT.get(),
        &crate::runtime::transaction::ObjectPayload::Struct(vec![
            crate::runtime::transaction::ObjectValue::I32(11),
        ]),
    )
    .unwrap();
    let data_record = TMemory::encode_publication_data_record(
        pack_object_granule_id(PackedGranuleDomain::TStruct, 41).unwrap(),
        3,
        PackedGranuleDomain::TStruct as u16,
        TypeLayoutId::DEFAULT_STRUCT.get(),
        &record,
    )
    .unwrap();
    let location = region.append_data_record(stream, &data_record).unwrap();
    let log_block = region.alloc_log_block(7, 0).unwrap();
    let entry = TMemory::publication_log_entry(
        pack_object_granule_id(PackedGranuleDomain::TStruct, 41).unwrap(),
        3,
        14,
        location.data_block,
        location.data_offset,
        region.block_generation(location.data_block).unwrap(),
        true,
    )
    .unwrap();
    region.write_log_entries(log_block, &[entry]).unwrap();

    let recovered = recover_region(&region.view(), TypeLayoutRegistry::default()).unwrap();
    let winners = recovered.committed_object_winners().unwrap();
    let mut observed = Vec::new();
    winners[0]
        .with_view_record_bytes(&region.view(), &mut |bytes| {
            observed.extend_from_slice(bytes);
            Ok(())
        })
        .unwrap();

    assert_eq!(observed, record);
}
```

- [ ] **Step 2: Verify the RED path**

Run:

```bash
cargo test -p wasmtime --lib recovered_object_winner_maps_record_bytes_from_region -- --exact --nocapture
```

Expected: compile failure because `RecoveredObjectWinner::with_view_record_bytes` does not exist and `RecoveredObjectWinner` still owns `record_bytes`.

- [ ] **Step 3: Replace `record_bytes: Vec<u8>` in `RecoveredObjectWinner`**

Change the type in `recovery.rs` to:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveredObjectWinner {
    pub(crate) object_id: u64,
    pub(crate) version: u32,
    pub(crate) kind: u16,
    pub(crate) type_layout_id: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) data_record_offset: usize,
    pub(crate) record_len: u64,
}
```

Add this method:

```rust
impl RecoveredObjectWinner {
    pub(crate) fn with_source_record_bytes(
        &self,
        source: &dyn crate::runtime::vm::block_region::MappedRegionSource,
        f: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        let payload_offset = self
            .data_record_offset
            .checked_add(size_of::<TxDataRecordHeader>())
            .context("recovered object payload offset overflow")?;
        let record_len =
            usize::try_from(self.record_len).context("recovered object record length overflow")?;
        source.with_mapped_slice(payload_offset, record_len, f)
    }

    pub(crate) fn with_view_record_bytes(
        &self,
        region: &BlockRegionBackendView<'_>,
        f: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        let payload_offset = self
            .data_record_offset
            .checked_add(size_of::<TxDataRecordHeader>())
            .context("recovered object payload offset overflow")?;
        let record_len =
            usize::try_from(self.record_len).context("recovered object record length overflow")?;
        region.with_mapped_slice(payload_offset, record_len, f)
    }

    pub(crate) fn durable_data_record_len(&self) -> Result<u64> {
        self.record_len
            .checked_add(u64::try_from(size_of::<TxDataRecordHeader>()).unwrap())
            .context("recovered object durable data record length overflow")
    }
}
```

- [ ] **Step 4: Split publication payload loading into mapped and copied paths**

Replace `load_publication_payload` with these helpers:

```rust
fn with_publication_payload<T>(
    region: &BlockRegionBackendView<'_>,
    data_chunk_index: &DataChunkIndex,
    data_block: u32,
    data_offset: u32,
    f: impl FnOnce(TxDataRecordHeader, &[u8], usize) -> Result<T>,
) -> Result<T> {
    let (record_offset, chunk_tail_offset) =
        data_chunk_index.locate_data_record_offset(region, data_block, data_offset)?;
    let mut header = None;
    region.with_mapped_slice(
        record_offset,
        size_of::<TxDataRecordHeader>(),
        &mut |bytes| {
            header = Some(TxDataRecordHeader::from_bytes(bytes)?);
            Ok(())
        },
    )?;
    let header = header.context("publication header was not mapped")?;
    let payload_len =
        usize::try_from(header.payload_len).context("publication payload length overflow")?;
    let payload_offset = record_offset
        .checked_add(size_of::<TxDataRecordHeader>())
        .context("publication payload offset overflow")?;
    let payload_end = payload_offset
        .checked_add(payload_len)
        .context("publication payload end overflow")?;
    ensure!(
        payload_end <= chunk_tail_offset,
        "publication data record extends beyond data chunk tail"
    );
    let mut result = None;
    let mut f = Some(f);
    region.with_mapped_slice(payload_offset, payload_len, &mut |payload| {
        let f = f.take().context("publication payload callback already used")?;
        result = Some(f(header, payload, record_offset)?);
        Ok(())
    })?;
    result.context("publication payload was not mapped")
}

fn copy_publication_payload(
    region: &BlockRegionBackendView<'_>,
    data_chunk_index: &DataChunkIndex,
    data_block: u32,
    data_offset: u32,
) -> Result<(TxDataRecordHeader, Vec<u8>)> {
    with_publication_payload(region, data_chunk_index, data_block, data_offset, |header, payload, _| {
        Ok((header, payload.to_vec()))
    })
}
```

Use `with_publication_payload` for object winners and `copy_publication_payload` for root ids, tmemory size records, and tmemory undo rollbacks. Do not store object payload bytes in `RecoveredObjectWinner`.

- [ ] **Step 5: Add one fused post-log winner classification pass**

Replace the separate calls in `recover_region`:

```rust
let object_winners = replay_object_winners(region, &discovered.data_chunk_index, &winners)?;
let tmemory_size_winners =
    replay_tmemory_size_winners(region, &discovered.data_chunk_index, &winners)?;
let root_object_ids = replay_root_object_ids(region, &discovered.data_chunk_index, &winners)?;
```

with:

```rust
let classified = classify_recovered_winners(region, &discovered.data_chunk_index, &winners)?;
```

and build `RecoveredRegion` from:

```rust
object_winners: classified.object_winners,
tmemory_size_winners: classified.tmemory_size_winners,
root_object_ids: classified.root_object_ids,
```

Add this private accumulator:

```rust
#[derive(Debug, Default)]
struct RecoveredClassifiedWinners {
    object_winners: Vec<RecoveredObjectWinner>,
    tmemory_size_winners: Vec<RecoveredTMemorySizeWinner>,
    root_object_ids: Vec<u64>,
}
```

Implement the fused pass:

```rust
fn classify_recovered_winners(
    region: &BlockRegionBackendView<'_>,
    data_chunk_index: &DataChunkIndex,
    winners: &[RecoveryWinner],
) -> Result<RecoveredClassifiedWinners> {
    let mut object_winners = BTreeMap::<u64, RecoveredObjectWinner>::new();
    let mut tmemory_size_winners = Vec::new();
    let mut root_object_ids = Vec::new();

    for winner in winners {
        if let Ok((domain, object_id)) = unpack_object_granule_id(winner.logical_id) {
            let candidate =
                classify_object_winner(region, data_chunk_index, winner, domain, object_id)?;
            match object_winners.get(&object_id) {
                Some(current) if current.version > candidate.version => {}
                Some(current) if current.version == candidate.version => {
                    bail!(
                        "duplicate committed object version {} for object id {}",
                        candidate.version,
                        object_id
                    );
                }
                _ => {
                    object_winners.insert(object_id, candidate);
                }
            }
            continue;
        }

        let Ok(domain) = packed_granule_domain(winner.logical_id) else {
            continue;
        };
        match domain {
            PackedGranuleDomain::TMemorySize => {
                tmemory_size_winners.push(classify_tmemory_size_winner(
                    region,
                    data_chunk_index,
                    winner,
                )?);
            }
            PackedGranuleDomain::TGlobal | PackedGranuleDomain::TTable => {
                root_object_ids.extend(classify_root_object_ids(
                    region,
                    data_chunk_index,
                    winner,
                    domain,
                )?);
            }
            _ => {}
        }
    }

    root_object_ids.sort_unstable();
    root_object_ids.dedup();
    Ok(RecoveredClassifiedWinners {
        object_winners: object_winners.into_values().collect(),
        tmemory_size_winners,
        root_object_ids,
    })
}
```

Delete the old multi-pass functions after the fused replacements are in place:

```rust
replay_object_winners
replay_tmemory_size_winners
replay_root_object_ids
load_root_publication_payload
```

Keep `decode_root_object_refs`, `object_domain_matches_object_kind`, and the new `copy_publication_payload` helper.

- [ ] **Step 6: Implement mapped object classification**

Add `classify_object_winner`:

```rust
fn classify_object_winner(
    region: &BlockRegionBackendView<'_>,
    data_chunk_index: &DataChunkIndex,
    winner: &RecoveryWinner,
    domain: PackedGranuleDomain,
    object_id: u64,
) -> Result<RecoveredObjectWinner> {
    with_publication_payload(
        region,
        data_chunk_index,
        winner.data_block,
        winner.data_offset,
        |data_header, record_bytes, data_record_offset| {
            ensure!(
                data_header.logical_id == winner.logical_id,
                "recovered object publication logical id does not match log winner"
            );
            ensure!(
                data_header.version == winner.version,
                "recovered object publication version does not match log winner"
            );
            ensure!(
                data_header.role()? == TxDataRecordRole::TObjectPub,
                "recovered object publication data record has non-publication role"
            );
            ensure!(
                data_header.kind == domain as u16,
                "recovered object publication kind does not match object domain"
            );
            let object_header = TxObjectHeader::read_from_prefix(record_bytes)?;
            ensure!(
                object_header.object_id == object_id,
                "recovered object record id does not match logical id"
            );
            ensure!(
                object_header.version == winner.version,
                "recovered object record version does not match log winner"
            );
            ensure!(
                data_header.type_info == object_header.type_layout_id,
                "recovered object publication outer type layout id does not match object record"
            );
            ensure!(
                object_domain_matches_object_kind(domain, object_header.kind),
                "recovered object publication outer kind does not match object record kind"
            );
            TypeLayoutId::new(object_header.type_layout_id)
                .context("recovered object record type layout id cannot be zero")?;
            Ok(RecoveredObjectWinner {
                object_id,
                version: winner.version,
                kind: object_header.kind,
                type_layout_id: object_header.type_layout_id,
                data_block: winner.data_block,
                data_offset: winner.data_offset,
                data_record_offset,
                record_len: object_header.record_len,
            })
        },
    )
}
```

- [ ] **Step 7: Implement small copied classification for tmemory size and roots**

Add `classify_tmemory_size_winner`:

```rust
fn classify_tmemory_size_winner(
    region: &BlockRegionBackendView<'_>,
    data_chunk_index: &DataChunkIndex,
    winner: &RecoveryWinner,
) -> Result<RecoveredTMemorySizeWinner> {
    let (data_header, payload) = copy_publication_payload(
        region,
        data_chunk_index,
        winner.data_block,
        winner.data_offset,
    )?;
    ensure!(
        data_header.logical_id == winner.logical_id,
        "recovered tmemory size logical id does not match log winner"
    );
    ensure!(
        data_header.version == winner.version,
        "recovered tmemory size version does not match log winner"
    );
    ensure!(
        data_header.role()? == TxDataRecordRole::TObjectPub,
        "recovered tmemory size data record has non-publication role"
    );
    ensure!(
        data_header.kind == PackedGranuleDomain::TMemorySize as u16,
        "recovered tmemory size kind does not match log winner"
    );
    ensure!(
        payload.len() == size_of::<u64>(),
        "recovered tmemory size payload must be exactly 8 bytes"
    );
    let (owner_instance, memory_index) = unpack_tmemory_size_logical_id(winner.logical_id)?;
    Ok(RecoveredTMemorySizeWinner {
        owner_instance,
        memory_index,
        version: winner.version,
        new_pages: u64::from_le_bytes(payload.try_into().unwrap()),
    })
}
```

Add `classify_root_object_ids`:

```rust
fn classify_root_object_ids(
    region: &BlockRegionBackendView<'_>,
    data_chunk_index: &DataChunkIndex,
    winner: &RecoveryWinner,
    expected_domain: PackedGranuleDomain,
) -> Result<Vec<u64>> {
    let (data_header, payload) = copy_publication_payload(
        region,
        data_chunk_index,
        winner.data_block,
        winner.data_offset,
    )?;
    ensure!(
        data_header.logical_id == winner.logical_id,
        "recovered root publication logical id does not match log winner"
    );
    ensure!(
        data_header.version == winner.version,
        "recovered root publication version does not match log winner"
    );
    ensure!(
        data_header.role()? == TxDataRecordRole::TObjectPub,
        "recovered root publication data record has non-publication role"
    );
    ensure!(
        data_header.kind == expected_domain as u16,
        "recovered root publication kind does not match granule domain"
    );
    match expected_domain {
        PackedGranuleDomain::TGlobal => Ok(decode_root_object_refs(&payload)?
            .into_iter()
            .take(1)
            .collect()),
        PackedGranuleDomain::TTable => decode_root_object_refs(&payload),
        _ => bail!("recovered root classifier received non-root domain"),
    }
}
```

- [ ] **Step 8: Update test-export helpers without reintroducing runtime copies**

Keep `TransactionPersistenceRecoveredObjectWinner { record_bytes: Vec<u8> }` as a test-facing compatibility type in `block_region.rs`, but build it explicitly through `winner.with_source_record_bytes` inside `reopen_and_recover_file_backed_region`.

Use this shape:

```rust
let runtime_recovered = reopen_and_recover_file_backed_region_for_runtime(path)?;
let region = FileBackedMemoryBlockRegion::open_for_test(path)?;
let source = region
    .view()
    .mapped_region_source()
    .context("file-backed recovery test helper requires mapped source")?;
let object_winners = runtime_recovered
    .object_winners
    .iter()
    .map(|winner| {
        let mut record_bytes = Vec::new();
        winner.with_source_record_bytes(&*source, &mut |bytes| {
            record_bytes.extend_from_slice(bytes);
            Ok(())
        })?;
        Ok(TransactionPersistenceRecoveredObjectWinner {
            object_id: winner.object_id,
            version: winner.version,
            record_bytes,
        })
    })
    .collect::<Result<Vec<_>>>()?;
```

This copy is allowed only in the explicit exported test helper. Runtime recovery and object-table rebuild must not use it.

- [ ] **Step 9: Update test fixtures that construct `RecoveredObjectWinner` manually**

For synthetic unit tests in `transaction/tests.rs` and `vm/libcalls.rs`, replace direct `RecoveredObjectWinner { record_bytes }` construction with a local helper that also creates an in-memory `VMemoryBlockRegion` publication when the test needs mapped bytes.

Use this helper shape in `transaction/tests.rs`:

```rust
struct RecoveredObjectFixture {
    region: crate::runtime::vm::block_region::VMemoryBlockRegion,
    winners: Vec<crate::runtime::vm::RecoveredObjectWinner>,
}
```

The helper publishes object records into `region`, calls `recover_region(&region.view(), layouts.clone())`, and returns `recovered.committed_object_winners()?`. Tests that only need metadata can construct `RecoveredObjectWinner` with `data_record_offset: 0` and must not call `with_view_record_bytes` or `with_source_record_bytes`.

- [ ] **Step 10: Verify recovery tests and the fused classifier shape**

Run:

```bash
cargo test -p wasmtime --lib recovered_object_winner_maps_record_bytes_from_region -- --exact --nocapture
cargo test -p wasmtime --lib transaction::vm::memory::tmemory::recovery -- --format terse
test -z "$(rg "fn replay_object_winners|fn replay_tmemory_size_winners|fn replay_root_object_ids|fn load_root_publication_payload" crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs || true)"
```

Expected: the new test passes, recovery tests pass, and the `test -z` command exits 0 because the removed multi-pass helpers are absent.

### Task 3: Add a mapped persistent object record source for recovered object tables

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_heap.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/object_table.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`

- [ ] **Step 1: Write the failing object-table zero-copy test**

Add this test to `transaction/tests.rs`:

```rust
#[test]
fn recovered_object_table_installs_mapped_records_without_copying_record_bytes() -> Result<()> {
    let temp = tempfile::Builder::new()
        .prefix("wasmtime-mapped-object-table")
        .tempdir()?;
    let path = temp.path().join("region.bin");
    let mut region = FileBackedMemoryBlockRegion::create_for_test(&path, 8)?;
    publish_committed_struct_object(&path, 7, 41, 1, TypeLayoutId::DEFAULT_STRUCT.get(), &[7])?;
    region.refresh_from_image()?;

    let recovered = recover_region(&region.view(), region.load_type_layout_metadata()?)?;
    let winners = recovered.committed_object_winners()?;
    let source = region
        .view()
        .mapped_region_source()
        .context("file-backed test region must expose mapped source")?;
    let mut objects = ObjectTable::default();

    objects.rebuild_from_mapped_recovered_object_winners(
        &recovered.type_layouts,
        &winners,
        source,
    )?;

    assert_eq!(
        objects.payload(ObjectId { object_index: 41 })?,
        ObjectPayload::Struct(vec![ObjectValue::I32(7)])
    );
    assert!(objects.current_record_is_persistent_mapped_for_test(ObjectId { object_index: 41 })?);
    Ok(())
}
```

The test must fail before implementation because `rebuild_from_mapped_recovered_object_winners` and `current_record_is_persistent_mapped_for_test` do not exist.

- [ ] **Step 2: Add persistent record storage to `ObjectHeap`**

Change `ObjectRecord` in `object_heap.rs` from a single copied-record representation to this shape:

```rust
#[derive(Clone, Debug)]
enum ObjectRecordStorage {
    Volatile {
        offset: usize,
        location: ObjectRecordLocation,
        payload: ObjectPayload,
    },
    PersistentMapped {
        source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
        data_record_offset: usize,
        record_len: u64,
        location: ObjectRecordLocation,
    },
}

#[derive(Clone, Debug)]
struct ObjectRecord {
    header: TxObjectHeader,
    array_length: Option<u32>,
    storage: ObjectRecordStorage,
}
```

Keep `allocate_record` and `install_record_bytes` using `ObjectRecordStorage::Volatile`. Add:

```rust
pub(crate) fn install_mapped_persistent_record(
    &mut self,
    winner: &crate::runtime::vm::RecoveredObjectWinner,
    source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
    record_bytes: &[u8],
) -> Result<TxRecordHandle> {
    let header = TxObjectHeader::read_from_prefix(record_bytes)?;
    let record_len =
        usize::try_from(header.record_len).context("record length does not fit usize")?;
    ensure!(
        record_len == record_bytes.len(),
        "serialized object record length does not match header"
    );
    let array_length = if header.kind == ObjectKind::Array as u16 {
        ensure!(
            record_bytes.len() >= size_of::<TxArrayHeader>(),
            "serialized array record is shorter than expected"
        );
        Some(u32::from_le_bytes(
            record_bytes[TxObjectHeader::BYTE_LEN..TxObjectHeader::BYTE_LEN + size_of::<u32>()]
                .try_into()
                .unwrap(),
        ))
    } else {
        None
    };
    let location = ObjectRecordLocation {
        chunk_start_block: winner.data_block,
        chunk_blocks: 0,
        data_block: winner.data_block,
        data_offset: winner.data_offset,
        record_len: winner.record_len,
    };
    self.records.push(ObjectRecord {
        header,
        array_length,
        storage: ObjectRecordStorage::PersistentMapped {
            source,
            data_record_offset: winner.data_record_offset,
            record_len: winner.record_len,
            location,
        },
    });
    let handle = u64::try_from(self.records.len()).context("record handle overflow")?;
    Ok(TxRecordHandle(handle))
}
```

- [ ] **Step 3: Add mapped-record accessors to `ObjectHeap`**

Add a private helper:

```rust
fn with_record_bytes<T>(
    &self,
    handle: TxRecordHandle,
    f: impl FnOnce(&[u8]) -> Result<T>,
) -> Result<T> {
    let record = self.record(handle)?;
    match &record.storage {
        ObjectRecordStorage::Volatile { offset, .. } => {
            let bytes = self
                .region()?
                .read(*offset, usize::try_from(record.header.record_len)?)?;
            f(&bytes)
        }
        ObjectRecordStorage::PersistentMapped {
            source,
            data_record_offset,
            record_len,
            ..
        } => {
            let payload_offset = data_record_offset
                .checked_add(size_of::<TxDataRecordHeader>())
                .context("mapped persistent object payload offset overflow")?;
            let mut result = None;
            let mut f = Some(f);
            source.with_mapped_slice(
                payload_offset,
                usize::try_from(*record_len).context("mapped persistent object record length overflow")?,
                &mut |bytes| {
                    let f = f.take().context("mapped record callback already used")?;
                    result = Some(f(bytes)?);
                    Ok(())
                },
            )?;
            result.context("mapped persistent object record was not read")
        }
    }
}
```

Then make `payload`, `payload_bytes`, `trace_object_ids`, `record_bytes`, and `publication_record` use `with_record_bytes`. For persistent mapped records, do not allocate a `Vec<u8>` unless the called public/test method explicitly returns `Vec<u8>`.

- [ ] **Step 4: Add mapped rebuild methods to `ObjectTable`**

Add:

```rust
pub(crate) fn rebuild_from_mapped_recovered_object_winners(
    &mut self,
    recovered_type_layouts: &TypeLayoutRegistry,
    winners: &[crate::runtime::vm::RecoveredObjectWinner],
    mapped_source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
) -> Result<()> {
    self.clear_volatile_index();
    self.heap = object_heap::ObjectHeap::default();
    self.install_recovered_type_layouts(recovered_type_layouts)?;
    self.rebuild_slots_for_recovered_winners(winners)?;

    for winner in winners {
        let mut handle = None;
        winner.with_source_record_bytes(&*mapped_source, &mut |record_bytes| {
            handle = Some(self.heap.install_mapped_persistent_record(
                winner,
                mapped_source.clone(),
                record_bytes,
            )?);
            Ok(())
        })?;
        let handle = handle.context("mapped recovered object was not installed")?;
        self.install_recovered_slot_from_handle(winner, handle)?;
    }

    self.rebuild_free_list_holes()?;
    Ok(())
}
```

Add the reachable-only wrapper used by runtime restart:

```rust
pub(crate) fn rebuild_reachable_from_mapped_recovered_object_winners(
    &mut self,
    recovered_type_layouts: &TypeLayoutRegistry,
    winners: &[crate::runtime::vm::RecoveredObjectWinner],
    root_object_ids: &[u64],
    mapped_source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
) -> Result<PersistentRecoveryGcReport> {
    let report = Self::persistent_recovery_gc_report_mapped(
        recovered_type_layouts,
        winners,
        root_object_ids,
        mapped_source.clone(),
    )?;
    let reachable_winners = winners
        .iter()
        .filter(|winner| {
            report.mark.reachable.contains(&ObjectId {
                object_index: winner.object_id,
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut rebuilt = ObjectTable::default();
    rebuilt.rebuild_from_mapped_recovered_object_winners(
        recovered_type_layouts,
        &reachable_winners,
        mapped_source,
    )?;
    *self = rebuilt;
    Ok(report)
}
```

Extract the existing slot allocation and validation logic from `rebuild_from_recovered_object_winners` into private helpers:

```rust
fn rebuild_slots_for_recovered_winners(
    &mut self,
    winners: &[crate::runtime::vm::RecoveredObjectWinner],
) -> Result<()> {
    if let Some(max_object_id) = winners.iter().map(|winner| winner.object_id).max() {
        let slot_len = usize::try_from(
            max_object_id
                .checked_add(1)
                .context("object slot range overflow")?,
        )
        .context("object slot range does not fit usize")?;
        self.slots.resize(slot_len, None);
    }
    Ok(())
}

fn install_recovered_slot_from_handle(
    &mut self,
    winner: &crate::runtime::vm::RecoveredObjectWinner,
    handle: object_heap::TxRecordHandle,
) -> Result<()> {
    let object_id = ObjectId {
        object_index: winner.object_id,
    };
    let index = object_slot_index(object_id)?;
    ensure!(
        self.slots.get(index).is_some(),
        "recovered object slot is outside slot range"
    );
    ensure!(
        self.slots[index].is_none(),
        "recovered object slot is already occupied"
    );

    let header = self.heap.header(handle)?;
    let kind = object_kind_from_u16(header.kind)?;
    let type_layout_id = TypeLayoutId::new(header.type_layout_id)
        .context("recovered object record type layout id cannot be zero")?;
    ensure!(
        header.object_id == winner.object_id,
        "recovered object record id does not match winner"
    );
    ensure!(
        header.version == winner.version,
        "recovered object record version does not match winner"
    );
    ensure!(
        header.kind == winner.kind,
        "recovered object record kind does not match winner"
    );
    ensure!(
        header.type_layout_id == winner.type_layout_id,
        "recovered object record type layout id does not match winner"
    );
    self.validate_type_layout_for_object_kind(kind, type_layout_id)?;

    self.next_record_version = self.next_record_version.max(header.version);
    let version = self.bump_object_version()?;
    self.slots[index] = Some(ObjectTableSlot {
        kind,
        version,
        type_layout_id: header.type_layout_id,
        runtime_type_index: None,
        persistent: true,
        current_record: handle,
    });
    self.live_count = self
        .live_count
        .checked_add(1)
        .context("object table live count overflow during recovery rebuild")?;
    Ok(())
}
```

Remove production use of `rebuild_from_recovered_object_winners`. Tests that used synthetic copied winners must build a `VMemoryBlockRegion` fixture and call the mapped rebuild method.

- [ ] **Step 5: Make persistent GC tracing use mapped bytes**

Add a mapped variant named `persistent_recovery_gc_report_mapped` with the same report construction as `persistent_recovery_gc_report` and this signature:

```rust
pub(crate) fn persistent_recovery_gc_report_mapped(
    recovered_type_layouts: &TypeLayoutRegistry,
    winners: &[crate::runtime::vm::RecoveredObjectWinner],
    root_object_ids: &[u64],
    mapped_source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
) -> Result<PersistentRecoveryGcReport>
```

The implementation must keep the existing winner-map, root validation, dangling-reference reporting, and reachable/unreachable record-location construction. Inside the mark loop, replace:

```rust
Self::trace_recovered_winner_object_ids(&layout_table, winner)?
```

with:

```rust
Self::trace_mapped_recovered_winner_object_ids(&layout_table, winner, &*mapped_source)?
```

First expose two narrow recovery helpers from `object_heap.rs`:

```rust
pub(crate) fn payload_offset_for_recovery(array_length: Option<u32>) -> usize {
    payload_offset(array_length)
}

pub(crate) fn decode_payload_bytes_for_recovery(
    header: TxObjectHeader,
    array_length: Option<u32>,
    bytes: &[u8],
) -> Result<ObjectPayload> {
    decode_payload_bytes(header, array_length, bytes)
}
```

Implement the mapped trace helper with this structure:

```rust
let mut traced = None;
winner.with_source_record_bytes(mapped_source, &mut |record_bytes| {
    let header = TxObjectHeader::read_from_prefix(record_bytes)?;
    let array_length = if header.kind == ObjectKind::Array as u16 {
        Some(u32::from_le_bytes(
            record_bytes[TxObjectHeader::BYTE_LEN..TxObjectHeader::BYTE_LEN + size_of::<u32>()]
                .try_into()
                .unwrap(),
        ))
    } else {
        None
    };
    let payload_start = object_heap::payload_offset_for_recovery(array_length);
    let payload = record_bytes
        .get(payload_start..)
        .context("mapped recovered object payload is out of bounds")?;
    let mut out = Vec::new();
    if uses_placeholder_persistent_trace_fallback(type_layout_id) {
        let decoded = object_heap::decode_payload_bytes_for_recovery(header, array_length, record_bytes)?;
        match decoded {
            ObjectPayload::Struct(fields) | ObjectPayload::Array(fields) => {
                out.extend(fields.into_iter().filter_map(|value| match value {
                    ObjectValue::Ref(Some(object_id)) => Some(object_id),
                    _ => None,
                }));
            }
        }
    } else {
        object_heap::trace_object_refs_with_layout(layout, payload, &mut out)?;
    }
    traced = Some(out);
    Ok(())
})?;
traced.context("mapped recovered object was not traced")
```

- [ ] **Step 6: Verify object-table tests**

Run:

```bash
cargo test -p wasmtime --lib recovered_object_table_installs_mapped_records_without_copying_record_bytes -- --exact --nocapture
cargo test -p wasmtime --lib transaction::tests::persistent -- --format terse
```

Expected: the new zero-copy test passes, and persistent object tests pass.

### Task 4: Route file-backed and DAX recovery through mapped object-table rebuild

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`

- [ ] **Step 1: Add a backend snapshot API that carries the mapped source handle**

Add this method to `TxDurableLogBackend`:

```rust
fn with_recovered_region_snapshot(
    &self,
    f: &mut dyn FnMut(
        &crate::runtime::vm::RecoveredRegion,
        Option<Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>>,
    ) -> Result<()>,
) -> Result<bool> {
    let Some(recovered) = self.recover_region_snapshot()? else {
        return Ok(false);
    };
    f(&recovered, None)?;
    Ok(true)
}
```

Add forwarding to `TxDurableLog`:

```rust
pub(crate) fn with_recovered_region_snapshot(
    &self,
    f: &mut dyn FnMut(
        &crate::runtime::vm::RecoveredRegion,
        Option<Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>>,
    ) -> Result<()>,
) -> Result<bool> {
    self.storage.with_recovered_region_snapshot(f)
}
```

Override it for `DurableRegionLog<R>`:

```rust
fn with_recovered_region_snapshot(
    &self,
    f: &mut dyn FnMut(
        &crate::runtime::vm::RecoveredRegion,
        Option<Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>>,
    ) -> Result<()>,
) -> Result<bool> {
    let recovered = self.region.recover_region_snapshot()?;
    let source = self
        .region
        .view()
        .mapped_region_source()
        .context("durable region recovery requires mapped source")?;
    f(&recovered, Some(source))?;
    Ok(true)
}
```

- [ ] **Step 2: Update runtime recovery call sites**

Replace call sites that currently do:

```rust
let recovered = self
    .durable_log
    .recover_region_snapshot()?
    .context("persistent object recovery requires durable block region backend")?;
let winners = recovered.committed_object_winners()?;
objects.rebuild_reachable_from_recovered_object_winners(
    &recovered.type_layouts,
    &winners,
    &recovered.root_object_ids,
)?;
```

with:

```rust
let mut consumed = false;
self.durable_log.with_recovered_region_snapshot(&mut |recovered, mapped_source| {
    let mapped_source = mapped_source.context("persistent object recovery requires mapped durable region")?;
    let winners = recovered.committed_object_winners()?;
    objects.rebuild_reachable_from_mapped_recovered_object_winners(
        &recovered.type_layouts,
        &winners,
        &recovered.root_object_ids,
        mapped_source,
    )?;
    consumed = true;
    Ok(())
})?;
ensure!(consumed, "persistent object recovery requires a durable block region backend");
```

Update the four `vm/libcalls.rs` recovery/rebuild call sites and the `state.rs` persistent compaction path in the same style.

- [ ] **Step 3: Preserve metadata-only snapshot APIs for non-object users**

Keep `recover_region_snapshot()` available for callers that only need:
- log winners
- root ids
- tmemory size winners
- tmemory undo rollbacks
- object metadata and GC record locations

Add this comment above `recover_region_snapshot`:

```rust
// This returns metadata only for recovered object records. Callers that need
// object record bytes must consume the snapshot through
// `with_recovered_region_snapshot` so the mapped backend view stays alive.
```

- [ ] **Step 4: Verify runtime recovery tests**

Run:

```bash
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo test -p wasmtime --lib tmemory -- --format terse
```

Expected: transaction and tmemory tests pass.

### Task 5: Update stress validation and add a large-recovery regression guard

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Modify: `docs/superpowers/plans/2026-06-21-dax-pmem-stress-test.md`

- [ ] **Step 1: Update DAX stress validation to sample mapped bytes**

In `dax_pmem_fsdax_persistent_data_stress`, replace direct `winner.record_bytes` comparisons with:

```rust
let recovered = log.recover_region_snapshot()?.unwrap();
let source = log
    .mapped_region_source_for_test()?
    .context("DAX stress requires mapped region source")?;
let winners = recovered
    .committed_object_winners()?
    .into_iter()
    .map(|winner| (winner.object_id, winner))
    .collect::<BTreeMap<_, _>>();

for expected in sampled_objects {
    let winner = winners
        .get(&expected.object_id)
        .context("sampled persistent object was not recovered")?;
    let mut matched = false;
    winner.with_source_record_bytes(&*source, &mut |record_bytes| {
        ensure!(record_bytes == expected.record.as_slice(), "sampled object bytes mismatch");
        matched = true;
        Ok(())
    })?;
    ensure!(matched, "sampled object bytes were not mapped");
}
```

If `mapped_region_source_for_test` does not exist, add a `#[cfg(test)]` helper on `TxDurableLog` that returns `Option<Arc<dyn MappedRegionSource>>` for `DurableRegionLog<R>` and `None` for in-memory storage.

- [ ] **Step 2: Add a bounded local regression test for no eager object-byte copies**

Add this ignored-free unit test in `persist.rs`:

```rust
#[test]
fn file_backed_recovery_handles_many_large_object_records_without_materializing_bytes() -> Result<()> {
    let temp = tempfile::Builder::new()
        .prefix("wasmtime-zero-copy-recovery")
        .tempdir()?;
    let path = temp.path().join("region.bin");
    let mut log = TxDurableLog::create_file_backed(&path, 256)?;
    let layout = scalar_struct_type_layout_for_test(9001, 64)?;
    log.ensure_type_layout(&layout)?;

    for object_id in 0..512u64 {
        let payload = vec![0x5au8; 4096];
        let object_record = encode_object_record_for_recovery(
            object_id,
            1,
            ObjectKind::Struct as u16,
            layout.id().get(),
            &ObjectPayload::Struct(
                payload
                    .chunks_exact(4)
                    .map(|chunk| ObjectValue::I32(i32::from_le_bytes(chunk.try_into().unwrap())))
                    .collect(),
            ),
        )?;
        let publication = PendingPublication::persistent_object(
            PackedGranuleDomain::TStruct,
            object_id,
            1,
            layout.id().get(),
            object_record,
        )?;
        publish_single_object_publication_for_test(&mut log, object_id as u32 + 1, publication)?;
    }

    let recovered = log.recover_region_snapshot()?.unwrap();
    let winners = recovered.committed_object_winners()?;
    assert_eq!(winners.len(), 512);
    assert_eq!(
        winners.iter().map(|winner| winner.record_len).sum::<u64>(),
        512 * winners[0].record_len
    );
    Ok(())
}
```

This is not a memory profiler. Its job is to keep the recovery API metadata-only and prevent accidental reintroduction of `record_bytes` into `RecoveredObjectWinner`.

- [ ] **Step 3: Verify local stress-related tests**

Run:

```bash
cargo test -p wasmtime --lib file_backed_recovery_handles_many_large_object_records_without_materializing_bytes -- --exact --nocapture
cargo test -p wasmtime --lib dax_pmem_fsdax_persistent_data_stress -- --ignored --exact --nocapture
```

Expected: the bounded test passes; the ignored DAX stress test exits early without env vars and prints the skip message.

- [ ] **Step 4: Update the stress-test plan note**

Append this note to `docs/superpowers/plans/2026-06-21-dax-pmem-stress-test.md`:

```markdown
## Zero-Copy Recovery Requirement

The stress harness must validate recovered object samples through mapped record
bytes. It must not access a `RecoveredObjectWinner.record_bytes` field or build a
`Vec<u8>` for every recovered object winner. A 256GiB object dataset is expected
to keep recovery RSS near metadata scale rather than object-data scale.
```

### Task 6: Real-PMEM verification on the Optane host

**Files:**
- No source edits expected.

- [ ] **Step 1: Sync the branch to the Optane host**

Run:

```bash
rsync -a --delete --exclude .git --exclude target ./ shisoft@192.168.10.74:/home/shisoft/wasmtime-dax-pmem-test/
```

Expected: sync completes without deleting `/pmem0/wasmtime-dcpmm` or `/pmem1/wasmtime-dcpmm`.

- [ ] **Step 2: Run a 1GiB release sanity check**

Run:

```bash
ssh shisoft@192.168.10.74 'cd /home/shisoft/wasmtime-dax-pmem-test && WASMTIME_TEST_REAL_PMEM=1 WASMTIME_DAX_STRESS_DIRS=/pmem0/wasmtime-dcpmm,/pmem1/wasmtime-dcpmm WASMTIME_DAX_STRESS_BYTES=1GiB cargo test -p wasmtime --release --lib runtime::transaction::persist::tests::dax_pmem_fsdax_persistent_data_stress -- --ignored --exact --nocapture'
```

Expected: both regions complete, object recovery prints throughput, and generated files are removed unless `WASMTIME_DAX_STRESS_KEEP_FILES=1`.

- [ ] **Step 3: Run the 256GiB release regression**

Run:

```bash
ssh shisoft@192.168.10.74 'cd /home/shisoft/wasmtime-dax-pmem-test && WASMTIME_TEST_REAL_PMEM=1 WASMTIME_DAX_STRESS_DIRS=/pmem0/wasmtime-dcpmm,/pmem1/wasmtime-dcpmm WASMTIME_DAX_STRESS_BYTES=256GiB cargo test -p wasmtime --release --lib runtime::transaction::persist::tests::dax_pmem_fsdax_persistent_data_stress -- --ignored --exact --nocapture'
```

Expected: the process is not killed by the kernel, RSS remains bounded by recovery metadata rather than object payload bytes, and recovery throughput is printed for both `/pmem0` and `/pmem1`.

- [ ] **Step 4: Clean real-PMEM artifacts if the run is interrupted**

Run:

```bash
ssh shisoft@192.168.10.74 'find /pmem0/wasmtime-dcpmm /pmem1/wasmtime-dcpmm -maxdepth 1 -type f \( -name "wasmtime-dax-stress-*" -o -name "dax-pmem-stress-*" \) -delete'
```

Expected: `df -h /pmem0 /pmem1` shows both filesystems back near their pre-test free space.

### Task 7: Final verification

**Files:**
- No source edits expected.

- [ ] **Step 1: Run formatting and whitespace checks**

Run:

```bash
cargo fmt --check
git diff --check
```

Expected: both pass.

- [ ] **Step 2: Run focused local test suites**

Run:

```bash
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo test -p wasmtime --lib tmemory -- --format terse
cargo test -p wasmtime --lib dax_pmem -- --format terse
```

Expected: all non-ignored tests pass.

- [ ] **Step 3: Run library check**

Run:

```bash
cargo check -p wasmtime --lib
```

Expected: check exits 0. Existing warnings unrelated to this change can remain, but new warnings from touched files should be fixed.

- [ ] **Step 4: Commit**

Run:

```bash
git status --short
git add crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs crates/wasmtime/src/runtime/transaction/object_heap.rs crates/wasmtime/src/runtime/transaction/object_table.rs crates/wasmtime/src/runtime/transaction/object_gc.rs crates/wasmtime/src/runtime/transaction/persist.rs crates/wasmtime/src/runtime/transaction/state.rs crates/wasmtime/src/runtime/transaction/tests.rs crates/wasmtime/src/runtime/vm/libcalls.rs crates/wasmtime/tests/transaction_persistence.rs docs/superpowers/plans/2026-06-21-dax-pmem-stress-test.md docs/superpowers/plans/2026-06-21-zero-copy-recovered-object-records.md
git commit -m "Avoid copying recovered persistent object records"
```

Expected: commit succeeds without any `Co-authored-by` trailer.

---

## Self-Review

**Spec coverage:** The plan removes eager `Vec<u8>` object record storage from recovery, adds mapped-byte access to both file-backed and DAX backends through the shared block-region view, fuses object winner, tmemory size, and root-id classification into one post-log replay pass, routes object-table rebuild through mapped bytes, preserves explicit test helper copies only at exported test boundaries, and verifies the 256GiB PMEM stress case.

**Placeholder scan:** The plan contains concrete file paths, method names, commands, and expected outcomes. It does not rely on unspecified error handling or unnamed tests.

**Type consistency:** The same `RecoveredObjectWinner::with_view_record_bytes`, `RecoveredObjectWinner::with_source_record_bytes`, `BlockRegionBackendView::with_mapped_slice`, and mapped rebuild method names are used across recovery, object table, stress validation, and runtime call-site tasks.
