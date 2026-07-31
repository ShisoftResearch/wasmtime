//! Wizard-style storage for transactional memories.
//!
//! This module backs per-instance `tmemory` sidecars with volatile,
//! DAX-PMEM-capable, and filesystem-backed transactional storage.

#![allow(dead_code)]

use crate::prelude::*;
use crate::runtime::transaction::PendingGranuleUndo;
use crate::runtime::transaction::{
    TMEMORY_GRANULE_SHIFT, TMEMORY_GRANULE_SIZE, TMemoryBackend, TMemoryDaxPmemBacking,
    TMemoryFileBacking, TMemoryPersistenceMode, TransactionConfig,
};
use alloc::collections::BTreeMap;
use wasmtime_environ::MemoryIndex;

pub(crate) mod block_region;
mod durable_log;
mod linear_region;
pub(crate) mod metadata;
pub(crate) mod numa;
pub(crate) mod recovery;
pub(crate) mod region;

use self::linear_region::TMemoryRegion;
pub(crate) use durable_log::*;

pub(crate) const WASM_PAGE_SIZE: usize = 64 * 1024;
const DEFAULT_MAX_WASM_PAGES: u64 = 1 << 16;
pub(crate) const SMALL_DATA_LIMIT: usize = block_region::IMMIX_LINE_SIZE;

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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TMemoryGranuleInfo {
    owner: u64,
    version: u64,
    hash: u64,
}

pub(crate) trait TMemoryBackendStorage: core::fmt::Debug + Send + Sync {
    fn backend_kind(&self) -> TMemoryBackend;
    fn byte_len(&self) -> usize;
    fn byte_capacity(&self) -> usize;
    fn granule_count(&self) -> usize;
    fn read_committed(&self, range: core::ops::Range<usize>) -> Result<Vec<u8>>;
    fn commit_range(&mut self, addr: usize, bytes: &[u8]) -> Result<()>;
    fn read_within_capacity(&self, range: core::ops::Range<usize>) -> Result<Vec<u8>>;
    fn commit_range_within_capacity(&mut self, addr: usize, bytes: &[u8]) -> Result<()>;
    fn reserve_backing_capacity_to_pages(&mut self, new_pages: u64) -> Result<()>;
    fn can_grow_to_pages(&self, new_pages: u64) -> bool;
    fn grow_to_pages(&mut self, new_pages: u64) -> Result<()>;
    fn granule_info(&self, granule: usize) -> Result<TMemoryGranuleInfo>;
    fn set_granule_info(&mut self, granule: usize, info: TMemoryGranuleInfo) -> Result<()>;
    fn granule_info_within_capacity(&self, granule: usize) -> Result<TMemoryGranuleInfo>;
    fn set_granule_info_within_capacity(
        &mut self,
        granule: usize,
        info: TMemoryGranuleInfo,
    ) -> Result<()>;
    #[cfg(test)]
    fn block_region_block_size_for_test(&self) -> usize;
    #[cfg(test)]
    fn immix_line_size_for_test(&self) -> usize;
    #[cfg(test)]
    fn line_mark_count_for_test(&self) -> usize;
}

/// Transactional memory storage selected by transaction configuration.
#[derive(Debug)]
pub(crate) struct TMemory {
    storage: Box<dyn TMemoryBackendStorage>,
}

/// Per-instance transactional memories keyed by raw module-level `MemoryIndex`.
#[derive(Debug, Default)]
pub(crate) struct TMemorySidecar {
    memories: TryBTreeMap<u32, TMemory>,
}

impl TMemorySidecar {
    pub(crate) fn insert(
        &mut self,
        memory: MemoryIndex,
        tmemory: TMemory,
    ) -> core::result::Result<Option<TMemory>, OutOfMemory> {
        self.memories.insert(memory.as_u32(), tmemory)
    }

    pub(crate) fn get(&self, memory: MemoryIndex) -> Option<&TMemory> {
        self.memories.get(memory.as_u32())
    }

    pub(crate) fn get_mut(&mut self, memory: MemoryIndex) -> Option<&mut TMemory> {
        self.memories.get_mut(memory.as_u32())
    }

    pub(crate) fn contains(&self, memory: MemoryIndex) -> bool {
        self.memories.contains_key(memory.as_u32())
    }
}

impl TMemory {
    pub(crate) fn new(
        config: TransactionConfig,
        min_pages: u64,
        max_pages: Option<u64>,
    ) -> Result<Self> {
        Self::new_for_backend(
            config.tmemory_backend(),
            config.tmemory_persistence_mode(),
            config.tmemory_file_backing(),
            config.tmemory_dax_pmem_backing(),
            min_pages,
            max_pages,
        )
    }

    pub(crate) fn new_with_backend(backend: TMemoryBackend, min_pages: u64) -> Result<Self> {
        Self::new_with_backend_limits(backend, min_pages, Some(min_pages))
    }

    pub(crate) fn new_with_backend_limits(
        backend: TMemoryBackend,
        min_pages: u64,
        max_pages: Option<u64>,
    ) -> Result<Self> {
        Self::new_for_backend(
            backend,
            TMemoryPersistenceMode::ResearchPretendDaxPmem,
            None,
            (backend == TMemoryBackend::DaxPmem).then_some(TMemoryDaxPmemBacking::ResearchTemp),
            min_pages,
            max_pages,
        )
    }

    pub(crate) fn new_vmemory(min_pages: u64) -> Result<Self> {
        Self::new_with_backend(TMemoryBackend::VMemory, min_pages)
    }

    pub(crate) fn new_vmemory_with_limits(min_pages: u64, max_pages: Option<u64>) -> Result<Self> {
        Self::new_with_backend_limits(TMemoryBackend::VMemory, min_pages, max_pages)
    }

    pub(crate) fn encode_publication_data_record(
        logical_id: u64,
        version: u32,
        kind: u16,
        type_info: u32,
        payload: &[u8],
    ) -> Result<Vec<u8>> {
        let payload_len =
            u32::try_from(payload.len()).context("publication payload length does not fit u32")?;
        let header = TxDataRecordHeader {
            logical_id,
            version,
            kind,
            role: 0,
            payload_len,
            type_info,
        };

        let mut record = Vec::with_capacity(
            header
                .as_bytes()
                .len()
                .checked_add(payload.len())
                .context("publication record length overflow")?,
        );
        record.extend_from_slice(&header.as_bytes());
        record.extend_from_slice(payload);
        Ok(record)
    }

    pub(crate) fn encode_granule_undo_data_record(
        logical_id: u64,
        version: u32,
        kind: u16,
        type_info: u32,
        old_granule_bytes: &[u8],
    ) -> Result<Vec<u8>> {
        let payload_len = u32::try_from(old_granule_bytes.len())
            .context("undo payload length does not fit u32")?;
        let mut header = TxDataRecordHeader {
            logical_id,
            version,
            kind,
            role: 0,
            payload_len,
            type_info,
        };
        header.set_role(TxDataRecordRole::TMemoryUndo);

        let mut record = Vec::with_capacity(
            header
                .as_bytes()
                .len()
                .checked_add(old_granule_bytes.len())
                .context("undo record length overflow")?,
        );
        record.extend_from_slice(&header.as_bytes());
        record.extend_from_slice(old_granule_bytes);
        Ok(record)
    }

    pub(crate) fn publication_log_entry(
        logical_id: u64,
        version: u32,
        tx_meta: u32,
        data_block: u32,
        data_offset: u32,
        data_block_generation: u32,
        is_final: bool,
    ) -> Result<TxLogEntry> {
        let mut entry = TxLogEntry::new(logical_id, version, tx_meta, data_block, data_offset);
        entry.set_data_block_generation(data_block_generation)?;
        if is_final {
            entry.tx_meta |= 1;
        }
        entry.seal_crc32();
        Ok(entry)
    }

    pub(crate) fn backend(&self) -> TMemoryBackend {
        self.backend_kind()
    }

    pub(crate) fn backend_kind(&self) -> TMemoryBackend {
        self.storage.backend_kind()
    }

    pub(crate) fn byte_len(&self) -> usize {
        self.storage.byte_len()
    }

    pub(crate) fn byte_capacity(&self) -> usize {
        self.storage.byte_capacity()
    }

    pub(crate) fn granule_len(&self) -> usize {
        self.granule_count()
    }

    pub(crate) fn granule_count(&self) -> usize {
        self.storage.granule_count()
    }

    pub(crate) fn read_committed(&self, range: core::ops::Range<usize>) -> Result<Vec<u8>> {
        self.storage.read_committed(range)
    }

    pub(crate) fn commit_range(&mut self, addr: usize, bytes: &[u8]) -> Result<()> {
        self.storage.commit_range(addr, bytes)?;
        if bytes.is_empty() {
            return Ok(());
        }

        let end = addr
            .checked_add(bytes.len())
            .context("tmemory write address overflow")?;
        let last_byte = end - 1;
        let first_granule = addr / TMEMORY_GRANULE_SIZE;
        let last_granule = last_byte / TMEMORY_GRANULE_SIZE;

        for granule in first_granule..=last_granule {
            let mut info = self.storage.granule_info(granule)?;
            info.owner = 0;
            info.version = info
                .version
                .checked_add(1)
                .context("tmemory granule version overflow")?;
            self.storage.set_granule_info(granule, info)?;
        }

        Ok(())
    }

    pub(crate) fn prepare_tmemory_undo_record(
        &self,
        owner_instance: Option<u32>,
        memory_index: u32,
        granule_index: u64,
        bytes: &[u8],
    ) -> Result<PendingGranuleUndo> {
        let granule = usize::try_from(granule_index)
            .context("tmemory granule index does not fit host usize")?;
        let range = tmemory_granule_range(granule, self.byte_len())?;
        ensure!(
            bytes.len() == range.end - range.start,
            "persistent tmemory staged granule length mismatch"
        );

        let old_bytes = self.read_committed(range)?;
        let current_version = self.granule_version(granule)?;
        let version = u32::try_from(
            current_version
                .checked_add(1)
                .context("tmemory granule version overflow")?,
        )
        .context("tmemory granule version does not fit durable log")?;
        let logical_id = pack_tmemory_granule_id(owner_instance, memory_index, granule_index)?;
        Ok(PendingGranuleUndo::tmemory(logical_id, version, old_bytes))
    }

    pub(crate) fn commit_staged_tmemory_granule(
        &mut self,
        granule_index: u64,
        bytes: &[u8],
    ) -> Result<()> {
        let granule = usize::try_from(granule_index)
            .context("tmemory granule index does not fit host usize")?;
        let range = tmemory_granule_range(granule, self.byte_len())?;
        ensure!(
            bytes.len() == range.end - range.start,
            "tmemory staged granule length mismatch"
        );
        self.commit_range(range.start, bytes)
    }

    pub(crate) fn commit_staged_tmemory_granules_direct(
        &mut self,
        staged: &[(u64, Vec<u8>)],
    ) -> Result<()> {
        for (granule_index, bytes) in staged {
            self.commit_staged_tmemory_granule(*granule_index, bytes)?;
        }
        Ok(())
    }

