# Durable Block Reuse Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Add durable block/chunk reuse for transactional persistent storage without object-level compaction or line-level reuse.

**Architecture:** Keep the Zen-style fixed `TxLogEntry` at 32 bytes and add a durable block metadata table that carries state, kind, and generation per region block. Recovery accepts a log entry only when the pointed data block is still an active data block of the expected kind and its durable generation matches the generation packed into the log entry. Whole-dead object chunks and completed linear-undo chunks can then be retired and reused without resurrecting stale records.

**Tech Stack:** Rust, Wasmtime runtime internals, `FileBackedMemoryBlockRegion`, `TxDurableLog`, persistent object recovery reports, file-backed restart tests, `cargo test`.

---

## Scope

This plan implements the coarse block/chunk reuse milestone needed for an
operational persistent transactional object system that can reuse whole dead
storage chunks. It does not implement mixed-block copying cleanup, line-level
Immix reuse inside active chunks, `ObjectId` reuse, compacting persistent GC, or
persistent object table storage.

The successful end state is:

- `TxLogEntry` stays exactly 32 bytes and 32-byte aligned.
- File-backed region images include a durable block metadata table.
- New log entries encode the generation of their pointed data block.
- Recovery ignores stale log entries whose data block was retired or reused.
- Whole-dead object-data chunks can be retired and allocated again.
- Completed linear-memory undo chunks can be retired and allocated again.
- Old log blocks may remain append-only; stale entries are made harmless by
  data-block state and generation validation.

## File Map

- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`
  - Add `TxEntryMeta`, `BlockState`, `BlockKind`, and `BlockMeta` codecs.
  - Repack `TxLogEntry.entry_meta` as low role bits plus high generation bits.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/metadata.rs`
  - Replace fixed metadata block constants with helpers that derive block-table,
    descriptor-table, and type-layout block placement from `num_blocks`.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
  - Store/load durable `BlockMeta` for file-backed regions.
  - Track in-memory block metadata for `VMemoryBlockRegion` and `NVMemoryBlockRegion`
    so existing tests can exercise the same API with generation zero.
  - Allocate chunks by kind, retire chunks, reuse retired chunks with incremented
    generations, and expose block-generation lookup.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
  - Discover only active/sealed log/data blocks.
  - Validate data block state, kind, and generation before replaying an entry.
  - Keep loose-end undo rollback behavior unchanged for non-retired undo chunks.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
  - Thread data-block generation into `TMemory::publication_log_entry`.
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
  - Return `DurableDataRecordPointer` from durable sinks and include generation
    in `PendingCommitLogEntry`.
  - Retire completed linear-undo chunks after LP publication.
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
  - Add a narrow test-only helper that feeds `PersistentRecoveryGcReport` record
    locations into file-backed object chunk retirement.
- Modify: `crates/wasmtime/tests/transaction_persistence.rs`
  - Add restart tests for stale-generation rejection, object chunk reuse, and
    linear-undo chunk reuse.
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
  - Keep the block metadata and generation validation section synchronized with
    the implemented field names.
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
  - Record the new durable block reuse milestone and limitations.

## Invariants

- `BlockMeta.generation` is a durable per-block generation, not an object
  version and not a transaction version.
- All blocks in one allocated data chunk carry the same `generation`,
  `kind`, `chunk_start`, and `chunk_blocks`.
- A data log entry is valid only if:
  - the entry CRC is valid,
  - the transaction is committed or the entry is being used for loose-end undo,
  - the target block has state `Active` or `Sealed`,
  - the target block kind matches the log entry role,
  - `entry.data_block_generation() == block_meta.generation`.
- Retiring a chunk changes its blocks to `Retired`. Reusing a retired/free chunk
  increments each block generation before writing new chunk bytes.
- Generation wrap at `2^30 - 1` is a hard error for that block. The runtime must
  stop reusing the block until a future checkpoint/rebuild feature renews the
  generation namespace.

## Task 1: Repack `TxLogEntry` Metadata

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`
- Test: `cargo test -p wasmtime --lib tx_log_entry -- --format terse`

- [x] **Step 1: Add failing metadata packing tests**

Add these tests to the existing `#[cfg(test)] mod tests` in
`durable_log.rs`:

```rust
#[test]
fn tx_log_entry_meta_roundtrips_role_and_generation() {
    let mut entry = TxLogEntry::new(0x1000_0000_0000_0003, 7, 11 << 1, 4, 32);

    assert_eq!(entry.role().unwrap(), TxLogEntryRole::TObjectPub);
    assert_eq!(entry.data_block_generation(), 0);

    entry.set_role(TxLogEntryRole::TMemoryUndo);
    entry.set_data_block_generation(17).unwrap();
    entry.seal_crc32();

    assert_eq!(entry.role().unwrap(), TxLogEntryRole::TMemoryUndo);
    assert_eq!(entry.data_block_generation(), 17);
    assert!(entry.validate_crc32());
}

#[test]
fn tx_log_entry_meta_rejects_generation_overflow() {
    let mut entry = TxLogEntry::new(0x1000_0000_0000_0003, 7, 11 << 1, 4, 32);

    let err = entry
        .set_data_block_generation(TxEntryMeta::MAX_DATA_BLOCK_GENERATION + 1)
        .unwrap_err()
        .to_string();

    assert!(err.contains("data block generation exceeds log entry capacity"));
}
```

- [x] **Step 2: Run tests and verify they fail**

Run:

```bash
cargo test -p wasmtime --lib tx_log_entry_meta -- --format terse
```

Expected: fail because `TxEntryMeta` and `data_block_generation` do not exist.

- [x] **Step 3: Implement `TxEntryMeta`**

In `durable_log.rs`, add this helper near `TxLogEntryRole`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TxEntryMeta(u32);

impl TxEntryMeta {
    pub(crate) const ROLE_BITS: u32 = 0b11;
    pub(crate) const GENERATION_SHIFT: u32 = 2;
    pub(crate) const MAX_DATA_BLOCK_GENERATION: u32 = (1 << 30) - 1;

    pub(crate) fn new(role: TxLogEntryRole, data_block_generation: u32) -> Result<Self> {
        ensure!(
            data_block_generation <= Self::MAX_DATA_BLOCK_GENERATION,
            "data block generation exceeds log entry capacity"
        );
        Ok(Self(role.to_bits() | (data_block_generation << Self::GENERATION_SHIFT)))
    }

