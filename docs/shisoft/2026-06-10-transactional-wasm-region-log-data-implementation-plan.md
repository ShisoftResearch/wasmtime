# Transactional Wasm Region/Log/Data Persistence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the first real durable region/log/data substrate for transactional Wasmtime: block-derived `RegionHeader` metadata, fixed-size CRC32-protected `TxLogEntry`s, append-only data records, commit-time `LP` publication, and recovery that rebuilds streams by scanning chunk-start blocks.

**Architecture:** Reuse the current Wizard-style block-region foundation and layer a persistent publication path on top of it. The first durable wave writes fixed-size log entries into single-block log chunks and variable-size data records into size-classed data chunks; transaction commit publishes data first, then ordinary log entries, then one final `LP` entry. Recovery discovers streams by scanning chunk-start blocks, replays each stream independently, and merges winners by `PackedGranuleId` plus `version`.

**Tech Stack:** Rust, Wasmtime runtime internals, existing `tmemory` block-region code, `crc32fast`, `mmap`/`msync`/`sync_data` for file-backed durability tests, `cargo test`.

---

## Scope

This plan covers the persistence substrate only:

- durable header/entry codecs
- stream-scoped log/data allocation
- commit-time publication ordering
- granule/object payload encoding into data records
- recovery discovery and replay
- file-backed restart validation

This plan does not cover:

- reclaim/orphan cleanup policy
- delete/tombstone semantics
- GC-driven object reclamation
- persistent-index mode
- PMEM-specific `CLWB` instruction plumbing on real hardware

## File Structure

- `crates/wasmtime/Cargo.toml`
  - add the `crc32fast` dependency for log-entry CRC32.
- `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`
  - durable layout constants, `RegionHeader`, `LogBlockHeader`,
    `DataChunkHeader`, `TxLogEntry`, `TxDataRecordHeader`, and CRC helpers.
- `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
  - chunk-start discovery, stream-scoped block/chunk allocation, size-classed
    data-chunk dispatch, file-backed region reopen support.
- `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
  - stream discovery, log replay, winner merge, and `next_stream_id`
    reconstruction.
- `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
  - durable region exports, append helpers, and recovery entrypoints used by
    `TMemory`.
- `crates/wasmtime/src/runtime/transaction/persist.rs`
  - commit-time publication buffer, flush/fence ordering, and stream-local
    transaction-id allocation.
- `crates/wasmtime/src/runtime/transaction.rs`
  - wire the new persistence submodule into transaction commit/abort paths.
- `crates/wasmtime/src/runtime/transaction/object_heap.rs`
  - encode object payloads with `TxObjectHeader` plus `TxDataRecordHeader`.
- `crates/wasmtime/tests/transaction_persistence.rs`
  - file-backed restart and corruption-smoke integration tests.
- `docs/shisoft/transactional-wasm-implementation-log.md`
  - execution-time status update after the work lands.

## Task 1: Add Durable Header And Entry Codecs

**Files:**
- Modify: `crates/wasmtime/Cargo.toml`
- Create: `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`

- [ ] **Step 1: Write the failing codec/layout tests**

```rust
#[test]
fn durable_header_sizes_match_design() {
    assert_eq!(size_of::<RegionHeader>(), 32);
    assert_eq!(size_of::<LogBlockHeader>(), 20);
    assert_eq!(size_of::<DataChunkHeader>(), 28);
    assert_eq!(size_of::<TxLogEntry>(), 32);
    assert_eq!(align_of::<TxLogEntry>(), 32);
    assert_eq!(size_of::<TxDataRecordHeader>(), 24);
}

#[test]
fn tx_log_entry_crc_detects_bitflip() {
    let mut entry = TxLogEntry::new(
        0x6000_0000_0000_002a,
        7,
        (11 << 1) | 1,
        19,
        96,
    );
    entry.seal_crc32();
    assert!(entry.validate_crc32());
    entry.data_offset ^= 0x10;
    assert!(!entry.validate_crc32());
}