    #[cfg(feature = "transaction-mvcc")]
    pub(crate) fn reserve_backing_capacity_to_pages_for_mvcc(
        &mut self,
        new_pages: u64,
    ) -> Result<()> {
        self.storage.reserve_backing_capacity_to_pages(new_pages)
    }

    #[cfg(feature = "transaction-mvcc")]
    pub(crate) fn prepare_tmemory_undo_record_within_capacity_for_mvcc(
        &self,
        owner_instance: Option<u32>,
        memory_index: u32,
        granule_index: u64,
        bytes: &[u8],
    ) -> Result<PendingGranuleUndo> {
        let granule = usize::try_from(granule_index)
            .context("tmemory granule index does not fit host usize")?;
        let range = tmemory_granule_range(granule, self.byte_capacity())?;
        ensure!(
            bytes.len() == range.end - range.start,
            "persistent tmemory staged capacity granule length mismatch"
        );

        let old_bytes = self.storage.read_within_capacity(range)?;
        let current_version = self.storage.granule_info_within_capacity(granule)?.version;
        let version = u32::try_from(
            current_version
                .checked_add(1)
                .context("tmemory granule version overflow")?,
        )
        .context("tmemory granule version does not fit durable log")?;
        let logical_id = pack_tmemory_granule_id(owner_instance, memory_index, granule_index)?;
        Ok(PendingGranuleUndo::tmemory(logical_id, version, old_bytes))
    }

    #[cfg(feature = "transaction-mvcc")]
    pub(crate) fn commit_staged_tmemory_granule_within_capacity_for_mvcc(
        &mut self,
        granule_index: u64,
        bytes: &[u8],
    ) -> Result<()> {
        let granule = usize::try_from(granule_index)
            .context("tmemory granule index does not fit host usize")?;
        let range = tmemory_granule_range(granule, self.byte_capacity())?;
        ensure!(
            bytes.len() == range.end - range.start,
            "tmemory staged capacity granule length mismatch"
        );
        self.storage
            .commit_range_within_capacity(range.start, bytes)?;
        let mut info = self.storage.granule_info_within_capacity(granule)?;
        info.owner = 0;
        info.version = info
            .version
            .checked_add(1)
            .context("tmemory granule version overflow")?;
        self.storage.set_granule_info_within_capacity(granule, info)
    }

    pub(crate) fn apply_recovered_tmemory_undo_rollbacks<I>(&mut self, rollbacks: I) -> Result<()>
    where
        I: IntoIterator<Item = recovery::RecoveredTMemoryUndoRollback>,
    {
        let mut oldest_by_granule = BTreeMap::<u64, recovery::RecoveredTMemoryUndoRollback>::new();
        for rollback in rollbacks {
            match oldest_by_granule.get(&rollback.logical_id) {
                Some(current)
                    if rollback.version == current.version
                        && rollback.old_granule_bytes != current.old_granule_bytes =>
                {
                    bail!("conflicting tmemory rollback records for same logical id/version");
                }
                Some(current) if current.version <= rollback.version => {}
                _ => {
                    oldest_by_granule.insert(rollback.logical_id, rollback);
                }
            }
        }

        for rollback in oldest_by_granule.into_values() {
            let (_, _, granule_index) = unpack_tmemory_granule_id(rollback.logical_id)?;
            let granule = usize::try_from(granule_index)
                .context("tmemory rollback granule index does not fit host usize")?;
            let range = tmemory_granule_range(granule, self.byte_capacity())?;
            ensure!(
                rollback.old_granule_bytes.len() == range.end - range.start,
                "tmemory rollback granule length mismatch"
            );
            self.storage
                .commit_range_within_capacity(range.start, &rollback.old_granule_bytes)?;
        }

        Ok(())
    }

    pub(crate) fn apply_file_backed_recovered_tmemory_undo_rollbacks_for_test<I>(
        &mut self,
        rollbacks: I,
    ) -> Result<()>
    where
        I: IntoIterator<Item = block_region::TransactionPersistenceRecoveredTMemoryUndoRollback>,
    {
        self.apply_recovered_tmemory_undo_rollbacks(rollbacks.into_iter().map(|rollback| {
            recovery::RecoveredTMemoryUndoRollback {
                logical_id: rollback.logical_id,
                version: rollback.version,
                data_block: 0,
                data_offset: 0,
                old_granule_bytes: rollback.old_granule_bytes,
            }
        }))
    }

    pub(crate) fn grow_to_pages(&mut self, new_pages: u64) -> Result<()> {
        self.storage.grow_to_pages(new_pages)
    }

    pub(crate) fn can_grow_to_pages(&self, new_pages: u64) -> bool {
        self.storage.can_grow_to_pages(new_pages)
    }

    pub(crate) fn granule_info(&self, granule: usize) -> Result<TMemoryGranuleInfo> {
        self.storage.granule_info(granule)
    }

    pub(crate) fn granule_version(&self, granule: usize) -> Result<u64> {
        Ok(self.storage.granule_info(granule)?.version)
    }

    pub(crate) fn set_granule_info(
        &mut self,
        granule: usize,
        info: TMemoryGranuleInfo,
    ) -> Result<()> {
        self.storage.set_granule_info(granule, info)
    }

    #[cfg(test)]
    pub(crate) fn block_region_block_size_for_test(&self) -> usize {
        self.storage.block_region_block_size_for_test()
    }

    #[cfg(test)]
    pub(crate) fn immix_line_size_for_test(&self) -> usize {
        self.storage.immix_line_size_for_test()
    }

    #[cfg(test)]
    pub(crate) fn line_mark_count_for_test(&self) -> usize {
        self.storage.line_mark_count_for_test()
    }

    fn new_for_backend(
        backend: TMemoryBackend,
        persistence_mode: TMemoryPersistenceMode,
        file_backing: Option<TMemoryFileBacking>,
        dax_pmem_backing: Option<TMemoryDaxPmemBacking>,
        min_pages: u64,
        max_pages: Option<u64>,
    ) -> Result<Self> {
        let storage: Box<dyn TMemoryBackendStorage> = match backend {
            TMemoryBackend::VMemory => Box::new(VMemory::new(min_pages, max_pages)?),
            TMemoryBackend::DaxPmem => {
                let backing =
                    dax_pmem_backing.context("DaxPmem requires DAX PMEM backing configuration")?;
                Box::new(DaxPmemMemory::new_with_backing(
                    min_pages,
                    max_pages,
                    persistence_mode,
                    backing,
                )?)
            }
            TMemoryBackend::FileBackedMemory => {
                let file_backing = file_backing
                    .context("FileBackedMemory requires explicit file backing configuration")?;
                Box::new(FileBackedMemory::new(min_pages, max_pages, file_backing)?)
            }
        };
        Ok(Self { storage })
    }
}

/// Volatile anonymous-mmap transactional memory storage.
#[derive(Debug)]
pub(crate) struct VMemory {
    region: TMemoryRegion,
    granules: Vec<TMemoryGranuleInfo>,
    byte_len: usize,
    byte_capacity: usize,
    max_pages: u64,
}

impl VMemory {
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
            region: TMemoryRegion::new(byte_capacity)?,
            granules: vec![TMemoryGranuleInfo::default(); granule_capacity],
            byte_len,
            byte_capacity,
            max_pages,
        })
    }

    pub(crate) fn byte_len(&self) -> usize {
        self.byte_len
    }

    pub(crate) fn granule_len(&self) -> usize {
        granules_for_bytes(self.byte_len)
    }

    pub(crate) fn granule_capacity(&self) -> usize {
        self.granules.len()
    }

    pub(crate) fn granule_index(addr: u64) -> Result<usize> {
        let index = addr >> TMEMORY_GRANULE_SHIFT;
        usize::try_from(index).context("tmemory address does not fit host usize")
    }

    pub(crate) fn grow_to_pages(&mut self, new_pages: u64) -> Result<()> {
        ensure!(
            new_pages <= self.max_pages,
            "tmemory growth exceeds maximum size"
        );
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

        let mut region = TMemoryRegion::new(new_byte_capacity)?;
        if self.byte_len > 0 {
            let old = self.region.read(0, self.byte_len)?;
            region.write(0, &old)?;
        }

        let new_granule_capacity = granules_for_bytes(new_byte_capacity);
        self.granules
            .resize(new_granule_capacity, TMemoryGranuleInfo::default());

        self.region = region;
        self.byte_capacity = new_byte_capacity;
        Ok(())
    }

    pub(crate) fn shrink_to_pages(&mut self, pages: u64) -> Result<()> {
        let new_byte_len = pages_to_bytes(pages)?;
        ensure!(new_byte_len <= self.byte_len, "tmemory shrink cannot grow");
        let old_byte_len = self.byte_len;
        let old_granule_len = self.granule_len();
        let new_granule_len = granules_for_bytes(new_byte_len);

        if new_byte_len < old_byte_len {
            self.region.fill(new_byte_len..old_byte_len, 0)?;
        }

        if new_granule_len < old_granule_len {
            for info in &mut self.granules[new_granule_len..old_granule_len] {
                *info = TMemoryGranuleInfo::default();
            }
        }

        self.byte_len = new_byte_len;
        Ok(())
    }

    pub(crate) fn txn_info(&self, granule: usize) -> Result<TMemoryGranuleInfo> {
        ensure!(
            granule < self.granule_len(),
            "tmemory granule out of bounds"
        );
        Ok(self.granules[granule])
    }

    pub(crate) fn copy_granule(&self, granule: usize) -> Result<Vec<u8>> {
        ensure!(
            granule < self.granule_len(),
            "tmemory granule out of bounds"
        );
        let range = self.granule_range(granule)?;
        self.region.read(range.start, range.end - range.start)
    }

    pub(crate) fn write_granule(&mut self, granule: usize, bytes: &[u8]) -> Result<()> {
        ensure!(
            granule < self.granule_len(),
            "tmemory granule out of bounds"
        );
        let range = self.granule_range(granule)?;
        ensure!(
            bytes.len() == range.end - range.start,
            "tmemory granule writeback length mismatch"
        );
        self.region.write(range.start, bytes)
    }

    pub(crate) fn set_txn_info(&mut self, granule: usize, info: TMemoryGranuleInfo) -> Result<()> {
        ensure!(
            granule < self.granule_len(),
            "tmemory granule out of bounds"
        );
        self.granules[granule] = info;
        Ok(())
    }

    pub(crate) fn fill(&mut self, range: core::ops::Range<usize>, byte: u8) -> Result<()> {
        ensure!(range.start <= range.end, "tmemory write invalid range");
        ensure!(range.end <= self.byte_len, "out of bounds tmemory access");
        self.region.fill(range, byte)
    }

    pub(crate) fn read(&self, range: core::ops::Range<usize>) -> Result<Vec<u8>> {
        ensure!(range.start <= range.end, "tmemory read invalid range");
        ensure!(range.end <= self.byte_len, "tmemory read out of bounds");
        self.region.read(range.start, range.end - range.start)
    }

    fn granule_range(&self, granule: usize) -> Result<core::ops::Range<usize>> {
        let start = granule
            .checked_mul(TMEMORY_GRANULE_SIZE)
            .context("tmemory granule byte offset overflow")?;
        ensure!(start < self.byte_len, "tmemory granule out of bounds");
        let end = start
            .saturating_add(TMEMORY_GRANULE_SIZE)
            .min(self.byte_len);
        Ok(start..end)
    }

    #[cfg(test)]
    pub(crate) fn block_region_block_size_for_test(&self) -> usize {
        self.region.block_size_for_test()
    }

    #[cfg(test)]
    pub(crate) fn immix_line_size_for_test(&self) -> usize {
        self.region.immix_line_size_for_test()
    }

    #[cfg(test)]
    pub(crate) fn line_mark_count_for_test(&self) -> usize {
        self.region.line_mark_count_for_test()
    }
}