    pub(crate) fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    pub(crate) fn bits(self) -> u32 {
        self.0
    }

    pub(crate) fn role(self) -> Result<TxLogEntryRole> {
        TxLogEntryRole::from_bits(self.0 & Self::ROLE_BITS)
    }

    pub(crate) fn data_block_generation(self) -> u32 {
        self.0 >> Self::GENERATION_SHIFT
    }

    pub(crate) fn with_role(self, role: TxLogEntryRole) -> Self {
        Self((self.0 & !Self::ROLE_BITS) | role.to_bits())
    }
}
```

Update `TxLogEntryRole` with bit conversions:

```rust
impl TxLogEntryRole {
    fn to_bits(self) -> u32 {
        match self {
            TxLogEntryRole::TObjectPub => 0,
            TxLogEntryRole::TMemoryUndo => 1,
        }
    }

    fn from_bits(bits: u32) -> Result<Self> {
        Ok(match bits {
            0 => TxLogEntryRole::TObjectPub,
            1 => TxLogEntryRole::TMemoryUndo,
            role => bail!("unknown transaction log entry role {role}"),
        })
    }
}
```

- [x] **Step 4: Rename the on-disk field**

Rename `TxLogEntry.reserved` to `entry_meta` in `durable_log.rs` and all
decoders/encoders:

```rust
pub(crate) struct TxLogEntry {
    pub(crate) logical_id: u64,
    pub(crate) version: u32,
    pub(crate) tx_meta: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) crc32: u32,
    pub(crate) entry_meta: u32,
}
```

Update `TxLogEntry` methods:

```rust
pub(crate) fn role(&self) -> Result<TxLogEntryRole> {
    TxEntryMeta::from_bits(self.entry_meta).role()
}

pub(crate) fn set_role(&mut self, role: TxLogEntryRole) {
    self.entry_meta = TxEntryMeta::from_bits(self.entry_meta)
        .with_role(role)
        .bits();
}

pub(crate) fn data_block_generation(&self) -> u32 {
    TxEntryMeta::from_bits(self.entry_meta).data_block_generation()
}

pub(crate) fn set_data_block_generation(&mut self, generation: u32) -> Result<()> {
    let role = self.role()?;
    self.entry_meta = TxEntryMeta::new(role, generation)?.bits();
    Ok(())
}
```

In `TxLogEntry::new`, initialize:

```rust
entry_meta: TxEntryMeta::new(TxLogEntryRole::TObjectPub, 0)
    .expect("generation zero fits in transaction log entry")
    .bits(),
```

In `bytes_without_crc32`, write `entry_meta` into bytes `24..28`.

- [x] **Step 5: Run tests**

Run:

```bash
cargo test -p wasmtime --lib tx_log_entry -- --format terse
```

Expected: pass, including `tx_log_entry_fits_half_cache_line`.

- [x] **Step 6: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs
git commit -m "Pack durable transaction log entry metadata"
```

## Task 2: Add Durable Block Metadata Codecs

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`
- Test: `cargo test -p wasmtime --lib block_meta -- --format terse`

- [x] **Step 1: Add failing codec tests**

Add these tests to `durable_log.rs`:

```rust
#[test]
fn block_meta_roundtrips_active_object_data_chunk() {
    let meta = BlockMeta {
        state: BlockState::Active as u8,
        kind: BlockKind::ObjectData as u8,
        reserved0: 0,
        generation: 9,
        owner_thread: 7,
        chunk_start: 11,
        chunk_blocks: 3,
        reserved1: 0,
    };

    let decoded = BlockMeta::from_bytes(&meta.as_bytes()).unwrap();

    assert_eq!(decoded.state().unwrap(), BlockState::Active);
    assert_eq!(decoded.kind().unwrap(), BlockKind::ObjectData);
    assert_eq!(decoded.generation, 9);
    assert_eq!(decoded.owner_thread, 7);
    assert_eq!(decoded.chunk_start, 11);
    assert_eq!(decoded.chunk_blocks, 3);
}

#[test]
fn block_meta_rejects_unknown_state() {
    let mut meta = BlockMeta::free();
    meta.state = 0xff;

    let err = BlockMeta::from_bytes(&meta.as_bytes())
        .unwrap_err()
        .to_string();

    assert!(err.contains("unknown durable block state"));
}
```

- [x] **Step 2: Run tests and verify they fail**

Run:

```bash
cargo test -p wasmtime --lib block_meta -- --format terse
```

Expected: fail because `BlockMeta`, `BlockState`, and `BlockKind` do not exist.

- [x] **Step 3: Add block metadata types**

Add these types to `durable_log.rs`:

```rust
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlockState {
    Free = 0,
    Active = 1,
    Sealed = 2,
    Retired = 3,
}