#[test]
fn tx_data_record_header_roundtrips() {
    let header = TxDataRecordHeader {
        logical_id: 0x1000_0000_0000_0007,
        version: 3,
        kind: PackedGranuleDomain::TMemory as u16,
        reserved: 0,
        payload_len: 256,
        type_info: 0,
    };
    let bytes = header.as_bytes();
    let decoded = TxDataRecordHeader::from_bytes(bytes).unwrap();
    assert_eq!(decoded, header);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:

```bash
cargo test -p wasmtime --lib durable_header_sizes_match_design
cargo test -p wasmtime --lib tx_log_entry_crc_detects_bitflip
cargo test -p wasmtime --lib tx_data_record_header_roundtrips
```

Expected: FAIL because `durable_log.rs`, the durable structs, and the CRC
helpers do not exist yet.

- [ ] **Step 3: Add the durable codec module**

Add `crc32fast` to `crates/wasmtime/Cargo.toml` and implement the codec types:

```rust
pub(crate) const NO_NEXT_BLOCK: u32 = u32::MAX;
pub(crate) const REGION_MAGIC: u32 = 0x5452_4547; // "TREG"
pub(crate) const LOG_BLOCK_MAGIC: u32 = 0x544c_4f47; // "TLOG"
pub(crate) const DATA_CHUNK_MAGIC: u32 = 0x5444_4154; // "TDAT"

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RegionHeader {
    pub(crate) magic: u32,
    pub(crate) block_size: u32,
    pub(crate) num_blocks: u32,
    pub(crate) block_table_start_block: u32,
    pub(crate) block_table_block_count: u32,
    pub(crate) metadata_descs_start_block: u32,
    pub(crate) metadata_descs_block_count: u32,
    pub(crate) num_descs: u32,
}

#[repr(C, align(32))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TxLogEntry {
    pub(crate) logical_id: u64,
    pub(crate) version: u32,
    pub(crate) tx_meta: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) crc32: u32,
    pub(crate) reserved: u32,
}

impl TxLogEntry {
    pub(crate) fn new(
        logical_id: u64,
        version: u32,
        tx_meta: u32,
        data_block: u32,
        data_offset: u32,
    ) -> Self {
        Self {
            logical_id,
            version,
            tx_meta,
            data_block,
            data_offset,
            crc32: 0,
            reserved: 0,
        }
    }

    pub(crate) fn seal_crc32(&mut self) {
        self.crc32 = crc32fast::hash(&self.bytes_without_crc32());
    }

    pub(crate) fn validate_crc32(&self) -> bool {
        self.crc32 == crc32fast::hash(&self.bytes_without_crc32())
    }
}
```

Export the module from `tmemory.rs`:

```rust
mod durable_log;
pub(crate) use durable_log::*;
```

- [ ] **Step 4: Run the codec tests**

Run:

```bash
cargo test -p wasmtime --lib durable_header_sizes_match_design
cargo test -p wasmtime --lib tx_log_entry_crc_detects_bitflip
cargo test -p wasmtime --lib tx_data_record_header_roundtrips
git diff --check
```

Expected: PASS for all three tests, and `git diff --check` reports no issues.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/Cargo.toml \
  crates/wasmtime/src/runtime/vm/memory/tmemory.rs \
  crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs
git commit -m "Add transactional durable log and data record codecs"
```

## Task 2: Add Stream-Scoped Log Blocks And Size-Classed Data Chunks

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`

- [ ] **Step 1: Write the failing allocator tests**

```rust
#[test]
fn allocates_single_block_log_chunks() {
    let mut region = VMemoryBlockRegion::new_for_test(64).unwrap();
    let first = region.alloc_log_block(3, 0).unwrap();
    let second = region.alloc_log_block(3, 1).unwrap();
    assert_ne!(first, second);
    assert_eq!(region.log_block_header(first).unwrap().block_seq, 0);
    assert_eq!(region.log_block_header(second).unwrap().block_seq, 1);
}

#[test]
fn dispatches_small_medium_and_large_data_chunks() {
    let mut region = VMemoryBlockRegion::new_for_test(128).unwrap();
    let small = region.alloc_data_chunk(7, 0, SMALL_DATA_RECORD_BYTES).unwrap();
    let medium = region.alloc_data_chunk(7, 1, MEDIUM_DATA_RECORD_BYTES).unwrap();
    let large = region.alloc_data_chunk(7, 2, LARGE_DATA_RECORD_BYTES).unwrap();

    assert_eq!(region.data_chunk_header(small).unwrap().chunk_blocks, 1);
    assert_eq!(region.data_chunk_header(medium).unwrap().chunk_blocks, 1);
    assert!(region.data_chunk_header(large).unwrap().chunk_blocks > 1);
}

#[test]
fn data_record_never_crosses_chunk_boundary() {
    let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
    let stream = region.alloc_stream(9).unwrap();
    let first = region.append_data_record(stream, &vec![0u8; MEDIUM_DATA_RECORD_BYTES]).unwrap();
    let second = region.append_data_record(stream, &vec![0u8; MEDIUM_DATA_RECORD_BYTES]).unwrap();
    assert_ne!(first.chunk_start_block, second.chunk_start_block);
}
```

- [ ] **Step 2: Run the allocator tests to verify they fail**

Run:

```bash
cargo test -p wasmtime --lib allocates_single_block_log_chunks -- --exact
cargo test -p wasmtime --lib dispatches_small_medium_and_large_data_chunks -- --exact
cargo test -p wasmtime --lib data_record_never_crosses_chunk_boundary -- --exact
```

Expected: FAIL because stream-scoped log/data allocation APIs do not exist yet.

- [ ] **Step 3: Add the log/data allocation APIs**

Implement a small stream cursor and size-classed data dispatch:

```rust
pub(crate) const SMALL_DATA_LIMIT: usize = IMMIX_LINE_SIZE as usize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DataChunkClass {
    Small,
    Medium,
    Large,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DataRecordLocation {
    pub(crate) chunk_start_block: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
}

impl VMemoryBlockRegion {
    pub(crate) fn alloc_log_block(&mut self, stream_id: u32, block_seq: u32) -> Result<u32> {
        // allocate exactly one chunk-start block and write LogBlockHeader
    }

    pub(crate) fn alloc_data_chunk(
        &mut self,
        stream_id: u32,
        chunk_seq: u32,
        total_bytes: usize,
    ) -> Result<u32> {
        let class = if total_bytes <= SMALL_DATA_LIMIT {
            DataChunkClass::Small
        } else if total_bytes <= self.block_payload_capacity() {
            DataChunkClass::Medium
        } else {
            DataChunkClass::Large
        };
        self.alloc_data_chunk_for_class(stream_id, chunk_seq, class, total_bytes)
    }
}
```

Keep the durable invariants explicit:

- log entries never span blocks
- data records never span chunk boundaries
- `chunk_blocks` is derived from required capacity for large records

- [ ] **Step 4: Run the allocator tests**

Run:

```bash
cargo test -p wasmtime --lib allocates_single_block_log_chunks -- --exact
cargo test -p wasmtime --lib dispatches_small_medium_and_large_data_chunks -- --exact
cargo test -p wasmtime --lib data_record_never_crosses_chunk_boundary -- --exact
git diff --check
```

Expected: PASS for all tests, and formatting is clean.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory.rs \
  crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
git commit -m "Add transactional stream log blocks and data chunk allocation"
```

## Task 3: Add Commit-Time Publication Buffers And LP Ordering

**Files:**
- Create: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`

- [ ] **Step 1: Write the failing publication-order tests**

```rust
#[test]
fn commit_publishes_only_the_last_entry_with_lp() {
    let mut recorder = RecordingDurability::default();
    let mut publisher = StreamPublisher::new_for_test(&mut recorder, 4, 21);
    publisher.publish_transaction(&sample_publications()).unwrap();

    assert_eq!(recorder.final_lp_count(), 1);
    assert!(recorder.non_final_log_entries_before_final_lp());
}

#[test]
fn commit_orders_data_before_final_lp() {
    let mut recorder = RecordingDurability::default();
    let mut publisher = StreamPublisher::new_for_test(&mut recorder, 4, 99);
    publisher.publish_transaction(&sample_publications()).unwrap();

    assert!(recorder.events().starts_with(&[
        DurabilityEvent::DataWrite,
        DurabilityEvent::DataFlush,
        DurabilityEvent::Fence,
    ]));
    assert_eq!(recorder.events().last(), Some(&DurabilityEvent::Fence));
}

#[test]
fn abort_does_not_publish_lp() {
    let mut recorder = RecordingDurability::default();
    let mut publisher = StreamPublisher::new_for_test(&mut recorder, 5, 12);
    publisher.abort_transaction(&sample_publications()).unwrap();
    assert_eq!(recorder.final_lp_count(), 0);
}
```

- [ ] **Step 2: Run the publication tests to verify they fail**

Run:

```bash
cargo test -p wasmtime --lib commit_publishes_only_the_last_entry_with_lp -- --exact
cargo test -p wasmtime --lib commit_orders_data_before_final_lp -- --exact
cargo test -p wasmtime --lib abort_does_not_publish_lp -- --exact
```

Expected: FAIL because the persistence submodule and publication path do not
exist yet.

- [ ] **Step 3: Implement the publication buffer and publisher**

Create `persist.rs` with the transaction-local publication shape:

```rust
#[derive(Debug, Clone)]
pub(crate) struct PendingPublication {
    pub(crate) logical_id: u64,
    pub(crate) version: u32,
    pub(crate) kind: u16,
    pub(crate) type_info: u32,
    pub(crate) payload: Vec<u8>,
}

pub(crate) trait DurableSink {
    fn append_data_record(&mut self, record: &[u8]) -> Result<DataRecordLocation>;
    fn append_log_entry(&mut self, entry: &TxLogEntry) -> Result<()>;
    fn flush_data(&mut self) -> Result<()>;
    fn flush_log(&mut self) -> Result<()>;
    fn fence(&mut self) -> Result<()>;
}

pub(crate) struct StreamPublisher<'a, S> {
    sink: &'a mut S,
    stream_id: u32,
    txid: u32,
}
```

Implement `publish_transaction` with the exact phase-1 ordering:

```rust
fn publish_transaction(&mut self, pubs: &[PendingPublication]) -> Result<()> {
    let mut ordinary = Vec::new();
    for pub_ in pubs {
        let record = encode_data_record(pub_)?;
        let location = self.sink.append_data_record(&record)?;
        ordinary.push(TxLogEntry::new(
            pub_.logical_id,
            pub_.version,
            self.txid << 1,
            location.data_block,
            location.data_offset,
        ));
    }

    self.sink.flush_data()?;
    self.sink.fence()?;

    for entry in ordinary.iter().take(ordinary.len().saturating_sub(1)) {
        self.sink.append_log_entry(entry)?;
    }
    self.sink.flush_log()?;

    if let Some(last) = ordinary.last_mut() {
        last.tx_meta |= 1;
        last.seal_crc32();
        self.sink.append_log_entry(last)?;
        self.sink.flush_log()?;
        self.sink.fence()?;
    }
    Ok(())
}
```

Wire the new submodule through `transaction.rs` and expose a `TMemory` durable
sink implementation from `tmemory.rs`.

- [ ] **Step 4: Run the publication-order tests**

Run:

```bash
cargo test -p wasmtime --lib commit_publishes_only_the_last_entry_with_lp -- --exact
cargo test -p wasmtime --lib commit_orders_data_before_final_lp -- --exact
cargo test -p wasmtime --lib abort_does_not_publish_lp -- --exact
git diff --check
```

Expected: PASS for all tests, and the publisher ordering matches the design.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction.rs \
  crates/wasmtime/src/runtime/transaction/persist.rs \
  crates/wasmtime/src/runtime/vm/memory/tmemory.rs
git commit -m "Add transactional durable publication ordering"
```

## Task 4: Encode Granule And Object Payloads As Data Records

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_heap.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`

- [ ] **Step 1: Write the failing payload-encoding tests**

```rust
#[test]
fn encodes_tmemory_granule_record() {
    let publication = PendingPublication::tmemory_for_test(0x1000_0000_0000_0001, 5, &[1, 2, 3, 4]);
    let bytes = encode_data_record(&publication).unwrap();
    let header = TxDataRecordHeader::read_from_prefix(&bytes).unwrap();
    assert_eq!(header.kind, PackedGranuleDomain::TMemory as u16);
    assert_eq!(header.version, 5);
    assert_eq!(header.payload_len, 4);
}

#[test]
fn encodes_object_record_with_tx_object_header() {
    let bytes = encode_object_record_for_test(41, 7, 12, &[9, 8, 7]).unwrap();
    let object = TxObjectHeader::read_from_prefix(&bytes).unwrap();
    assert_eq!(object.object_id, 41);
    assert_eq!(object.version, 7);
    assert_eq!(object.type_index, 12);
}
```

- [ ] **Step 2: Run the payload-encoding tests to verify they fail**

Run:

```bash
cargo test -p wasmtime --lib encodes_tmemory_granule_record -- --exact
cargo test -p wasmtime --lib encodes_object_record_with_tx_object_header -- --exact
```

Expected: FAIL because the shared data-record encoder and object-record encoder
do not exist yet.

- [ ] **Step 3: Add the encoders**

Implement a shared encoder for non-object granules:

```rust
pub(crate) fn encode_data_record(pub_: &PendingPublication) -> Result<Vec<u8>> {
    let header = TxDataRecordHeader {
        logical_id: pub_.logical_id,
        version: pub_.version,
        kind: pub_.kind,
        reserved: 0,
        payload_len: pub_.payload.len().try_into()?,
        type_info: pub_.type_info,
    };

    let mut bytes = Vec::with_capacity(size_of::<TxDataRecordHeader>() + pub_.payload.len());
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(&pub_.payload);
    Ok(bytes)
}
```

Implement object payload encoding in `object_heap.rs`:

```rust
pub(crate) fn encode_object_record(
    object_id: u64,
    version: u32,
    kind: u16,
    type_index: u32,
    payload: &[u8],
) -> Result<Vec<u8>> {
    let header = TxObjectHeader {
        record_len: (size_of::<TxObjectHeader>() + payload.len()) as u64,
        object_id,
        version,
        kind,
        flags: 0,
        type_index,
    };
    let mut bytes = Vec::with_capacity(header.record_len as usize);
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(payload);
    Ok(bytes)
}
```

Use the shared `PendingPublication` path so `tmemory`, `tglobal`, `ttable`,
and object-heap publication all emit through the same durable publisher.

- [ ] **Step 4: Run the payload-encoding tests**

Run:

```bash
cargo test -p wasmtime --lib encodes_tmemory_granule_record -- --exact
cargo test -p wasmtime --lib encodes_object_record_with_tx_object_header -- --exact
git diff --check
```

Expected: PASS for both tests, and `version` remains `u32` end-to-end.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/object_heap.rs \
  crates/wasmtime/src/runtime/transaction/persist.rs \
  crates/wasmtime/src/runtime/vm/memory/tmemory.rs
git commit -m "Encode transactional granule and object data records"
```

## Task 5: Add Recovery Discovery And Stream Replay

**Files:**
- Create: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`

- [ ] **Step 1: Write the failing recovery tests**

```rust
#[test]
fn recovery_discovers_streams_from_chunk_starts() {
    let region = sample_region_with_two_streams();
    let recovered = recover_region_for_test(&region).unwrap();
    assert_eq!(recovered.streams.len(), 2);
    assert_eq!(recovered.next_stream_id, 3);
}

#[test]
fn recovery_discards_loose_end_without_lp() {
    let region = sample_region_with_trailing_uncommitted_tx();
    let recovered = recover_region_for_test(&region).unwrap();
    assert_eq!(recovered.winners.len(), 1);
    assert_eq!(recovered.winners[0].version, 1);
}

#[test]
fn recovery_picks_highest_version_per_logical_id() {
    let region = sample_region_with_competing_stream_updates();
    let recovered = recover_region_for_test(&region).unwrap();
    assert_eq!(recovered.winners[0].version, 9);
}
```

- [ ] **Step 2: Run the recovery tests to verify they fail**

Run:

```bash
cargo test -p wasmtime --lib recovery_discovers_streams_from_chunk_starts -- --exact
cargo test -p wasmtime --lib recovery_discards_loose_end_without_lp -- --exact
cargo test -p wasmtime --lib recovery_picks_highest_version_per_logical_id -- --exact
```

Expected: FAIL because the recovery module and replay helpers do not exist yet.

- [ ] **Step 3: Implement discovery and replay**

Add the recovery types:

```rust
#[derive(Debug)]
pub(crate) struct RecoveredStream {
    pub(crate) stream_id: u32,
    pub(crate) log_blocks: Vec<u32>,
    pub(crate) data_chunks: Vec<u32>,
}

#[derive(Debug)]
pub(crate) struct RecoveryWinner {
    pub(crate) logical_id: u64,
    pub(crate) version: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
}

#[derive(Debug)]
pub(crate) struct RecoveredRegion {
    pub(crate) streams: Vec<RecoveredStream>,
    pub(crate) winners: Vec<RecoveryWinner>,
    pub(crate) next_stream_id: u32,
}
```

Implement the recovery flow:

```rust
pub(crate) fn recover_region(region: &BlockRegionBackendView<'_>) -> Result<RecoveredRegion> {
    let streams = discover_streams(region)?;
    let mut winners = BTreeMap::<u64, RecoveryWinner>::new();

    for stream in streams.iter() {
        for update in replay_stream(region, stream)? {
            match winners.get(&update.logical_id) {
                Some(current) if current.version > update.version => {}
                Some(current) if current.version == update.version => {
                    bail!("duplicate committed version for logical id {}", update.logical_id);
                }
                _ => {
                    winners.insert(update.logical_id, update);
                }
            }
        }
    }

    Ok(RecoveredRegion {
        next_stream_id: streams.iter().map(|s| s.stream_id).max().unwrap_or(0) + 1,
        streams,
        winners: winners.into_values().collect(),
    })
}
```

Keep the first driver deterministic and sequential. Structure `replay_stream`
as a pure helper so a later follow-up can parallelize stream replay with scoped
threads without redesigning the API.

- [ ] **Step 4: Run the recovery tests**

Run:

```bash
cargo test -p wasmtime --lib recovery_discovers_streams_from_chunk_starts -- --exact
cargo test -p wasmtime --lib recovery_discards_loose_end_without_lp -- --exact
cargo test -p wasmtime --lib recovery_picks_highest_version_per_logical_id -- --exact
git diff --check
```

Expected: PASS for all three tests, and recovery behavior matches the design.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory.rs \
  crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs \
  crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs
git commit -m "Add transactional recovery discovery and replay"
```

## Task 6: Add File-Backed Restart Smoke Tests

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Create: `crates/wasmtime/tests/transaction_persistence.rs`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Write the failing file-backed integration tests**

```rust
#[test]
fn file_backed_region_recovers_committed_tmemory_update() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tmemory-region.bin");

    {
        let mut region = FileBackedBlockRegion::create_for_test(&path, 128).unwrap();
        let mut tx = TestTransaction::new(&mut region, 1, 1);
        tx.publish_memory_granule(0x1000_0000_0000_0001, 1, &[1, 2, 3, 4]).unwrap();
        tx.commit().unwrap();
    }

    let reopened = FileBackedBlockRegion::open_for_test(&path).unwrap();
    let recovered = recover_region_for_test(&reopened).unwrap();
    assert_eq!(recovered.winners.len(), 1);
    assert_eq!(recovered.winners[0].version, 1);
}