impl TMemoryBackendStorage for VMemory {
    fn backend_kind(&self) -> TMemoryBackend {
        TMemoryBackend::VMemory
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
        self.region.write(addr, bytes)
    }

    fn read_within_capacity(&self, range: core::ops::Range<usize>) -> Result<Vec<u8>> {
        ensure!(range.start <= range.end, "tmemory read invalid range");
        ensure!(
            range.end <= self.byte_capacity,
            "tmemory capacity read out of bounds"
        );
        self.region.read(range.start, range.end - range.start)
    }

    fn commit_range_within_capacity(&mut self, addr: usize, bytes: &[u8]) -> Result<()> {
        let end = addr
            .checked_add(bytes.len())
            .context("tmemory write address overflow")?;
        ensure!(
            end <= self.byte_capacity,
            "tmemory capacity write out of bounds"
        );
        self.region.write(addr, bytes)
    }

    fn reserve_backing_capacity_to_pages(&mut self, new_pages: u64) -> Result<()> {
        ensure!(
            new_pages <= self.max_pages,
            "tmemory capacity growth exceeds maximum size"
        );
        let new_byte_capacity = pages_to_bytes(new_pages)?;
        ensure!(
            new_byte_capacity >= self.byte_len,
            "tmemory capacity cannot shrink below live size"
        );
        self.reserve_capacity_to_pages(new_pages)?;
        self.region.fence()
    }

    fn can_grow_to_pages(&self, new_pages: u64) -> bool {
        VMemory::can_grow_to_pages(self, new_pages)
    }

    fn grow_to_pages(&mut self, new_pages: u64) -> Result<()> {
        VMemory::grow_to_pages(self, new_pages)
    }

    fn granule_info(&self, granule: usize) -> Result<TMemoryGranuleInfo> {
        self.txn_info(granule)
    }

    fn set_granule_info(&mut self, granule: usize, info: TMemoryGranuleInfo) -> Result<()> {
        self.set_txn_info(granule, info)
    }

    fn granule_info_within_capacity(&self, granule: usize) -> Result<TMemoryGranuleInfo> {
        ensure!(
            granule < self.granules.len(),
            "tmemory capacity granule out of bounds"
        );
        Ok(self.granules[granule])
    }

    fn set_granule_info_within_capacity(
        &mut self,
        granule: usize,
        info: TMemoryGranuleInfo,
    ) -> Result<()> {
        ensure!(
            granule < self.granules.len(),
            "tmemory capacity granule out of bounds"
        );
        self.granules[granule] = info;
        Ok(())
    }

    #[cfg(test)]
    fn block_region_block_size_for_test(&self) -> usize {
        self.block_region_block_size_for_test()
    }

    #[cfg(test)]
    fn immix_line_size_for_test(&self) -> usize {
        self.immix_line_size_for_test()
    }

    #[cfg(test)]
    fn line_mark_count_for_test(&self) -> usize {
        self.line_mark_count_for_test()
    }
}

/// DAX-PMEM-capable transactional storage backed by a CLWB/SFENCE flush path.
#[derive(Debug)]
pub(crate) struct DaxPmemMemory {
    region: TMemoryRegion,
    backing: TMemoryDaxPmemBacking,
    granules: Vec<TMemoryGranuleInfo>,
    byte_len: usize,
    byte_capacity: usize,
    max_pages: u64,
}

impl DaxPmemMemory {
    pub(crate) fn new(min_pages: u64, max_pages: Option<u64>) -> Result<Self> {
        Self::new_with_backing(
            min_pages,
            max_pages,
            TMemoryPersistenceMode::ResearchPretendDaxPmem,
            TMemoryDaxPmemBacking::ResearchTemp,
        )
    }

    pub(crate) fn new_with_backing(
        min_pages: u64,
        max_pages: Option<u64>,
        persistence_mode: TMemoryPersistenceMode,
        backing: TMemoryDaxPmemBacking,
    ) -> Result<Self> {
        ensure!(
            matches!(
                (&persistence_mode, &backing),
                (
                    TMemoryPersistenceMode::ResearchPretendDaxPmem,
                    TMemoryDaxPmemBacking::ResearchTemp
                ) | (
                    TMemoryPersistenceMode::RequireDaxPmem,
                    TMemoryDaxPmemBacking::FsDaxPath(_)
                ) | (
                    TMemoryPersistenceMode::RequireDaxPmem,
                    TMemoryDaxPmemBacking::ExistingFsDaxPath(_)
                ) | (
                    TMemoryPersistenceMode::RequireDaxPmem,
                    TMemoryDaxPmemBacking::FsDaxRegions(_)
                ) | (
                    TMemoryPersistenceMode::RequireDaxPmem,
                    TMemoryDaxPmemBacking::ExistingFsDaxRegions(_)
                )
            ),
            "DAX PMEM persistence mode does not match backing"
        );
        let requested_max_pages = max_pages;
        let max_pages = max_pages.unwrap_or(DEFAULT_MAX_WASM_PAGES);
        ensure!(min_pages <= max_pages, "tmemory minimum exceeds maximum");
        let byte_len = pages_to_bytes(min_pages)?;
        let requested_byte_capacity = match requested_max_pages {
            Some(max_pages) => pages_to_bytes(max_pages)?,
            None => byte_len,
        };

        let region = match &backing {
            TMemoryDaxPmemBacking::FsDaxRegions(regions) => TMemoryRegion::new_dax_pmem_regions(
                requested_byte_capacity,
                regions.clone(),
                false,
            )?,
            TMemoryDaxPmemBacking::ExistingFsDaxRegions(regions) => {
                TMemoryRegion::new_dax_pmem_regions(requested_byte_capacity, regions.clone(), true)?
            }
            _ => TMemoryRegion::new_dax_pmem(requested_byte_capacity, backing.clone())?,
        };
        let byte_capacity = if matches!(
            backing,
            TMemoryDaxPmemBacking::ExistingFsDaxPath(_)
                | TMemoryDaxPmemBacking::ExistingFsDaxRegions(_)
        ) {
            region.logical_len().max(requested_byte_capacity)
        } else {
            requested_byte_capacity
        };
        let existing_pages = byte_capacity.div_ceil(WASM_PAGE_SIZE) as u64;
        let max_pages = max_pages.max(existing_pages);
        let granule_capacity = granules_for_bytes(byte_capacity);

        Ok(Self {
            region,
            backing,
            granules: vec![TMemoryGranuleInfo::default(); granule_capacity],
            byte_len,
            byte_capacity,
            max_pages,
        })
    }

    pub(crate) fn byte_len(&self) -> usize {
        self.byte_len
    }

    pub(crate) fn granule_len(&self) -> usize {
        granules_for_bytes(self.byte_len)
    }

    pub(crate) fn txn_info(&self, granule: usize) -> Result<TMemoryGranuleInfo> {
        ensure!(
            granule < self.granule_len(),
            "tmemory granule out of bounds"
        );
        Ok(self.granules[granule])
    }

    pub(crate) fn set_txn_info(&mut self, granule: usize, info: TMemoryGranuleInfo) -> Result<()> {
        ensure!(
            granule < self.granule_len(),
            "tmemory granule out of bounds"
        );
        self.granules[granule] = info;
        Ok(())
    }

    pub(crate) fn read(&self, range: core::ops::Range<usize>) -> Result<Vec<u8>> {
        ensure!(range.start <= range.end, "tmemory read invalid range");
        ensure!(range.end <= self.byte_len, "tmemory read out of bounds");
        self.region.read(range.start, range.end - range.start)
    }