impl BlockState {
    pub(crate) fn from_byte(byte: u8) -> Result<Self> {
        Ok(match byte {
            0 => BlockState::Free,
            1 => BlockState::Active,
            2 => BlockState::Sealed,
            3 => BlockState::Retired,
            state => bail!("unknown durable block state {state}"),
        })
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlockKind {
    Free = 0,
    Log = 1,
    ObjectData = 2,
    LinearUndo = 3,
    Metadata = 4,
}

impl BlockKind {
    pub(crate) fn from_byte(byte: u8) -> Result<Self> {
        Ok(match byte {
            0 => BlockKind::Free,
            1 => BlockKind::Log,
            2 => BlockKind::ObjectData,
            3 => BlockKind::LinearUndo,
            4 => BlockKind::Metadata,
            kind => bail!("unknown durable block kind {kind}"),
        })
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BlockMeta {
    pub(crate) state: u8,
    pub(crate) kind: u8,
    pub(crate) reserved0: u16,
    pub(crate) generation: u32,
    pub(crate) owner_thread: u32,
    pub(crate) chunk_start: u32,
    pub(crate) chunk_blocks: u32,
    pub(crate) reserved1: u64,
}

impl Default for BlockMeta {
    fn default() -> Self {
        Self::free()
    }
}

impl BlockMeta {
    pub(crate) const BYTE_LEN: usize = 32;

    pub(crate) fn free() -> Self {
        Self {
            state: BlockState::Free as u8,
            kind: BlockKind::Free as u8,
            reserved0: 0,
            generation: 0,
            owner_thread: 0,
            chunk_start: NO_NEXT_BLOCK,
            chunk_blocks: 0,
            reserved1: 0,
        }
    }

    pub(crate) fn active(
        kind: BlockKind,
        generation: u32,
        owner_thread: u32,
        chunk_start: u32,
        chunk_blocks: u32,
    ) -> Self {
        Self {
            state: BlockState::Active as u8,
            kind: kind as u8,
            reserved0: 0,
            generation,
            owner_thread,
            chunk_start,
            chunk_blocks,
            reserved1: 0,
        }
    }

    pub(crate) fn state(&self) -> Result<BlockState> {
        BlockState::from_byte(self.state)
    }

    pub(crate) fn kind(&self) -> Result<BlockKind> {
        BlockKind::from_byte(self.kind)
    }

    pub(crate) fn is_active_or_sealed(&self) -> Result<bool> {
        Ok(matches!(self.state()?, BlockState::Active | BlockState::Sealed))
    }

    pub(crate) fn as_bytes(&self) -> [u8; Self::BYTE_LEN] {
        let mut bytes = [0u8; Self::BYTE_LEN];
        bytes[0] = self.state;
        bytes[1] = self.kind;
        bytes[2..4].copy_from_slice(&self.reserved0.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.generation.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.owner_thread.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.chunk_start.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.chunk_blocks.to_le_bytes());
        bytes[20..28].copy_from_slice(&self.reserved1.to_le_bytes());
        bytes
    }

    pub(crate) fn from_bytes(bytes: impl AsRef<[u8]>) -> Result<Self> {
        let bytes = bytes.as_ref();
        ensure!(
            bytes.len() == Self::BYTE_LEN,
            "durable block metadata length mismatch"
        );
        let meta = Self {
            state: bytes[0],
            kind: bytes[1],
            reserved0: u16::from_le_bytes(bytes[2..4].try_into().unwrap()),
            generation: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            owner_thread: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            chunk_start: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
            chunk_blocks: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            reserved1: u64::from_le_bytes(bytes[20..28].try_into().unwrap()),
        };
        ensure!(meta.reserved0 == 0, "durable block metadata reserved0 must be zero");
        ensure!(meta.reserved1 == 0, "durable block metadata reserved1 must be zero");
        meta.state()?;
        meta.kind()?;
        Ok(meta)
    }
}
```

- [x] **Step 4: Run tests**

Run:

```bash
cargo test -p wasmtime --lib block_meta -- --format terse
```

Expected: pass.

- [x] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs
git commit -m "Add durable block metadata codecs"
```

## Task 3: Lay Out The Durable Block Metadata Table

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/metadata.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Test: `cargo test -p wasmtime --lib file_backed_region_persists_block_metadata -- --format terse`

- [x] **Step 1: Add failing file-backed metadata tests**

Add this test in `block_region.rs` tests:

```rust
#[test]
fn file_backed_region_persists_block_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("block-meta-region.bin");

    {
        let region = FileBackedMemoryBlockRegion::create_for_test(&path, 32).unwrap();
        assert_eq!(
            region.block_meta_for_test(0).unwrap().kind().unwrap(),
            BlockKind::Metadata
        );
        assert_eq!(
            region.block_meta_for_test(1).unwrap().kind().unwrap(),
            BlockKind::Metadata
        );
    }

    let region = FileBackedMemoryBlockRegion::open_for_test(&path).unwrap();

    assert_eq!(
        region.block_meta_for_test(0).unwrap().kind().unwrap(),
        BlockKind::Metadata
    );
    assert_eq!(
        region.block_meta_for_test(1).unwrap().kind().unwrap(),
        BlockKind::Metadata
    );
    assert_eq!(region.block_meta_for_test(8).unwrap(), BlockMeta::free());
}
```

- [x] **Step 2: Run test and verify it fails**

Run:

```bash
cargo test -p wasmtime --lib file_backed_region_persists_block_metadata -- --format terse
```

Expected: fail because the region has no persisted block metadata table.

- [x] **Step 3: Replace fixed metadata placement helpers**

In `metadata.rs`, add dynamic helpers:

```rust
use super::durable_log::BlockMeta;

pub(crate) const REGION_HEADER_BLOCK: u32 = 0;
pub(crate) const BLOCK_TABLE_START_BLOCK: u32 = 1;
pub(crate) const METADATA_DESC_BLOCK_COUNT: u32 = 1;
pub(crate) const TYPE_LAYOUT_METADATA_BLOCK_COUNT: u32 = 1;
pub(crate) const TYPE_LAYOUT_METADATA_HEADER_LEN: usize = 4;

pub(crate) fn block_table_block_count(num_blocks: usize) -> Result<u32> {
    let bytes = num_blocks
        .checked_mul(BlockMeta::BYTE_LEN)
        .context("transactional block metadata table size overflow")?;
    let blocks = bytes.div_ceil(BLOCK_SIZE).max(1);
    u32::try_from(blocks).context("transactional block metadata table block count overflow")
}

pub(crate) fn metadata_desc_start_block(num_blocks: usize) -> Result<u32> {
    BLOCK_TABLE_START_BLOCK
        .checked_add(block_table_block_count(num_blocks)?)
        .context("transactional metadata descriptor start block overflow")
}

pub(crate) fn type_layout_metadata_start_block(num_blocks: usize) -> Result<u32> {
    metadata_desc_start_block(num_blocks)?
        .checked_add(METADATA_DESC_BLOCK_COUNT)
        .context("transactional type-layout metadata start block overflow")
}

pub(crate) fn reserved_metadata_blocks(num_blocks: usize) -> Result<usize> {
    let blocks = type_layout_metadata_start_block(num_blocks)?
        .checked_add(TYPE_LAYOUT_METADATA_BLOCK_COUNT)
        .context("transactional reserved metadata block count overflow")?;
    usize::try_from(blocks).context("transactional reserved metadata block count conversion overflow")
}
```

Update callers that use `METADATA_DESC_START_BLOCK`,
`TYPE_LAYOUT_METADATA_START_BLOCK`, and `RESERVED_METADATA_BLOCKS` to use these
helpers with the current region block count.

- [x] **Step 4: Add in-memory block metadata vectors**

In `block_region.rs`, add `block_metas: Vec<BlockMeta>` to:

```rust
pub(crate) struct VMemoryBlockRegion { ... }
pub(crate) struct NVMemoryBlockRegion { ... }
pub(crate) struct FileBackedMemoryBlockRegion { ... }
```

Initialize each with:

```rust
block_metas: vec![BlockMeta::free(); num_blocks],
```

For `grow_to_blocks`, resize with `BlockMeta::free()`.

- [x] **Step 5: Persist and load the file-backed block table**

Add file-backed helpers:

```rust
fn block_table_offset(&self) -> usize {
    BLOCK_TABLE_START_BLOCK as usize * BLOCK_SIZE
}

fn block_meta_offset(&self, block: usize) -> Result<usize> {
    self.block_table_offset()
        .checked_add(
            block
                .checked_mul(BlockMeta::BYTE_LEN)
                .context("durable block metadata offset overflow")?,
        )
        .context("durable block metadata offset overflow")
}

fn write_block_meta(&mut self, block: usize, meta: BlockMeta) -> Result<()> {
    let offset = self.block_meta_offset(block)?;
    self.block_metas[block] = meta;
    self.write(offset, &meta.as_bytes())
}

fn flush_block_meta_range(&self, start: usize, count: usize) -> Result<()> {
    let offset = self.block_meta_offset(start)?;
    let len = count
        .checked_mul(BlockMeta::BYTE_LEN)
        .context("durable block metadata flush length overflow")?;
    self.flush(offset, len)
}

fn load_block_meta_table(&mut self, header: RegionHeader) -> Result<()> {
    let table_blocks = usize::try_from(header.block_table_block_count)
        .context("transactional block table block count overflow")?;
    ensure!(table_blocks > 0, "transactional region has no block metadata table");
    let table_bytes = table_blocks
        .checked_mul(BLOCK_SIZE)
        .context("transactional block table byte length overflow")?;
    let bytes = self.read(self.block_table_offset(), table_bytes)?;
    for block in 0..self.num_blocks() {
        let start = block
            .checked_mul(BlockMeta::BYTE_LEN)
            .context("durable block metadata decode offset overflow")?;
        let end = start + BlockMeta::BYTE_LEN;
        self.block_metas[block] = BlockMeta::from_bytes(&bytes[start..end])?;
    }
    Ok(())
}
```

- [x] **Step 6: Initialize metadata blocks in new images**

In `initialize_region_image`, compute:

```rust
let reserved_metadata_blocks = reserved_metadata_blocks(self.num_blocks())?;
```

Write `RegionHeader` with:

```rust
block_table_start_block: BLOCK_TABLE_START_BLOCK,
block_table_block_count: block_table_block_count(self.num_blocks())?,
metadata_descs_start_block: metadata_desc_start_block(self.num_blocks())?,
metadata_descs_block_count: METADATA_DESC_BLOCK_COUNT,
num_descs: 1,
```

Set blocks `0..reserved_metadata_blocks` to:

```rust
BlockMeta::active(
    BlockKind::Metadata,
    0,
    0,
    u32::try_from(block).unwrap(),
    1,
)
```

Flush the metadata range and fence:

```rust
self.flush(0, reserved_metadata_blocks * BLOCK_SIZE)?;
self.fence()
```

- [x] **Step 7: Validate and load metadata on open**

In `open_for_test`, after validating the region header, call:

```rust
region.load_block_meta_table(header)?;
region.rebuild_state_from_image()?;
```

Update `validate_region_header` to require:

```rust
header.block_table_start_block == BLOCK_TABLE_START_BLOCK
    && header.block_table_block_count == block_table_block_count(self.num_blocks())?
    && header.metadata_descs_start_block == metadata_desc_start_block(self.num_blocks())?
    && header.metadata_descs_block_count == METADATA_DESC_BLOCK_COUNT
    && header.num_descs == 1
```

- [x] **Step 8: Add test-only metadata accessor**

Add to `FileBackedMemoryBlockRegion`:

```rust
#[cfg(test)]
pub(crate) fn block_meta_for_test(&self, block: usize) -> Result<BlockMeta> {
    self.block_metas
        .get(block)
        .copied()
        .context("test block metadata index out of bounds")
}
```

- [x] **Step 9: Run tests**

Run:

```bash
cargo test -p wasmtime --lib file_backed_region_persists_block_metadata -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
```

Expected: pass. Old file-backed image fixtures are not supported; current tests
create fresh images.

- [x] **Step 10: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/metadata.rs crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
git commit -m "Persist transactional block metadata"
```

## Task 4: Allocate Chunks By Durable Kind

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Test: `cargo test -p wasmtime --lib block_region_tracks_chunk_kinds -- --format terse`

- [x] **Step 1: Add failing kind-tracking tests**

Add these tests in `block_region.rs`:

```rust
#[test]
fn block_region_tracks_log_and_data_chunk_kinds() {
    let mut region = VMemoryBlockRegion::new_for_test(16).unwrap();
    let stream = region.alloc_stream(3).unwrap();

    let object_record = TMemory::encode_publication_data_record(
        0x6000_0000_0000_002a,
        1,
        PackedGranuleDomain::TStruct as u16,
        11,
        &[1, 2, 3],
    )
    .unwrap();
    let object_location = region.append_data_record(stream, &object_record).unwrap();
    let log_block = region.alloc_log_block(3, 0).unwrap();

    assert_eq!(
        region.block_meta_for_test(usize::try_from(object_location.data_block).unwrap())
            .unwrap()
            .kind()
            .unwrap(),
        BlockKind::ObjectData
    );
    assert_eq!(
        region.block_meta_for_test(usize::try_from(log_block).unwrap())
            .unwrap()
            .kind()
            .unwrap(),
        BlockKind::Log
    );
}

#[test]
fn block_region_tracks_linear_undo_chunk_kind() {
    let mut region = VMemoryBlockRegion::new_for_test(16).unwrap();
    let stream = region.alloc_stream(9).unwrap();
    let undo_record = TMemory::encode_granule_undo_data_record(
        0x1000_0000_0000_0007,
        2,
        PackedGranuleDomain::TMemory as u16,
        0,
        &[0xaa; 16],
    )
    .unwrap();

    let location = region.append_data_record(stream, &undo_record).unwrap();

    assert_eq!(
        region.block_meta_for_test(usize::try_from(location.data_block).unwrap())
            .unwrap()
            .kind()
            .unwrap(),
        BlockKind::LinearUndo
    );
}
```

- [x] **Step 2: Run tests and verify they fail**

Run:

```bash
cargo test -p wasmtime --lib block_region_tracks_ -- --format terse
```

Expected: fail because allocation does not set durable block kind.

- [x] **Step 3: Add data-record kind detection**

In `block_region.rs`, add:

```rust
fn block_kind_for_data_record(bytes: &[u8]) -> Result<BlockKind> {
    let header = TxDataRecordHeader::from_bytes(
        bytes
            .get(..size_of::<TxDataRecordHeader>())
            .context("transactional data record is shorter than its header")?,
    )?;
    Ok(match header.role()? {
        TxDataRecordRole::TObjectPub => BlockKind::ObjectData,
        TxDataRecordRole::TMemoryUndo => BlockKind::LinearUndo,
    })
}
```

Import `TxDataRecordHeader` and `TxDataRecordRole`.

- [x] **Step 4: Mark allocated chunks**

Change `alloc_chunk` to a private `alloc_chunk_for_kind(block_count, kind,
owner_thread)` for each concrete region. It should:

1. Prefer retired/free contiguous chunks.
2. Mark `BlockEntry.used = 1`.
3. Write `BlockMeta::active(kind, generation, owner_thread, chunk_start, block_count)` for each block.
4. Flush metadata for file-backed regions before writing chunk bytes.

Keep the trait method `alloc_chunk(block_count)` for generic callers by routing
it to `BlockKind::ObjectData` with owner thread zero:

```rust
fn alloc_chunk(&mut self, block_count: usize) -> Result<RegionChunk> {
    self.alloc_chunk_for_kind(block_count, BlockKind::ObjectData, 0)
}
```

- [x] **Step 5: Pass kind through chunk allocation**

Update:

```rust
alloc_log_block(stream_id, block_seq)
```

to allocate with `BlockKind::Log`.

Update:

```rust
append_data_record(stream, bytes)
```

to compute `let kind = block_kind_for_data_record(bytes)?;` and pass that kind
when it needs to allocate a new data chunk. If `current_data_chunk_start` exists,
verify the current chunk metadata kind matches the new record kind. If it does
not, allocate a new chunk for the new kind.

- [x] **Step 6: Add `block_generation` lookup**

Add to `BlockRegionBackendView`:

```rust
pub(crate) fn block_meta(&self, block: u32) -> Result<BlockMeta> {
    self.backend.block_meta(block)
}

pub(crate) fn block_generation(&self, block: u32) -> Result<u32> {
    Ok(self.block_meta(block)?.generation)
}
```

Add to `BlockRegionBackend`:

```rust
fn block_meta(&self, block: u32) -> Result<BlockMeta>;
```

Implement it for all concrete regions using the in-memory `block_metas` vector.

- [x] **Step 7: Run tests**

Run:

```bash
cargo test -p wasmtime --lib block_region_tracks_ -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
```

Expected: pass.

- [x] **Step 8: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
git commit -m "Track durable block kinds during allocation"
```

## Task 5: Thread Data Block Generation Into Log Entries

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Test: `cargo test -p wasmtime --lib tx_durable_log_sink_preserves_non_zero_data_block_generation -- --format terse` and `cargo test -p wasmtime --lib file_backed_publisher_stamps_data_block_generation_on_log_entry -- --format terse`

- [x] **Step 1: Add failing generation propagation test**

Add a test in `persist.rs` backend tests:

```rust
#[test]
fn file_backed_publisher_stamps_data_block_generation_on_log_entry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("generation-log.bin");
    let mut log = TxDurableLog::create_file_backed(&path, 64).unwrap();
    let mut sink = log.stream_sink(7);
    let mut publisher = StreamPublisher::new(&mut sink, 7, 11);

    let publication = PendingPublication::persistent_object_for_test(41, 1, &[9, 8, 7]);
    let marker = publisher
        .publish_object_publication_before_commit(&publication)
        .unwrap();
    publisher.publish_commit_lp(marker).unwrap();

    let entries = log.log_entries_for_test(7);

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].data_block_generation(), 0);
    assert_eq!(entries[1].data_block_generation(), 0);
}
```

- [x] **Step 2: Run test and verify it fails**

Run:

```bash
cargo test -p wasmtime --lib file_backed_publisher_stamps_data_block_generation_on_log_entry -- --format terse
```

Expected: fail because the durable sink does not return generation metadata.

- [x] **Step 3: Add a durable data pointer type**

In `persist.rs`, replace tuple returns from `DurableSink::append_data_record`
with:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DurableDataRecordPointer {
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) data_block_generation: u32,
}
```

Change the trait method to:

```rust
fn append_data_record(
    &mut self,
    record: &[u8],
    data_stream: DurableDataStream,
) -> Result<DurableDataRecordPointer>;
```

For the in-memory backend, return generation zero:

```rust
Ok(DurableDataRecordPointer {
    data_block,
    data_offset: 0,
    data_block_generation: 0,
})
```

For the file-backed backend, return:

```rust
Ok(DurableDataRecordPointer {
    data_block: location.data_block,
    data_offset: location.data_offset,
    data_block_generation: self.region.block_generation(location.data_block)?,
})
```

- [x] **Step 4: Store generation in pending commit markers**

Update `PendingCommitLogEntry`:

```rust
pub(crate) struct PendingCommitLogEntry {
    pub(crate) logical_id: u64,
    pub(crate) version: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) data_block_generation: u32,
    pub(crate) role: TxLogEntryRole,
}
```

Every place that constructs it must copy `pointer.data_block_generation`.

- [x] **Step 5: Update `TMemory::publication_log_entry`**

Change the signature in `tmemory.rs`:

```rust
pub(crate) fn publication_log_entry(
    logical_id: u64,
    version: u32,
    tx_meta: u32,
    data_block: u32,
    data_offset: u32,
    data_block_generation: u32,
    is_final: bool,
) -> Result<TxLogEntry>
```

Set the generation:

```rust
let mut entry = TxLogEntry::new(logical_id, version, tx_meta, data_block, data_offset);
entry.set_data_block_generation(data_block_generation)?;
if is_final {
    entry.tx_meta |= 1;
}
entry.seal_crc32();
Ok(entry)
```

Update all call sites. Test helpers that do not use durable metadata should pass
generation zero.

- [x] **Step 6: Update `DurableSink::append_log_entry`**

Add `data_block_generation: u32` to the sink method and call:

```rust
let mut entry = TMemory::publication_log_entry(
    logical_id,
    version,
    tx_meta,
    data_block,
    data_offset,
    data_block_generation,
    is_final,
)?;
entry.set_role(role);
entry.seal_crc32();
```

- [x] **Step 7: Run tests**

Run:

```bash
cargo test -p wasmtime --lib tx_durable_log_sink_preserves_non_zero_data_block_generation -- --format terse
cargo test -p wasmtime --lib file_backed_publisher_stamps_data_block_generation_on_log_entry -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
```

Expected: pass.

- [x] **Step 8: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory.rs crates/wasmtime/src/runtime/transaction/persist.rs crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
git commit -m "Stamp durable log entries with block generation"
```

## Task 6: Validate Generation During Recovery

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Test: `cargo test -p wasmtime --test transaction_persistence stale_generation -- --format terse`

- [x] **Step 1: Add failing stale-generation test**

Add this test to `crates/wasmtime/tests/transaction_persistence.rs`:

```rust
#[test]
fn file_backed_recovery_ignores_stale_generation_object_publication() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("stale-generation-region.bin");

    create_file_backed_region_image(&path, 128).unwrap();
    publish_committed_struct_object(&path, 1, 41, 1, 12, &[9, 8, 7]).unwrap();
    wasmtime::_internal::transaction_persistence::bump_first_object_data_block_generation_for_test(
        &path,
    )
    .unwrap();

    let recovered = reopen_and_recover_file_backed_region(&path).unwrap();

    assert!(recovered.winners.is_empty());
    assert!(recovered.object_winners.is_empty());
}
```

Expose `bump_first_object_data_block_generation_for_test` from
`block_region.rs` through `_internal::transaction_persistence`.

- [x] **Step 2: Run test and verify it fails**

Run:

```bash
cargo test -p wasmtime --test transaction_persistence stale_generation -- --format terse
```

Expected: fail because recovery still accepts the old log entry.

- [x] **Step 3: Add block validation helper**

In `recovery.rs`, add:

```rust
fn expected_data_kind(role: TxLogEntryRole) -> BlockKind {
    match role {
        TxLogEntryRole::TObjectPub => BlockKind::ObjectData,
        TxLogEntryRole::TMemoryUndo => BlockKind::LinearUndo,
    }
}

fn validate_entry_data_block(
    region: &BlockRegionBackendView<'_>,
    entry: TxLogEntry,
) -> Result<bool> {
    let meta = region.block_meta(entry.data_block)?;
    if !meta.is_active_or_sealed()? {
        return Ok(false);
    }
    if meta.kind()? != expected_data_kind(entry.role()?) {
        return Ok(false);
    }
    Ok(meta.generation == entry.data_block_generation())
}
```

Import `BlockKind`.

- [x] **Step 4: Use validation in committed replay**

In `replay_stream`, when processing committed entries:

```rust
for committed in &txn.entries {
    if !validate_entry_data_block(region, *committed)? {
        continue;
    }
    validate_committed_entry(region, data_chunk_index, *committed)?;
    if committed.role()? == TxLogEntryRole::TObjectPub {
        replay.winners.push(RecoveryWinner {
            logical_id: committed.logical_id,
            version: committed.version,
            data_block: committed.data_block,
            data_offset: committed.data_offset,
        });
    }
}
```

- [x] **Step 5: Use validation for loose-end undo**

In `loose_end_tmemory_undo_rollbacks`, skip undo entries that do not validate:

```rust
if !validate_entry_data_block(region, entry)? {
    continue;
}
```

This keeps retired/reused undo chunks from rolling back unrelated bytes.

- [x] **Step 6: Make discovery metadata-aware**

In `discover_region`, ignore block magic for blocks whose metadata says
`Free`, `Retired`, or a mismatched kind. For a log block, require
`BlockKind::Log`. For a data chunk, require either `BlockKind::ObjectData` or
`BlockKind::LinearUndo`.

Use:

```rust
let meta = region.block_meta(start_block)?;
if !meta.is_active_or_sealed()? {
    block += 1;
    continue;
}
```

Then validate kind inside each magic branch.

- [x] **Step 7: Run tests**

Run:

```bash
cargo test -p wasmtime --test transaction_persistence stale_generation -- --format terse
cargo test -p wasmtime --lib recovery -- --format terse
```

Expected: pass.

- [x] **Step 8: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs crates/wasmtime/tests/transaction_persistence.rs
git commit -m "Validate durable block generations during recovery"
```

## Task 7: Retire And Reuse Whole-Dead Object Chunks

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/tests/transaction_persistence.rs`
- Test: `cargo test -p wasmtime --test transaction_persistence object_chunk_reuse -- --format terse`

- [x] **Step 1: Add failing object reuse test**

Add this test:

```rust
#[test]
fn file_backed_object_chunk_reuse_does_not_resurrect_old_object() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("object-reuse-region.bin");

    create_file_backed_region_image(&path, 128).unwrap();
    publish_committed_struct_object(&path, 1, 41, 1, 12, &[1, 1, 1]).unwrap();

    wasmtime::_internal::transaction_persistence::retire_unreachable_object_chunks_for_test(
        &path,
        &[41],
    )
    .unwrap();
    publish_committed_struct_object(&path, 2, 42, 1, 12, &[2, 2, 2]).unwrap();

    let recovered = reopen_and_recover_file_backed_region(&path).unwrap();
    let object_ids = recovered
        .object_winners
        .iter()
        .map(|winner| winner.object_id)
        .collect::<Vec<_>>();

    assert_eq!(object_ids, vec![42]);
}
```

The helper argument `&[41]` means object 41 is treated as unreachable by the
test harness. The helper must not alter test WAST files or existing test cases.

- [x] **Step 2: Run test and verify it fails**

Run:

```bash
cargo test -p wasmtime --test transaction_persistence object_chunk_reuse -- --format terse
```

Expected: fail because object chunks are never retired or reused.

- [x] **Step 3: Add block-range utilities**

In `block_region.rs`, add:

```rust
fn record_block_range(data_block: u32, data_offset: u32, record_len: u64) -> Result<core::ops::Range<u32>> {
    let start = usize::try_from(data_block)
        .context("record data block index overflow")?;
    let offset = usize::try_from(data_offset)
        .context("record data offset overflow")?;
    let len = usize::try_from(record_len)
        .context("record length does not fit usize")?;
    let end_byte = offset
        .checked_add(len)
        .context("record block range overflow")?;
    let extra_blocks = end_byte.div_ceil(BLOCK_SIZE);
    let end = start
        .checked_add(extra_blocks)
        .context("record block range overflow")?;
    Ok(u32::try_from(start).unwrap()..u32::try_from(end).unwrap())
}
```

- [x] **Step 4: Add chunk retirement API**

Add to `FileBackedMemoryBlockRegion`:

```rust
pub(crate) fn retire_whole_dead_object_chunks(
    &mut self,
    reachable: &[PersistentRecoveredRecordLocation],
    unreachable: &[PersistentRecoveredRecordLocation],
) -> Result<Vec<u32>> {
    let mut live_blocks = BTreeSet::new();
    for record in reachable {
        for block in record_block_range(record.data_block, record.data_offset, record.record_len)? {
            live_blocks.insert(block);
        }
    }

    let mut candidate_chunk_starts = BTreeSet::new();
    for record in unreachable {
        let meta = self.block_meta(record.data_block)?;
        if meta.kind()? == BlockKind::ObjectData && meta.is_active_or_sealed()? {
            candidate_chunk_starts.insert(meta.chunk_start);
        }
    }

    let mut retired = Vec::new();
    for chunk_start in candidate_chunk_starts {
        let meta = self.block_meta(chunk_start)?;
        if meta.kind()? != BlockKind::ObjectData || !meta.is_active_or_sealed()? {
            continue;
        }
        let chunk_blocks = usize::try_from(meta.chunk_blocks)
            .context("object data chunk block count overflow")?;
        let chunk_start_usize = usize::try_from(chunk_start)
            .context("object data chunk start overflow")?;
        let chunk_end = chunk_start_usize
            .checked_add(chunk_blocks)
            .context("object data chunk end overflow")?;
        let has_live = (chunk_start_usize..chunk_end).any(|block| {
            live_blocks.contains(&u32::try_from(block).unwrap())
        });
        if has_live {
            continue;
        }
        self.retire_chunk(chunk_start_usize, chunk_blocks)?;
        retired.push(chunk_start);
    }
    Ok(retired)
}
```

Add `retire_chunk`:

```rust
fn retire_chunk(&mut self, start: usize, block_count: usize) -> Result<()> {
    let end = start
        .checked_add(block_count)
        .context("retired chunk range overflow")?;
    ensure!(end <= self.num_blocks(), "retired chunk range out of bounds");
    for block in start..end {
        let mut meta = self.block_metas[block];
        meta.state = BlockState::Retired as u8;
        self.write_block_meta(block, meta)?;
        self.block_entries[block].used = 0;
        self.block_entries[block].list_num = ListKind::LargeFree as i16;
    }
    self.flush_block_meta_range(start, block_count)?;
    self.fence()
}
```

- [x] **Step 5: Reuse retired chunks with incremented generation**

In `alloc_chunk_for_kind`, when choosing a contiguous free/retired range:

```rust
let next_generation = match meta.state()? {
    BlockState::Retired => meta
        .generation
        .checked_add(1)
        .filter(|generation| *generation <= TxEntryMeta::MAX_DATA_BLOCK_GENERATION)
        .context("durable block generation exhausted")?,
    BlockState::Free => meta.generation,
    _ => continue,
};
```

Set every block in the chosen range to `BlockMeta::active(kind, next_generation,
owner_thread, start_block, block_count)`.

- [x] **Step 6: Add test helper**

Expose a test helper that:

1. Opens the file-backed region.
2. Runs recovery.
3. Filters recovered object winners by the unreachable object-id list.
4. Calls `retire_whole_dead_object_chunks`.
5. Fences before returning.

Signature:

```rust
pub fn retire_unreachable_object_chunks_for_test(
    path: &Path,
    unreachable_object_ids: &[u64],
) -> Result<Vec<u32>>
```

- [x] **Step 7: Run tests**

Run:

```bash
cargo test -p wasmtime --test transaction_persistence object_chunk_reuse -- --format terse
cargo test -p wasmtime --lib persistent_gc_recovery_filter -- --format terse
```

Expected: pass.

- [x] **Step 8: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/tests/transaction_persistence.rs
git commit -m "Reuse whole-dead persistent object chunks"
```

## Task 8: Retire Completed Linear Undo Chunks

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Modify: `crates/wasmtime/tests/transaction_persistence.rs`
- Test: `cargo test -p wasmtime --test transaction_persistence linear_undo_chunk_reuse -- --format terse`

- [x] **Step 1: Add failing linear-undo reuse test**

Add this test:

```rust
#[test]
fn file_backed_linear_undo_chunk_reuse_does_not_roll_back_committed_transaction() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("undo-reuse-region.bin");

    create_file_backed_region_image(&path, 128).unwrap();
    wasmtime::_internal::transaction_persistence::publish_committed_tmemory_undo_for_test(
        &path,
        1,
        0x1000_0000_0000_0001,
        1,
        &[1, 1, 1, 1],
    )
    .unwrap();
    wasmtime::_internal::transaction_persistence::retire_completed_linear_undo_chunks_for_test(
        &path,
    )
    .unwrap();
    publish_committed_struct_object(&path, 2, 42, 1, 12, &[2, 2, 2]).unwrap();

    let recovered = reopen_and_recover_file_backed_region(&path).unwrap();

    assert!(recovered.tmemory_undo_rollbacks.is_empty());
    assert_eq!(recovered.object_winners.len(), 1);
    assert_eq!(recovered.object_winners[0].object_id, 42);
}
```

- [x] **Step 2: Run test and verify it fails**

Run:

```bash
cargo test -p wasmtime --test transaction_persistence linear_undo_chunk_reuse -- --format terse
```

Expected: fail because linear-undo chunks are not retired.

- [x] **Step 3: Add linear-undo retirement API**

In `FileBackedMemoryBlockRegion`, add:

```rust
pub(crate) fn retire_linear_undo_chunks<I>(&mut self, chunk_starts: I) -> Result<Vec<u32>>
where
    I: IntoIterator<Item = u32>,
{
    let mut retired = Vec::new();
    for chunk_start in chunk_starts {
        let meta = self.block_meta(chunk_start)?;
        if meta.kind()? != BlockKind::LinearUndo || !meta.is_active_or_sealed()? {
            continue;
        }
        self.retire_chunk(
            usize::try_from(chunk_start).context("linear undo chunk start overflow")?,
            usize::try_from(meta.chunk_blocks).context("linear undo chunk block count overflow")?,
        )?;
        retired.push(chunk_start);
    }
    Ok(retired)
}
```

- [x] **Step 4: Track undo chunk starts in pending markers**

Extend `DurableDataRecordPointer` and `PendingCommitLogEntry`:

```rust
pub(crate) struct DurableDataRecordPointer {
    pub(crate) chunk_start_block: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) data_block_generation: u32,
}