#[test]
fn file_backed_region_rejects_corrupted_log_crc() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt-region.bin");
    let mut region = make_region_with_one_committed_entry(&path).unwrap();
    corrupt_log_crc_for_test(&mut region).unwrap();
    let reopened = FileBackedBlockRegion::open_for_test(&path).unwrap();
    let recovered = recover_region_for_test(&reopened).unwrap();
    assert!(recovered.winners.is_empty());
}
```

- [ ] **Step 2: Run the integration tests to verify they fail**

Run:

```bash
cargo test -p wasmtime --test transaction_persistence file_backed_region_recovers_committed_tmemory_update -- --exact
cargo test -p wasmtime --test transaction_persistence file_backed_region_rejects_corrupted_log_crc -- --exact
```

Expected: FAIL because the file-backed reopen path and recovery integration are
not wired yet.

- [ ] **Step 3: Wire file-backed reopen support**

Extend the file-backed region backend so restart can reopen the existing mapped
image, read `RegionHeader`, and expose the same `BlockRegionBackendView` used
by `recover_region`:

```rust
impl FileBackedBlockRegion {
    pub(crate) fn create_for_test(path: &Path, blocks: u32) -> Result<Self> {
        // initialize RegionHeader, metadata blocks, and first free block range
    }

    pub(crate) fn open_for_test(path: &Path) -> Result<Self> {
        // map existing file, validate RegionHeader, and rebuild an in-memory view
    }
}
```

Update `transactional-wasm-implementation-log.md` when the tests pass:

```markdown
- Durable region/log/data foundation now reopens `FileBackedMemory` regions and
  rebuilds stream state through chunk-start scanning plus log replay.
```

- [ ] **Step 4: Run the integration tests and focused unit suite**

Run:

```bash
cargo test -p wasmtime --test transaction_persistence file_backed_region_recovers_committed_tmemory_update -- --exact
cargo test -p wasmtime --test transaction_persistence file_backed_region_rejects_corrupted_log_crc -- --exact
cargo test -p wasmtime --lib durable_header_sizes_match_design
cargo test -p wasmtime --lib recovery_picks_highest_version_per_logical_id -- --exact
git diff --check
```

Expected: PASS for both integration tests and the focused unit checks.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs \
  crates/wasmtime/tests/transaction_persistence.rs \
  docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Add file-backed transactional persistence restart tests"
```

## Plan Review

- The plan covers the full durable substrate discussed in the design session:
  header codecs, stream/block allocation, log/data publication order,
  granule/object record encoding, recovery, and file-backed restart tests.
- The plan intentionally excludes reclaim/orphan cleanup, deletion, GC-driven
  reuse, and persistent-index mode because those were explicitly deferred.
- `version` is `u32` in every new code snippet. `LP` is always carried in
  `tx_meta` bit 0. `TxDataRecordHeader.kind` mirrors `PackedGranuleId` domain
  numbering in every task that emits or decodes durable data.