    pub(crate) fn grow_to_pages(&mut self, new_pages: u64) -> Result<()> {
        ensure!(
            new_pages <= self.max_pages,
            "tmemory growth exceeds maximum size"
        );
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

        self.region.reserve_capacity(new_byte_capacity)?;

        let new_granule_capacity = granules_for_bytes(new_byte_capacity);
        self.granules
            .resize(new_granule_capacity, TMemoryGranuleInfo::default());

        self.byte_capacity = new_byte_capacity;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn block_region_block_size_for_test(&self) -> usize {
        self.region.block_size_for_test()
    }

    #[cfg(test)]
    pub(crate) fn immix_line_size_for_test(&self) -> usize {
        self.region.immix_line_size_for_test()
    }

    #[cfg(test)]
    pub(crate) fn line_mark_count_for_test(&self) -> usize {
        self.region.line_mark_count_for_test()
    }
}

impl TMemoryBackendStorage for DaxPmemMemory {
    fn backend_kind(&self) -> TMemoryBackend {
        TMemoryBackend::DaxPmem
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

    fn read_within_capacity(&self, range: core::ops::Range<usize>) -> Result<Vec<u8>> {
        ensure!(range.start <= range.end, "tmemory read invalid range");
        ensure!(
            range.end <= self.byte_capacity,
            "tmemory capacity read out of bounds"
        );
        self.region.read(range.start, range.end - range.start)
    }

    fn commit_range_within_capacity(&mut self, addr: usize, bytes: &[u8]) -> Result<()> {
        let end = addr
            .checked_add(bytes.len())
            .context("tmemory write address overflow")?;
        ensure!(
            end <= self.byte_capacity,
            "tmemory capacity write out of bounds"
        );
        self.region.write(addr, bytes)?;
        self.region.flush(addr, bytes.len())?;
        self.region.fence()
    }

    fn reserve_backing_capacity_to_pages(&mut self, new_pages: u64) -> Result<()> {
        ensure!(
            new_pages <= self.max_pages,
            "tmemory capacity growth exceeds maximum size"
        );
        let new_byte_capacity = pages_to_bytes(new_pages)?;
        ensure!(
            new_byte_capacity >= self.byte_len,
            "tmemory capacity cannot shrink below live size"
        );
        self.reserve_capacity_to_pages(new_pages)?;
        self.region.fence()
    }

    fn can_grow_to_pages(&self, new_pages: u64) -> bool {
        DaxPmemMemory::can_grow_to_pages(self, new_pages)
    }

    fn grow_to_pages(&mut self, new_pages: u64) -> Result<()> {
        DaxPmemMemory::grow_to_pages(self, new_pages)
    }

    fn granule_info(&self, granule: usize) -> Result<TMemoryGranuleInfo> {
        self.txn_info(granule)
    }

    fn set_granule_info(&mut self, granule: usize, info: TMemoryGranuleInfo) -> Result<()> {
        self.set_txn_info(granule, info)
    }

    fn granule_info_within_capacity(&self, granule: usize) -> Result<TMemoryGranuleInfo> {
        ensure!(
            granule < self.granules.len(),
            "tmemory capacity granule out of bounds"
        );
        Ok(self.granules[granule])
    }

    fn set_granule_info_within_capacity(
        &mut self,
        granule: usize,
        info: TMemoryGranuleInfo,
    ) -> Result<()> {
        ensure!(
            granule < self.granules.len(),
            "tmemory capacity granule out of bounds"
        );
        self.granules[granule] = info;
        Ok(())
    }

    #[cfg(test)]
    fn block_region_block_size_for_test(&self) -> usize {
        self.block_region_block_size_for_test()
    }

    #[cfg(test)]
    fn immix_line_size_for_test(&self) -> usize {
        self.immix_line_size_for_test()
    }

    #[cfg(test)]
    fn line_mark_count_for_test(&self) -> usize {
        self.line_mark_count_for_test()
    }
}

/// Filesystem-backed transactional storage backed by a file-mapped block region.
#[derive(Debug)]
pub(crate) struct FileBackedMemory {
    region: TMemoryRegion,
    file_backing: TMemoryFileBacking,
    granules: Vec<TMemoryGranuleInfo>,
    byte_len: usize,
    byte_capacity: usize,
    max_pages: u64,
}

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
        let requested_byte_capacity = match requested_max_pages {
            Some(max_pages) => pages_to_bytes(max_pages)?,
            None => byte_len,
        };
        let (region, byte_capacity) = match &file_backing {
            TMemoryFileBacking::Regions(regions) => {
                let region = TMemoryRegion::new_file_backed_regions(
                    requested_byte_capacity,
                    regions.clone(),
                    false,
                )?;
                (region, requested_byte_capacity)
            }
            TMemoryFileBacking::ExistingRegions(regions) => {
                let region = TMemoryRegion::new_file_backed_regions(
                    requested_byte_capacity,
                    regions.clone(),
                    true,
                )?;
                let byte_capacity = region.logical_len().max(requested_byte_capacity);
                (region, byte_capacity)
            }
            _ => {
                let byte_capacity =
                    existing_file_backed_len(&file_backing)?.unwrap_or(requested_byte_capacity);
                let byte_capacity = byte_capacity.max(requested_byte_capacity);
                let region = TMemoryRegion::new_file_backed(
                    byte_capacity,
                    file_backed_region_mode(&file_backing)?,
                )?;
                (region, byte_capacity)
            }
        };
        let existing_pages = byte_capacity.div_ceil(WASM_PAGE_SIZE) as u64;
        let max_pages = max_pages.max(existing_pages);
        let granule_capacity = granules_for_bytes(byte_capacity);

        Ok(Self {
            region,
            file_backing,
            granules: vec![TMemoryGranuleInfo::default(); granule_capacity],
            byte_len,
            byte_capacity,
            max_pages,
        })
    }

    pub(crate) fn byte_len(&self) -> usize {
        self.byte_len
    }

    pub(crate) fn granule_len(&self) -> usize {
        granules_for_bytes(self.byte_len)
    }

    pub(crate) fn read(&self, range: core::ops::Range<usize>) -> Result<Vec<u8>> {
        ensure!(range.start <= range.end, "tmemory read invalid range");
        ensure!(range.end <= self.byte_len, "tmemory read out of bounds");
        self.region.read(range.start, range.end - range.start)
    }

    pub(crate) fn grow_to_pages(&mut self, new_pages: u64) -> Result<()> {
        ensure!(
            new_pages <= self.max_pages,
            "tmemory growth exceeds maximum size"
        );
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

        self.region.reserve_capacity(new_byte_capacity)?;

        let new_granule_capacity = granules_for_bytes(new_byte_capacity);
        self.granules
            .resize(new_granule_capacity, TMemoryGranuleInfo::default());

        self.byte_capacity = new_byte_capacity;
        Ok(())
    }

    pub(crate) fn txn_info(&self, granule: usize) -> Result<TMemoryGranuleInfo> {
        ensure!(
            granule < self.granule_len(),
            "tmemory granule out of bounds"
        );
        Ok(self.granules[granule])
    }

    pub(crate) fn set_txn_info(&mut self, granule: usize, info: TMemoryGranuleInfo) -> Result<()> {
        ensure!(
            granule < self.granule_len(),
            "tmemory granule out of bounds"
        );
        self.granules[granule] = info;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn block_region_block_size_for_test(&self) -> usize {
        self.region.block_size_for_test()
    }

    #[cfg(test)]
    pub(crate) fn immix_line_size_for_test(&self) -> usize {
        self.region.immix_line_size_for_test()
    }

    #[cfg(test)]
    pub(crate) fn line_mark_count_for_test(&self) -> usize {
        self.region.line_mark_count_for_test()
    }
}

fn existing_file_backed_len(file_backing: &TMemoryFileBacking) -> Result<Option<usize>> {
    let TMemoryFileBacking::ExistingPath(path) = file_backing else {
        return Ok(None);
    };
    let len = std::fs::metadata(path)
        .with_context(|| {
            format!(
                "failed to stat existing file-backed tmemory {}",
                path.display()
            )
        })?
        .len();
    usize::try_from(len)
        .map(Some)
        .context("existing file-backed tmemory is too large for this host")
}

fn file_backed_region_mode(
    file_backing: &TMemoryFileBacking,
) -> Result<block_region::FileBackedRegionMode> {
    Ok(match file_backing {
        TMemoryFileBacking::Temp => block_region::FileBackedRegionMode::Temp,
        TMemoryFileBacking::Path(path) => block_region::FileBackedRegionMode::Path(path.clone()),
        TMemoryFileBacking::ExistingPath(path) => {
            block_region::FileBackedRegionMode::OpenExistingPath(path.clone())
        }
        TMemoryFileBacking::Regions(_) | TMemoryFileBacking::ExistingRegions(_) => {
            bail!("multi-region file-backed tmemory is not implemented yet")
        }
    })
}

impl TMemoryBackendStorage for FileBackedMemory {
    fn backend_kind(&self) -> TMemoryBackend {
        TMemoryBackend::FileBackedMemory
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
        if bytes.is_empty() {
            return Ok(());
        }
        self.region.write(addr, bytes)?;
        self.region.flush(addr, bytes.len())?;
        self.region.fence()
    }

    fn read_within_capacity(&self, range: core::ops::Range<usize>) -> Result<Vec<u8>> {
        ensure!(range.start <= range.end, "tmemory read invalid range");
        ensure!(
            range.end <= self.byte_capacity,
            "tmemory capacity read out of bounds"
        );
        self.region.read(range.start, range.end - range.start)
    }

    fn commit_range_within_capacity(&mut self, addr: usize, bytes: &[u8]) -> Result<()> {
        let end = addr
            .checked_add(bytes.len())
            .context("tmemory write address overflow")?;
        ensure!(
            end <= self.byte_capacity,
            "tmemory capacity write out of bounds"
        );
        if bytes.is_empty() {
            return Ok(());
        }
        self.region.write(addr, bytes)?;
        self.region.flush(addr, bytes.len())?;
        self.region.fence()
    }

    fn reserve_backing_capacity_to_pages(&mut self, new_pages: u64) -> Result<()> {
        ensure!(
            new_pages <= self.max_pages,
            "tmemory capacity growth exceeds maximum size"
        );
        let new_byte_capacity = pages_to_bytes(new_pages)?;
        ensure!(
            new_byte_capacity >= self.byte_len,
            "tmemory capacity cannot shrink below live size"
        );
        self.reserve_capacity_to_pages(new_pages)?;
        self.region.fence()
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

    fn granule_info_within_capacity(&self, granule: usize) -> Result<TMemoryGranuleInfo> {
        ensure!(
            granule < self.granules.len(),
            "tmemory capacity granule out of bounds"
        );
        Ok(self.granules[granule])
    }

    fn set_granule_info_within_capacity(
        &mut self,
        granule: usize,
        info: TMemoryGranuleInfo,
    ) -> Result<()> {
        ensure!(
            granule < self.granules.len(),
            "tmemory capacity granule out of bounds"
        );
        self.granules[granule] = info;
        Ok(())
    }

    #[cfg(test)]
    fn block_region_block_size_for_test(&self) -> usize {
        self.block_region_block_size_for_test()
    }

    #[cfg(test)]
    fn immix_line_size_for_test(&self) -> usize {
        self.immix_line_size_for_test()
    }

    #[cfg(test)]
    fn line_mark_count_for_test(&self) -> usize {
        self.line_mark_count_for_test()
    }
}

fn pages_to_bytes(pages: u64) -> Result<usize> {
    let pages = usize::try_from(pages).context("tmemory page count does not fit host usize")?;
    pages
        .checked_mul(WASM_PAGE_SIZE)
        .context("tmemory byte length overflow")
}

fn granules_for_bytes(bytes: usize) -> usize {
    bytes.div_ceil(TMEMORY_GRANULE_SIZE)
}

fn tmemory_granule_addr(granule_index: u64) -> Result<usize> {
    let granule_index =
        usize::try_from(granule_index).context("tmemory granule index does not fit host usize")?;
    granule_index
        .checked_mul(TMEMORY_GRANULE_SIZE)
        .context("tmemory granule byte offset overflow")
}

fn tmemory_granule_range(granule_index: usize, byte_len: usize) -> Result<core::ops::Range<usize>> {
    let start = granule_index
        .checked_mul(TMEMORY_GRANULE_SIZE)
        .context("tmemory granule byte offset overflow")?;
    ensure!(start < byte_len, "tmemory granule out of bounds");
    let end = start.saturating_add(TMEMORY_GRANULE_SIZE).min(byte_len);
    Ok(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, size_of};

    const GRANULES_PER_WASM_PAGE: usize = WASM_PAGE_SIZE / TMEMORY_GRANULE_SIZE;

    #[test]
    fn wizard_layout_constants_match_reference() {
        assert_eq!(block_region::BLOCK_SIZE, 512 * 1024);
        assert_eq!(block_region::IMMIX_LINE_SIZE, 256);
        assert_eq!(block_region::PW_REGION_HEADER_SIZE, 56);
        assert_eq!(block_region::META_DATA_DESC_SIZE, 32);
        assert_eq!(block_region::BLOCK_ENTRY_SIZE, 40);
        assert_eq!(block_region::CHUNK_HEADER_SIZE, 32);
        assert_eq!(block_region::LINE_MARK_SIZE, 1);
    }

    #[test]
    fn durable_header_sizes_match_design() {
        assert_eq!(REGION_MAGIC, 0x5452_4547);
        assert_eq!(LOG_BLOCK_MAGIC, 0x544c_4f47);
        assert_eq!(DATA_CHUNK_MAGIC, 0x5444_4154);
        let _ = LogBlockHeader {
            magic: LOG_BLOCK_MAGIC,
            stream_id: 7,
            block_seq: 3,
            next_block: NO_NEXT_BLOCK,
            entry_count: 19,
        };
        let _ = DataChunkHeader {
            magic: DATA_CHUNK_MAGIC,
            stream_id: 7,
            chunk_seq: 4,
            next_chunk: NO_NEXT_BLOCK,
            chunk_blocks: 2,
            tail_block_delta: 1,
            tail_in_block: 96,
        };
        assert_eq!(size_of::<RegionHeader>(), 32);
        assert_eq!(size_of::<LogBlockHeader>(), 20);
        assert_eq!(size_of::<DataChunkHeader>(), 28);
        assert_eq!(size_of::<TxLogEntry>(), 32);
        assert_eq!(align_of::<TxLogEntry>(), 32);
        assert_eq!(size_of::<TxDataRecordHeader>(), 24);
    }

    #[test]
    fn region_header_roundtrips() {
        let header = RegionHeader {
            magic: REGION_MAGIC,
            block_size: 512 * 1024,
            num_blocks: 17,
            block_table_start_block: 1,
            block_table_block_count: 2,
            metadata_descs_start_block: 3,
            metadata_descs_block_count: 4,
            num_descs: 9,
        };
        let bytes = header.as_bytes();
        let decoded = RegionHeader::from_bytes(bytes).unwrap();
        assert_eq!(decoded, header);
    }

    #[test]
    fn log_block_header_roundtrips() {
        let header = LogBlockHeader {
            magic: LOG_BLOCK_MAGIC,
            stream_id: 5,
            block_seq: 11,
            next_block: NO_NEXT_BLOCK,
            entry_count: 23,
        };
        let bytes = header.as_bytes();
        let decoded = LogBlockHeader::from_bytes(bytes).unwrap();
        assert_eq!(decoded, header);
    }

    #[test]
    fn data_chunk_header_roundtrips() {
        let header = DataChunkHeader {
            magic: DATA_CHUNK_MAGIC,
            stream_id: 7,
            chunk_seq: 13,
            next_chunk: NO_NEXT_BLOCK,
            chunk_blocks: 4,
            tail_block_delta: 2,
            tail_in_block: 128,
        };
        let bytes = header.as_bytes();
        let decoded = DataChunkHeader::from_bytes(bytes).unwrap();
        assert_eq!(decoded, header);
    }

    #[test]
    fn tx_log_entry_crc_detects_bitflip() {
        let mut entry = TxLogEntry::new(0x6000_0000_0000_002a, 7, (11 << 1) | 1, 19, 96);
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
            role: 0,
            payload_len: 0x0001_0203,
            type_info: 0x8001_0002,
        };
        let bytes = header.as_bytes();
        let decoded = TxDataRecordHeader::from_bytes(bytes).unwrap();
        assert_eq!(decoded, header);
    }