pub(crate) struct PendingCommitLogEntry {
    pub(crate) chunk_start_block: u32,
    pub(crate) logical_id: u64,
    pub(crate) version: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) data_block_generation: u32,
    pub(crate) role: TxLogEntryRole,
}
```

- [x] **Step 5: Retire committed undo chunks after LP**

In `FileBackedTxDurableLog`, add:

```rust
fn retire_committed_undo_chunk(&mut self, chunk_start_block: u32) -> Result<()> {
    self.region
        .retire_linear_undo_chunks(core::iter::once(chunk_start_block))?;
    Ok(())
}
```

In `StreamPublisher::publish_commit_lp`, after the LP log flush and fence:

```rust
if marker.role == TxLogEntryRole::TMemoryUndo {
    self.sink.retire_committed_undo_chunk(marker.chunk_start_block)?;
}
```

Add a default no-op implementation for in-memory sinks.

- [x] **Step 6: Add recovery/test helper**

Expose:

```rust
pub fn retire_completed_linear_undo_chunks_for_test(path: &Path) -> Result<Vec<u32>>
```

The helper opens the file-backed region, scans committed transactions through
normal recovery, collects active `LinearUndo` chunks that have no loose-end undo
rollback entry, retires them, flushes metadata, and fences.

- [x] **Step 7: Run tests**

Run:

```bash
cargo test -p wasmtime --test transaction_persistence linear_undo_chunk_reuse -- --format terse
cargo test -p wasmtime --lib model_recovery -- --format terse
```

Expected: pass.

- [x] **Step 8: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/persist.rs crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs crates/wasmtime/tests/transaction_persistence.rs
git commit -m "Retire completed linear undo chunks"
```

## Task 9: Documentation And Quality Gate

**Files:**
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
- Test: full verification commands below

- [x] **Step 1: Update design documentation**

In `transactional-wasm-runtime-core-design.md`, make the field names match the
implementation:

```text
TxLogEntry.entry_meta:
  low 2 bits  = TxLogEntryRole
  high 30 bits = data_block_generation
```

Document that a recovered log entry is accepted only when:

```text
BlockMeta.state in {Active, Sealed}
BlockMeta.kind matches TxLogEntryRole
BlockMeta.generation == TxLogEntry.data_block_generation
```

- [x] **Step 2: Update implementation log**

Add an entry to `transactional-wasm-implementation-log.md`:

```markdown
## Durable Block Reuse

- Added durable block metadata for file-backed transaction regions.
- Packed data-block generation into the fixed 32-byte `TxLogEntry`.
- Recovery rejects entries that point to retired or reused data blocks.
- Whole-dead object-data chunks and completed linear-undo chunks can be reused.
- Mixed-block copying cleanup and line-level reuse remain outside this milestone.
```

- [x] **Step 3: Run formatting and focused tests**

Run:

```bash
cargo fmt --check
cargo test -p wasmtime --lib tx_log_entry -- --format terse
cargo test -p wasmtime --lib block_meta -- --format terse
cargo test -p wasmtime --lib block_region_tracks_ -- --format terse
cargo test -p wasmtime --lib recovery -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
```