    #[test]
    fn tx_data_record_header_rejects_wrong_len() {
        let err = TxDataRecordHeader::from_bytes([0u8; 23]).unwrap_err();
        assert!(
            err.to_string()
                .contains("durable tx data record header length mismatch")
        );
    }

    #[test]
    fn tmemory_uses_configured_vmemory_backend() {
        let memory = TMemory::new(TransactionConfig::default(), 1, Some(1)).unwrap();

        assert_eq!(memory.backend(), TMemoryBackend::VMemory);
        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE);
        assert_eq!(memory.granule_len(), GRANULES_PER_WASM_PAGE);
    }

    #[test]
    fn tmemory_can_construct_dax_pmem_in_research_mode() {
        let memory = TMemory::new_with_backend_limits(TMemoryBackend::DaxPmem, 1, Some(1)).unwrap();

        assert_eq!(memory.backend(), TMemoryBackend::DaxPmem);
        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE);
        assert_eq!(memory.granule_len(), GRANULES_PER_WASM_PAGE);
    }

    #[test]
    fn tmemory_can_construct_dax_pmem_research_backend_from_config() {
        let config = TransactionConfig::with_tmemory_backend(TMemoryBackend::DaxPmem).unwrap();
        let memory = TMemory::new(config, 1, Some(1)).unwrap();

        assert_eq!(memory.backend(), TMemoryBackend::DaxPmem);
        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE);
        assert_eq!(memory.granule_len(), GRANULES_PER_WASM_PAGE);
    }

    #[test]
    fn tmemory_vmemory_uses_block_region_storage() {
        let memory = TMemory::new_vmemory_with_limits(0, Some(0)).unwrap();

        assert_eq!(
            memory.block_region_block_size_for_test(),
            block_region::BLOCK_SIZE
        );
        assert_eq!(
            memory.immix_line_size_for_test(),
            block_region::IMMIX_LINE_SIZE
        );
        assert_eq!(memory.line_mark_count_for_test(), 0);
    }

    #[test]
    fn tmemory_commit_and_read_crosses_block_region_chunk_boundary() {
        let mut memory = TMemory::new_vmemory_with_limits(9, Some(9)).unwrap();

        assert_eq!(
            memory.block_region_block_size_for_test(),
            block_region::BLOCK_SIZE
        );
        assert_eq!(
            memory.line_mark_count_for_test(),
            2 * (block_region::BLOCK_SIZE / block_region::IMMIX_LINE_SIZE)
        );

        let boundary = block_region::BLOCK_SIZE;
        memory
            .commit_range(boundary - 2, &[0x11, 0x22, 0x33, 0x44])
            .unwrap();

        assert_eq!(
            memory.read_committed(boundary - 2..boundary + 2).unwrap(),
            vec![0x11, 0x22, 0x33, 0x44]
        );
    }

    #[test]
    fn vmemory_commit_and_read_committed_bytes() {
        let mut memory = TMemory::new_vmemory(2).unwrap();

        assert_eq!(memory.backend_kind(), TMemoryBackend::VMemory);
        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE * 2);
        assert_eq!(memory.granule_count(), GRANULES_PER_WASM_PAGE * 2);

        memory.commit_range(8, &[0xaa, 0xbb, 0xcc, 0xdd]).unwrap();

        assert_eq!(
            memory.read_committed(6..14).unwrap(),
            vec![0x00, 0x00, 0xaa, 0xbb, 0xcc, 0xdd, 0x00, 0x00]
        );
    }

    #[test]
    fn dax_pmem_commit_range_writes_and_versions_granules() {
        let mut memory =
            TMemory::new_with_backend_limits(TMemoryBackend::DaxPmem, 1, Some(1)).unwrap();

        memory.commit_range(4, &[10, 11, 12, 13]).unwrap();

        assert_eq!(memory.read_committed(4..8).unwrap(), vec![10, 11, 12, 13]);
        assert_eq!(memory.granule_version(0).unwrap(), 1);
    }

    #[test]
    fn dax_pmem_prepare_undo_record_does_not_publish_or_write() {
        let mut memory =
            TMemory::new_with_backend_limits(TMemoryBackend::DaxPmem, 1, Some(1)).unwrap();
        memory.commit_range(0, &[1, 2, 3, 4]).unwrap();

        let undo = memory
            .prepare_tmemory_undo_record(Some(3), 0, 0, &[9; TMEMORY_GRANULE_SIZE])
            .unwrap();

        assert_eq!(memory.read_committed(0..4).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(undo.version, 2);
        assert_eq!(undo.old_granule_bytes[..4], [1, 2, 3, 4]);
        assert_eq!(
            undo.logical_id,
            pack_tmemory_granule_id(Some(3), 0, 0).unwrap()
        );
    }

    #[test]
    fn vmemory_commit_staged_granules_keeps_volatile_commit_path() {
        let mut memory = TMemory::new_vmemory(1).unwrap();

        memory
            .commit_staged_tmemory_granules_direct(&[(0, vec![9; TMEMORY_GRANULE_SIZE])])
            .unwrap();

        assert_eq!(memory.read_committed(0..4).unwrap(), vec![9, 9, 9, 9]);
    }

    #[test]
    fn recovered_tmemory_undo_rollbacks_restore_base_bytes() {
        let mut memory =
            TMemory::new_with_backend_limits(TMemoryBackend::DaxPmem, 1, Some(1)).unwrap();
        memory.commit_range(0, &[9, 9, 9, 9]).unwrap();
        let rollback = recovery::RecoveredTMemoryUndoRollback {
            logical_id: pack_tmemory_granule_id(Some(3), 0, 0).unwrap(),
            version: 1,
            data_block: 0,
            data_offset: 0,
            old_granule_bytes: vec![1; TMEMORY_GRANULE_SIZE],
        };

        memory
            .apply_recovered_tmemory_undo_rollbacks([rollback])
            .unwrap();

        assert_eq!(memory.read_committed(0..4).unwrap(), vec![1, 1, 1, 1]);
        assert_eq!(memory.granule_version(0).unwrap(), 1);
    }

    #[test]
    fn recovered_tmemory_undo_rollbacks_choose_oldest_version_for_same_granule() {
        let mut memory =
            TMemory::new_with_backend_limits(TMemoryBackend::DaxPmem, 1, Some(1)).unwrap();
        memory.commit_range(0, &[9, 9, 9, 9]).unwrap();
        let logical_id = pack_tmemory_granule_id(Some(3), 0, 0).unwrap();
        let older = recovery::RecoveredTMemoryUndoRollback {
            logical_id,
            version: 2,
            data_block: 0,
            data_offset: 0,
            old_granule_bytes: vec![1; TMEMORY_GRANULE_SIZE],
        };
        let newer = recovery::RecoveredTMemoryUndoRollback {
            logical_id,
            version: 3,
            data_block: 0,
            data_offset: 0,
            old_granule_bytes: vec![3; TMEMORY_GRANULE_SIZE],
        };

        memory
            .apply_recovered_tmemory_undo_rollbacks([older, newer])
            .unwrap();

        assert_eq!(memory.read_committed(0..4).unwrap(), vec![1, 1, 1, 1]);
    }

    #[test]
    fn commit_range_increments_touched_granule_versions_and_clears_owner() {
        let mut memory = TMemory::new_vmemory(1).unwrap();

        memory
            .set_granule_info(
                0,
                TMemoryGranuleInfo {
                    owner: 11,
                    version: 2,
                    hash: 33,
                },
            )
            .unwrap();
        memory
            .set_granule_info(
                1,
                TMemoryGranuleInfo {
                    owner: 22,
                    version: 7,
                    hash: 44,
                },
            )
            .unwrap();
        memory
            .set_granule_info(
                2,
                TMemoryGranuleInfo {
                    owner: 99,
                    version: 1,
                    hash: 55,
                },
            )
            .unwrap();

        memory
            .commit_range(TMEMORY_GRANULE_SIZE - 1, &[0xaa, 0xbb])
            .unwrap();

        assert_eq!(
            memory.granule_info(0).unwrap(),
            TMemoryGranuleInfo {
                owner: 0,
                version: 3,
                hash: 33,
            }
        );
        assert_eq!(
            memory.granule_info(1).unwrap(),
            TMemoryGranuleInfo {
                owner: 0,
                version: 8,
                hash: 44,
            }
        );
        assert_eq!(
            memory.granule_info(2).unwrap(),
            TMemoryGranuleInfo {
                owner: 99,
                version: 1,
                hash: 55,
            }
        );
    }

    #[test]
    fn vmemory_direct_constructor_with_limits_reserves_capacity_and_grows() {
        let mut memory = TMemory::new_vmemory_with_limits(1, Some(2)).unwrap();

        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE);
        assert_eq!(memory.byte_capacity(), WASM_PAGE_SIZE * 2);

        memory.grow_to_pages(2).unwrap();

        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE * 2);
        assert_eq!(memory.byte_capacity(), WASM_PAGE_SIZE * 2);
    }

    #[test]
    fn dax_pmem_direct_constructor_with_limits_reserves_capacity_and_grows() {
        let mut memory =
            TMemory::new_with_backend_limits(TMemoryBackend::DaxPmem, 1, Some(2)).unwrap();

        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE);
        assert_eq!(memory.byte_capacity(), WASM_PAGE_SIZE * 2);

        memory.grow_to_pages(2).unwrap();

        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE * 2);
        assert_eq!(memory.byte_capacity(), WASM_PAGE_SIZE * 2);
    }

    #[test]
    fn vmemory_unbounded_constructor_does_not_eagerly_reserve_default_wasm_capacity() {
        let mut memory = TMemory::new_vmemory_with_limits(0, None).unwrap();

        assert_eq!(memory.byte_len(), 0);
        assert_eq!(memory.byte_capacity(), 0);

        memory.grow_to_pages(1).unwrap();

        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE);
        assert_eq!(memory.byte_capacity(), WASM_PAGE_SIZE);
    }

    #[test]
    fn vmemory_unbounded_grow_remaps_and_preserves_committed_state() {
        let mut memory = TMemory::new_vmemory_with_limits(1, None).unwrap();
        let info = TMemoryGranuleInfo {
            owner: 11,
            version: 7,
            hash: 33,
        };

        assert_eq!(memory.byte_capacity(), WASM_PAGE_SIZE);

        memory.commit_range(0, &[1, 2, 3, 4]).unwrap();
        memory.set_granule_info(0, info).unwrap();
        memory.grow_to_pages(2).unwrap();

        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE * 2);
        assert_eq!(memory.byte_capacity(), WASM_PAGE_SIZE * 2);
        assert_eq!(memory.read_committed(0..4).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(memory.granule_info(0).unwrap(), info);
    }

    #[test]
    fn dax_pmem_grow_beyond_capacity_preserves_bytes_and_granule_metadata() {
        let mut memory =
            TMemory::new_with_backend_limits(TMemoryBackend::DaxPmem, 1, None).unwrap();
        memory.commit_range(8, &[1, 2, 3, 4]).unwrap();
        let mut info = memory.granule_info(0).unwrap();
        info.owner = 7;
        info.hash = 99;
        memory.set_granule_info(0, info).unwrap();

        assert_eq!(memory.byte_capacity(), WASM_PAGE_SIZE);

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

    #[cfg(unix)]
    #[test]
    fn tmemory_can_construct_file_backed_temp_backend() {
        let config = TransactionConfig::with_file_backed_tmemory_temp().unwrap();
        let memory = TMemory::new(config, 1, Some(1)).unwrap();

        assert_eq!(memory.backend(), TMemoryBackend::FileBackedMemory);
        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE);
        assert_eq!(
            memory.block_region_block_size_for_test(),
            block_region::BLOCK_SIZE
        );
        assert_eq!(
            memory.immix_line_size_for_test(),
            block_region::IMMIX_LINE_SIZE
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_commit_range_writes_and_versions_granules() {
        let config = TransactionConfig::with_file_backed_tmemory_temp().unwrap();
        let mut memory = TMemory::new(config, 1, Some(1)).unwrap();

        memory.commit_range(4, &[10, 11, 12, 13]).unwrap();

        assert_eq!(memory.read_committed(4..8).unwrap(), vec![10, 11, 12, 13]);
        assert_eq!(memory.granule_version(0).unwrap(), 1);
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[test]
    fn file_backed_explicit_path_grow_extends_existing_file_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("explicit-grow.tmemory");
        let config = TransactionConfig::with_file_backed_tmemory_path(path.clone()).unwrap();
        let mut memory = TMemory::new(config, 1, None).unwrap();

        memory.commit_range(8, &[1, 2, 3, 4]).unwrap();
        memory.grow_to_pages(9).unwrap();
        memory
            .commit_range(block_region::BLOCK_SIZE + 8, &[5, 6, 7, 8])
            .unwrap();

        assert_eq!(memory.byte_len(), 9 * WASM_PAGE_SIZE);
        assert_eq!(memory.byte_capacity(), 9 * WASM_PAGE_SIZE);
        assert_eq!(memory.read_committed(8..12).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(
            memory
                .read_committed(block_region::BLOCK_SIZE + 8..block_region::BLOCK_SIZE + 12)
                .unwrap(),
            vec![5, 6, 7, 8]
        );

        let bytes = std::fs::read(path).unwrap();
        assert_eq!(&bytes[8..12], &[1, 2, 3, 4]);
        assert_eq!(
            &bytes[block_region::BLOCK_SIZE + 8..block_region::BLOCK_SIZE + 12],
            &[5, 6, 7, 8]
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_existing_tmemory_path_rejects_undersized_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing-undersized.tmemory");
        std::fs::File::create(&path)
            .unwrap()
            .set_len((WASM_PAGE_SIZE - 1) as u64)
            .unwrap();

        let config = TransactionConfig::with_file_backed_tmemory_existing_path(path).unwrap();
        let error = TMemory::new(config, 1, Some(1)).unwrap_err().to_string();

        assert!(error.contains("smaller"));
        assert!(error.contains("capacity"));
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_existing_tmemory_path_accepts_grown_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing-grown.tmemory");
        let config = TransactionConfig::with_file_backed_tmemory_path(path.clone()).unwrap();
        let mut memory = TMemory::new(config, 1, None).unwrap();

        memory.grow_to_pages(9).unwrap();
        memory
            .commit_range(block_region::BLOCK_SIZE + 8, &[9, 8, 7, 6])
            .unwrap();
        drop(memory);

        let config = TransactionConfig::with_file_backed_tmemory_existing_path(path).unwrap();
        let mut reopened = TMemory::new(config, 1, Some(1)).unwrap();
        assert_eq!(reopened.byte_capacity(), 9 * WASM_PAGE_SIZE);

        reopened.grow_to_pages(9).unwrap();
        assert_eq!(
            reopened
                .read_committed(block_region::BLOCK_SIZE + 8..block_region::BLOCK_SIZE + 12)
                .unwrap(),
            vec![9, 8, 7, 6]
        );
    }

    #[cfg(all(feature = "transaction", unix))]
    #[test]
    fn file_backed_recovery_helper_replays_loose_end_undo_to_existing_tmemory_file() {
        use crate::runtime::transaction::{StreamPublisher, TxDurableLog};

        let dir = tempfile::tempdir().unwrap();
        let tmemory_path = dir.path().join("recover.tmemory");
        let tx_log_path = dir.path().join("recover-log.bin");
        let old_granule = vec![0xaa; TMEMORY_GRANULE_SIZE];
        let new_granule = vec![0x11; TMEMORY_GRANULE_SIZE];
        let mut memory = TMemory::new(
            TransactionConfig::with_file_backed_tmemory_path(tmemory_path.clone()).unwrap(),
            1,
            Some(1),
        )
        .unwrap();
        memory
            .commit_staged_tmemory_granule(1, &old_granule)
            .unwrap();
        let undo = memory
            .prepare_tmemory_undo_record(Some(0), 0, 1, &new_granule)
            .unwrap();
        memory
            .commit_staged_tmemory_granule(1, &new_granule)
            .unwrap();
        drop(memory);

        let mut log = TxDurableLog::create_file_backed(&tx_log_path, 32).unwrap();
        let mut sink = log.stream_sink(41);
        let mut publisher = StreamPublisher::new(&mut sink, 41, 41);
        publisher
            .publish_tmemory_undo_before_in_place_write(&undo)
            .unwrap();
        drop(sink);
        drop(log);

        crate::_internal::transaction_persistence::recover_file_backed_tmemory_for_test(
            &tx_log_path,
            &tmemory_path,
            1,
            Some(1),
        )
        .unwrap();

        let reopened = TMemory::new(
            TransactionConfig::with_file_backed_tmemory_existing_path(tmemory_path).unwrap(),
            1,
            Some(1),
        )
        .unwrap();
        assert_eq!(
            reopened
                .read_committed(TMEMORY_GRANULE_SIZE..TMEMORY_GRANULE_SIZE + 4)
                .unwrap(),
            vec![0xaa, 0xaa, 0xaa, 0xaa]
        );
    }

    #[cfg(all(feature = "transaction", unix))]
    #[test]
    fn file_backed_recovery_restores_hidden_growth_tail_without_mvcc_feature() {
        use crate::runtime::transaction::{StreamPublisher, TxDurableLog};

        let dir = tempfile::tempdir().unwrap();
        let tmemory_path = dir.path().join("recover-hidden-tail.tmemory");
        let tx_log_path = dir.path().join("recover-hidden-tail-log.bin");
        let tail_granule = u64::try_from(WASM_PAGE_SIZE / TMEMORY_GRANULE_SIZE).unwrap();
        let tail = usize::try_from(tail_granule).unwrap();
        let range = tmemory_granule_range(tail, 2 * WASM_PAGE_SIZE).unwrap();
        let aborted_tail = vec![0xa5; TMEMORY_GRANULE_SIZE];
        let mut memory = TMemory::new(
            TransactionConfig::with_file_backed_tmemory_path(tmemory_path.clone()).unwrap(),
            1,
            None,
        )
        .unwrap();
        memory.storage.reserve_backing_capacity_to_pages(2).unwrap();
        let old_tail = memory.storage.read_within_capacity(range.clone()).unwrap();
        let logical_id = pack_tmemory_granule_id(Some(0), 0, tail_granule).unwrap();
        let undo = PendingGranuleUndo::tmemory(logical_id, 1, old_tail);
        memory
            .storage
            .commit_range_within_capacity(range.start, &aborted_tail)
            .unwrap();
        let mut info = memory.storage.granule_info_within_capacity(tail).unwrap();
        info.version = 1;
        memory
            .storage
            .set_granule_info_within_capacity(tail, info)
            .unwrap();
        drop(memory);

        let mut log = TxDurableLog::create_file_backed(&tx_log_path, 32).unwrap();
        {
            let mut sink = log.stream_sink(42);
            let mut publisher = StreamPublisher::new(&mut sink, 42, 42);
            publisher
                .publish_tmemory_undo_before_in_place_write(&undo)
                .unwrap();
        }
        drop(log);

        crate::_internal::transaction_persistence::recover_file_backed_tmemory_for_test(
            &tx_log_path,
            &tmemory_path,
            1,
            None,
        )
        .unwrap();

        let mut reopened = TMemory::new(
            TransactionConfig::with_file_backed_tmemory_existing_path(tmemory_path).unwrap(),
            1,
            None,
        )
        .unwrap();
        assert_eq!(reopened.byte_len(), WASM_PAGE_SIZE);
        reopened.grow_to_pages(2).unwrap();
        assert_eq!(
            reopened
                .read_committed(WASM_PAGE_SIZE..WASM_PAGE_SIZE + TMEMORY_GRANULE_SIZE)
                .unwrap(),
            vec![0; TMEMORY_GRANULE_SIZE]
        );
    }

    #[cfg(all(feature = "transaction-mvcc", unix))]
    #[test]
    fn mvcc_file_backed_lp_committed_growth_tail_recovers() {
        use crate::runtime::transaction::{StreamPublisher, TxDurableLog};

        let dir = tempfile::tempdir().unwrap();
        let tmemory_path = dir.path().join("committed-growth.tmemory");
        let tx_log_path = dir.path().join("committed-growth.txlog");
        let tail_granule = u64::try_from(WASM_PAGE_SIZE / TMEMORY_GRANULE_SIZE).unwrap();
        let committed_tail = vec![0x5a; TMEMORY_GRANULE_SIZE];
        let mut memory = TMemory::new(
            TransactionConfig::with_file_backed_tmemory_path(tmemory_path.clone()).unwrap(),
            1,
            None,
        )
        .unwrap();
        memory
            .reserve_backing_capacity_to_pages_for_mvcc(2)
            .unwrap();
        let undo = memory
            .prepare_tmemory_undo_record_within_capacity_for_mvcc(
                Some(0),
                0,
                tail_granule,
                &committed_tail,
            )
            .unwrap();

        let mut log = TxDurableLog::create_file_backed(&tx_log_path, 32).unwrap();
        {
            let mut sink = log.stream_sink(51);
            let mut publisher = StreamPublisher::new(&mut sink, 51, 51);
            publisher
                .publish_tmemory_undo_before_in_place_write(&undo)
                .unwrap()
        };
        memory
            .commit_staged_tmemory_granule_within_capacity_for_mvcc(tail_granule, &committed_tail)
            .unwrap();
        let size =
            crate::runtime::transaction::PendingPublication::tmemory_size(None, 0, 1, 2).unwrap();
        let final_marker = {
            let mut sink = log.stream_sink(51);
            let mut publisher = StreamPublisher::new(&mut sink, 51, 51);
            publisher
                .publish_object_publication_before_commit(&size)
                .unwrap()
        };
        {
            let mut sink = log.stream_sink(51);
            let mut publisher = StreamPublisher::new(&mut sink, 51, 51);
            publisher.publish_commit_lp(final_marker).unwrap();
        }
        drop(log);
        drop(memory);

        let recovered =
            block_region::reopen_and_recover_file_backed_region_for_test(&tx_log_path).unwrap();
        assert_eq!(
            recovered.committed_file_backed_tmemory_pages().unwrap(),
            Some(2)
        );
        assert!(recovered.tmemory_undo_rollbacks.is_empty());
        crate::_internal::transaction_persistence::recover_file_backed_tmemory_for_test(
            &tx_log_path,
            &tmemory_path,
            1,
            None,
        )
        .unwrap();
        let reopened = TMemory::new(
            TransactionConfig::with_file_backed_tmemory_existing_path(tmemory_path).unwrap(),
            2,
            None,
        )
        .unwrap();
        assert_eq!(
            reopened
                .read_committed(WASM_PAGE_SIZE..WASM_PAGE_SIZE + TMEMORY_GRANULE_SIZE)
                .unwrap(),
            committed_tail
        );
    }

    #[cfg(all(feature = "transaction-mvcc", unix))]
    #[test]
    fn mvcc_file_backed_lp_loose_growth_tail_undo_recovers() {
        use crate::runtime::transaction::{StreamPublisher, TxDurableLog};

        let dir = tempfile::tempdir().unwrap();
        let tmemory_path = dir.path().join("loose-growth.tmemory");
        let tx_log_path = dir.path().join("loose-growth.txlog");
        let tail_granule = u64::try_from(WASM_PAGE_SIZE / TMEMORY_GRANULE_SIZE).unwrap();
        let aborted_tail = vec![0xa5; TMEMORY_GRANULE_SIZE];
        let mut memory = TMemory::new(
            TransactionConfig::with_file_backed_tmemory_path(tmemory_path.clone()).unwrap(),
            1,
            None,
        )
        .unwrap();
        memory
            .reserve_backing_capacity_to_pages_for_mvcc(2)
            .unwrap();
        let undo = memory
            .prepare_tmemory_undo_record_within_capacity_for_mvcc(
                Some(0),
                0,
                tail_granule,
                &aborted_tail,
            )
            .unwrap();

        let mut log = TxDurableLog::create_file_backed(&tx_log_path, 32).unwrap();
        {
            let mut sink = log.stream_sink(52);
            let mut publisher = StreamPublisher::new(&mut sink, 52, 52);
            publisher
                .publish_tmemory_undo_before_in_place_write(&undo)
                .unwrap();
        }
        memory
            .commit_staged_tmemory_granule_within_capacity_for_mvcc(tail_granule, &aborted_tail)
            .unwrap();
        drop(log);
        drop(memory);

        let recovered =
            block_region::reopen_and_recover_file_backed_region_for_test(&tx_log_path).unwrap();
        assert_eq!(
            recovered.committed_file_backed_tmemory_pages().unwrap(),
            None
        );
        assert_eq!(recovered.tmemory_undo_rollbacks.len(), 1);
        crate::_internal::transaction_persistence::recover_file_backed_tmemory_for_test(
            &tx_log_path,
            &tmemory_path,
            1,
            None,
        )
        .unwrap();
        let mut reopened = TMemory::new(
            TransactionConfig::with_file_backed_tmemory_existing_path(tmemory_path).unwrap(),
            1,
            None,
        )
        .unwrap();
        reopened.grow_to_pages(2).unwrap();
        assert_eq!(
            reopened
                .read_committed(WASM_PAGE_SIZE..WASM_PAGE_SIZE + TMEMORY_GRANULE_SIZE)
                .unwrap(),
            vec![0; TMEMORY_GRANULE_SIZE]
        );
    }

    #[test]
    fn ordinary_logical_bounds_reject_reserved_capacity_addresses() {
        let mut memory = TMemory::new_vmemory_with_limits(1, Some(2)).unwrap();

        assert!(
            memory
                .read_committed(WASM_PAGE_SIZE..WASM_PAGE_SIZE + 1)
                .is_err()
        );
        assert!(memory.commit_range(WASM_PAGE_SIZE, &[1]).is_err());
        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dax_pmem_fsdax_path_rejects_non_dax_storage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-dax.tmemory");
        let config = TransactionConfig::with_dax_pmem_fsdax_path(path).unwrap();
        let error = TMemory::new(config, 1, Some(1)).unwrap_err().to_string();

        assert!(
            error.contains("does not have the DAX inode attribute")
                || error.contains("failed to statx DAX PMEM fsdax path"),
            "{error}"
        );
    }

    #[test]
    fn dax_pmem_research_growth_preserves_committed_bytes() {
        let mut memory =
            TMemory::new_with_backend_limits(TMemoryBackend::DaxPmem, 1, None).unwrap();
        memory.commit_range(8, &[1, 2, 3, 4]).unwrap();

        assert!(memory.can_grow_to_pages(9));
        memory.grow_to_pages(9).unwrap();
        memory
            .commit_range(block_region::BLOCK_SIZE + 8, &[5, 6, 7, 8])
            .unwrap();

        assert_eq!(memory.read_committed(8..12).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(
            memory
                .read_committed(block_region::BLOCK_SIZE + 8..block_region::BLOCK_SIZE + 12)
                .unwrap(),
            vec![5, 6, 7, 8]
        );
    }

    #[test]
    fn dax_pmem_research_zero_capacity_grows_to_first_page() {
        let mut memory =
            TMemory::new_with_backend_limits(TMemoryBackend::DaxPmem, 0, None).unwrap();

        assert_eq!(memory.byte_len(), 0);
        assert_eq!(memory.byte_capacity(), 0);
        assert!(memory.can_grow_to_pages(1));

        memory.grow_to_pages(1).unwrap();
        memory.commit_range(8, &[1, 2, 3, 4]).unwrap();

        assert_eq!(memory.read_committed(8..12).unwrap(), vec![1, 2, 3, 4]);
    }

    fn real_pmem_test_path(file_name: &str) -> Option<std::path::PathBuf> {
        if std::env::var_os("WASMTIME_TEST_REAL_PMEM").is_none() {
            return None;
        }
        let root = std::env::var_os("WASMTIME_TEST_DAX_PMEM_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/pmem0/wasmtime-dcpmm"));
        Some(root.join(file_name))
    }

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
            let mut memory = TMemory::new(config, 1, Some(16)).unwrap();
            memory
                .commit_staged_tmemory_granule(0, &[0x5a; TMEMORY_GRANULE_SIZE])
                .unwrap();
        }

        {
            let config =
                TransactionConfig::with_dax_pmem_existing_fsdax_path(path.clone()).unwrap();
            let mut memory = TMemory::new(config, 1, Some(1)).unwrap();
            assert_eq!(memory.read_committed(0..4).unwrap(), vec![0x5a; 4]);
            assert!(memory.byte_capacity() >= 16 * WASM_PAGE_SIZE);
            assert!(memory.can_grow_to_pages(16));
            memory.grow_to_pages(16).unwrap();
        }

        {
            let config =
                TransactionConfig::with_dax_pmem_existing_fsdax_path(path.clone()).unwrap();
            let memory = TMemory::new(config, 0, None).unwrap();
            assert_eq!(memory.byte_len(), 0);
            assert!(memory.byte_capacity() >= 16 * WASM_PAGE_SIZE);
            assert!(memory.can_grow_to_pages(16));
        }

        let _ = std::fs::remove_file(path);
    }

    #[test]
    #[ignore = "requires real fsdax PMEM and WASMTIME_TEST_REAL_PMEM=1"]
    fn dax_pmem_fsdax_growth_survives_reopen() {
        let Some(path) = real_pmem_test_path("dax-pmem-grow.tmemory") else {
            eprintln!("set WASMTIME_TEST_REAL_PMEM=1 to run this test");
            return;
        };
        let _ = std::fs::remove_file(&path);

        {
            let config = TransactionConfig::with_dax_pmem_fsdax_path(path.clone()).unwrap();
            let mut memory = TMemory::new(config, 1, None).unwrap();
            memory.commit_range(8, &[1, 2, 3, 4]).unwrap();
            memory.grow_to_pages(9).unwrap();
            memory
                .commit_range(block_region::BLOCK_SIZE + 8, &[5, 6, 7, 8])
                .unwrap();
        }

        {
            let config =
                TransactionConfig::with_dax_pmem_existing_fsdax_path(path.clone()).unwrap();
            let memory = TMemory::new(config, 9, Some(9)).unwrap();
            assert_eq!(memory.read_committed(8..12).unwrap(), vec![1, 2, 3, 4]);
            assert_eq!(
                memory
                    .read_committed(block_region::BLOCK_SIZE + 8..block_region::BLOCK_SIZE + 12)
                    .unwrap(),
                vec![5, 6, 7, 8]
            );
        }

        let _ = std::fs::remove_file(path);
    }

    #[test]
    #[ignore = "requires real fsdax PMEM and WASMTIME_TEST_REAL_PMEM=1"]
    fn dax_pmem_fsdax_zero_capacity_grows_after_reopen() {
        let Some(path) = real_pmem_test_path("dax-pmem-zero-grow.tmemory") else {
            eprintln!("set WASMTIME_TEST_REAL_PMEM=1 to run this test");
            return;
        };
        let _ = std::fs::remove_file(&path);

        {
            let config = TransactionConfig::with_dax_pmem_fsdax_path(path.clone()).unwrap();
            let memory = TMemory::new(config, 0, None).unwrap();
            assert_eq!(memory.byte_len(), 0);
            assert_eq!(memory.byte_capacity(), 0);
        }

        {
            let config =
                TransactionConfig::with_dax_pmem_existing_fsdax_path(path.clone()).unwrap();
            let mut memory = TMemory::new(config, 0, None).unwrap();
            assert_eq!(memory.byte_len(), 0);
            assert_eq!(memory.byte_capacity(), 0);
            assert!(memory.can_grow_to_pages(1));
            memory.grow_to_pages(1).unwrap();
            memory.commit_range(8, &[1, 2, 3, 4]).unwrap();
        }

        {
            let config =
                TransactionConfig::with_dax_pmem_existing_fsdax_path(path.clone()).unwrap();
            let memory = TMemory::new(config, 1, Some(1)).unwrap();
            assert_eq!(memory.read_committed(8..12).unwrap(), vec![1, 2, 3, 4]);
        }

        let _ = std::fs::remove_file(path);
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

    #[test]
    fn vmemory_grow_initializes_new_granule_metadata() {
        let mut memory = TMemory::new(TransactionConfig::default(), 1, Some(2)).unwrap();

        assert_eq!(memory.granule_count(), GRANULES_PER_WASM_PAGE);

        memory.grow_to_pages(2).unwrap();

        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE * 2);
        assert_eq!(memory.granule_count(), GRANULES_PER_WASM_PAGE * 2);
        assert_eq!(
            memory.granule_info(GRANULES_PER_WASM_PAGE).unwrap(),
            TMemoryGranuleInfo::default()
        );
        assert_eq!(
            memory.granule_info(GRANULES_PER_WASM_PAGE * 2 - 1).unwrap(),
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

    #[test]
    fn granule_info_round_trips_through_tmemory_api() {
        let mut memory = TMemory::new_vmemory(1).unwrap();
        let info = TMemoryGranuleInfo {
            owner: 11,
            version: 22,
            hash: 33,
        };

        memory.set_granule_info(0, info).unwrap();

        assert_eq!(memory.granule_info(0).unwrap(), info);
    }

    #[test]
    fn transaction_memory_sidecar_resolves_by_memory_index() {
        let memory0 = MemoryIndex::from_u32(0);
        let memory2 = MemoryIndex::from_u32(2);
        let mut sidecar = TMemorySidecar::default();

        assert!(!sidecar.contains(memory0));
        assert!(!sidecar.contains(memory2));
        assert!(sidecar.get(memory0).is_none());

        sidecar
            .insert(
                memory0,
                TMemory::new_vmemory_with_limits(1, Some(2)).unwrap(),
            )
            .unwrap();

        assert!(sidecar.contains(memory0));
        assert!(!sidecar.contains(memory2));
        assert_eq!(sidecar.get(memory0).unwrap().byte_len(), WASM_PAGE_SIZE);
        assert!(sidecar.get(memory2).is_none());

        sidecar.get_mut(memory0).unwrap().grow_to_pages(2).unwrap();

        assert_eq!(sidecar.get(memory0).unwrap().byte_len(), WASM_PAGE_SIZE * 2);
    }

    #[test]
    fn read_committed_rejects_out_of_bounds_ranges() {
        let memory = TMemory::new_vmemory(1).unwrap();

        assert!(
            memory
                .read_committed(WASM_PAGE_SIZE - 1..WASM_PAGE_SIZE + 1)
                .is_err()
        );
    }

    #[test]
    fn commit_range_rejects_out_of_bounds_ranges() {
        let mut memory = TMemory::new_vmemory(1).unwrap();

        assert!(memory.commit_range(WASM_PAGE_SIZE - 1, &[1, 2]).is_err());
    }

    #[test]
    fn granule_info_rejects_out_of_bounds_granules() {
        let memory = TMemory::new_vmemory(1).unwrap();

        assert!(memory.granule_info(memory.granule_count()).is_err());
    }

    #[test]
    fn set_granule_info_rejects_out_of_bounds_granules() {
        let mut memory = TMemory::new_vmemory(1).unwrap();

        assert!(
            memory
                .set_granule_info(memory.granule_count(), TMemoryGranuleInfo::default())
                .is_err()
        );
    }

    #[test]
    fn one_wasm_page_has_wizard_granules() {
        let memory = VMemory::new(1, Some(1)).unwrap();

        assert_eq!(TMEMORY_GRANULE_SIZE, 64);
        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE);
        assert_eq!(memory.granule_len(), GRANULES_PER_WASM_PAGE);
        assert_eq!(memory.granule_capacity(), GRANULES_PER_WASM_PAGE);
        assert_eq!(VMemory::granule_index(0).unwrap(), 0);
        assert_eq!(VMemory::granule_index(63).unwrap(), 0);
        assert_eq!(VMemory::granule_index(64).unwrap(), 1);
    }

    #[test]
    fn growing_one_page_adds_granule_metadata() {
        let mut memory = VMemory::new(1, Some(2)).unwrap();

        assert_eq!(memory.granule_len(), GRANULES_PER_WASM_PAGE);
        assert_eq!(memory.granule_capacity(), GRANULES_PER_WASM_PAGE * 2);

        memory.grow_to_pages(2).unwrap();

        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE * 2);
        assert_eq!(memory.granule_len(), GRANULES_PER_WASM_PAGE * 2);
        assert_eq!(
            memory.txn_info(GRANULES_PER_WASM_PAGE * 2 - 1).unwrap(),
            TMemoryGranuleInfo::default()
        );
    }

    #[test]
    fn copy_and_writeback_one_granule() {
        let mut memory = VMemory::new(1, Some(1)).unwrap();
        memory.fill(0..TMEMORY_GRANULE_SIZE, 0x11).unwrap();
        let staged_copy = memory.copy_granule(0).unwrap();

        memory.fill(0..TMEMORY_GRANULE_SIZE, 0x22).unwrap();
        memory.write_granule(0, &staged_copy).unwrap();

        assert_eq!(
            memory.read(0..TMEMORY_GRANULE_SIZE).unwrap(),
            vec![0x11; TMEMORY_GRANULE_SIZE]
        );
    }

    #[test]
    fn writing_one_granule_preserves_neighbors() {
        let mut memory = VMemory::new(1, Some(1)).unwrap();
        memory.fill(0..TMEMORY_GRANULE_SIZE, 0x11).unwrap();
        memory
            .fill(TMEMORY_GRANULE_SIZE..TMEMORY_GRANULE_SIZE * 2, 0x33)
            .unwrap();
        let staged_copy = memory.copy_granule(0).unwrap();

        memory.fill(0..TMEMORY_GRANULE_SIZE * 2, 0x22).unwrap();
        memory.write_granule(0, &staged_copy).unwrap();

        assert_eq!(
            memory.read(0..TMEMORY_GRANULE_SIZE).unwrap(),
            vec![0x11; TMEMORY_GRANULE_SIZE]
        );
        assert_eq!(
            memory
                .read(TMEMORY_GRANULE_SIZE..TMEMORY_GRANULE_SIZE * 2)
                .unwrap(),
            vec![0x22; TMEMORY_GRANULE_SIZE]
        );
    }

    #[test]
    fn shrinking_restores_visible_byte_length() {
        let mut memory = VMemory::new(1, Some(2)).unwrap();

        memory.grow_to_pages(2).unwrap();
        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE * 2);

        memory.shrink_to_pages(1).unwrap();
        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE);
        assert!(memory.read(WASM_PAGE_SIZE..WASM_PAGE_SIZE + 1).is_err());
    }

    #[test]
    fn grow_to_pages_rejects_shrinking() {
        let mut memory = VMemory::new(2, Some(2)).unwrap();

        assert!(memory.grow_to_pages(1).is_err());
        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE * 2);
    }

    #[test]
    fn read_and_fill_reject_invalid_ranges() {
        let mut memory = VMemory::new(1, Some(1)).unwrap();

        assert!(memory.read(10..0).is_err());
        assert!(memory.fill(10..0, 0x11).is_err());
    }

    #[test]
    fn shrink_then_grow_does_not_expose_stale_data_or_metadata() {
        let mut memory = VMemory::new(1, Some(2)).unwrap();

        memory.grow_to_pages(2).unwrap();
        memory
            .fill(WASM_PAGE_SIZE..WASM_PAGE_SIZE + 8, 0x44)
            .unwrap();

        memory
            .set_txn_info(
                GRANULES_PER_WASM_PAGE,
                TMemoryGranuleInfo {
                    owner: 0xff,
                    version: 0xff,
                    hash: 0xff,
                },
            )
            .unwrap();
        assert_ne!(
            memory.txn_info(GRANULES_PER_WASM_PAGE).unwrap(),
            TMemoryGranuleInfo::default()
        );

        memory.shrink_to_pages(1).unwrap();
        memory.grow_to_pages(2).unwrap();

        assert_eq!(
            memory.read(WASM_PAGE_SIZE..WASM_PAGE_SIZE + 8).unwrap(),
            [0; 8]
        );
        assert_eq!(
            memory.txn_info(GRANULES_PER_WASM_PAGE).unwrap(),
            TMemoryGranuleInfo::default()
        );
    }
}