Expected: all pass.

- [x] **Step 4: Run transaction WAST gate**

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```

Expected: all transaction WAST cases still pass with no ignored transaction WAST
tests reintroduced.

- [x] **Step 5: Run diff hygiene**

Run:

```bash
git diff --check
git status --short
```

Expected: no whitespace errors. `git status --short` should show only the
intended implementation, tests, and documentation before committing.

- [x] **Step 6: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs \
        crates/wasmtime/src/runtime/vm/memory/tmemory/metadata.rs \
        crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs \
        crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs \
        crates/wasmtime/src/runtime/vm/memory/tmemory.rs \
        crates/wasmtime/src/runtime/transaction/persist.rs \
        crates/wasmtime/src/runtime/transaction.rs \
        crates/wasmtime/tests/transaction_persistence.rs \
        docs/shisoft/transactional-wasm-runtime-core-design.md \
        docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Add durable block reuse for transactional storage"
```

## Follow-On Plan Seeds

These are intentionally outside this milestone:

- Mixed object-data chunk cleaner: copy live records to fresh chunks, publish
  higher object versions, LP the cleaner transaction, then retire the old chunk.
- Line-level reuse inside active object chunks after chunk-generation reuse has
  been stress tested.
- Log-block reuse with checkpointed stream summaries.
- Durable checkpoint/rebuild path for exhausted 30-bit block generations.
- Durable clean-abort rollback completion so non-LP undo chunks can be
  reclaimed only after rollback is known durable.
- Full persistent GC with object movement and object-table rebuild after
  compaction.

## Self-Review

- Spec coverage: The plan covers durable block metadata, fixed-size log-entry
  generation packing, generation-checked recovery, whole-dead object-data chunk
  reuse, linear-undo chunk reuse, docs, and verification.
- Rejected ideas preserved: no object table persistence, no transaction-log
  tombstones, no object-level line reuse in this milestone, no backward
  compatibility burden for old file-backed research images.
- Type consistency: `BlockMeta`, `BlockState`, `BlockKind`, `TxEntryMeta`,
  `DurableDataRecordPointer`, and `PendingCommitLogEntry` are introduced before
  later tasks consume them.
