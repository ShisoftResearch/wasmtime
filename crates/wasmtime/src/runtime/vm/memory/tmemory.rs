//! Wizard-style storage for transactional memories.
//!
//! This module backs per-instance `tmemory` sidecars with volatile and research
//! NVMemory block/chunk backends, plus filesystem-backed transactional
//! `FileBackedMemory`.

#![allow(dead_code)]

use crate::prelude::*;
use crate::runtime::transaction::{
    TMEMORY_GRANULE_SHIFT, TMEMORY_GRANULE_SIZE, TMemoryBackend, TMemoryFileBacking,
    TMemoryPersistenceMode, TransactionConfig,
};
use wasmtime_environ::MemoryIndex;

pub(crate) mod block_region;
mod linear_region;

use self::linear_region::TMemoryRegion;

pub(crate) const WASM_PAGE_SIZE: usize = 64 * 1024;
const DEFAULT_MAX_WASM_PAGES: u64 = 1 << 16;

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
    fn can_grow_to_pages(&self, new_pages: u64) -> bool;
    fn grow_to_pages(&mut self, new_pages: u64) -> Result<()>;
    fn granule_info(&self, granule: usize) -> Result<TMemoryGranuleInfo>;
    fn set_granule_info(&mut self, granule: usize, info: TMemoryGranuleInfo) -> Result<()>;
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
            TMemoryPersistenceMode::ResearchPretendPmem,
            None,
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
        min_pages: u64,
        max_pages: Option<u64>,
    ) -> Result<Self> {
        let storage: Box<dyn TMemoryBackendStorage> = match backend {
            TMemoryBackend::VMemory => Box::new(VMemory::new(min_pages, max_pages)?),
            TMemoryBackend::NVMemory => Box::new(NVMemory::new_with_persistence_mode(
                min_pages,
                max_pages,
                persistence_mode,
            )?),
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

/// Research NVMemory transactional storage backed by a PMEM-capable block region.
#[derive(Debug)]
pub(crate) struct NVMemory {
    region: TMemoryRegion,
    persistence_mode: block_region::PersistenceMode,
    granules: Vec<TMemoryGranuleInfo>,
    byte_len: usize,
    byte_capacity: usize,
    max_pages: u64,
}

impl NVMemory {
    pub(crate) fn new(min_pages: u64, max_pages: Option<u64>) -> Result<Self> {
        Self::new_with_persistence_mode(
            min_pages,
            max_pages,
            TMemoryPersistenceMode::ResearchPretendPmem,
        )
    }

    pub(crate) fn new_with_persistence_mode(
        min_pages: u64,
        max_pages: Option<u64>,
        persistence_mode: TMemoryPersistenceMode,
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
        let persistence_mode = block_region_persistence_mode(persistence_mode);

        Ok(Self {
            region: TMemoryRegion::new_nvmemory(byte_capacity, persistence_mode)?,
            persistence_mode,
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

        let mut region = TMemoryRegion::new_nvmemory(new_byte_capacity, self.persistence_mode)?;
        if self.byte_len > 0 {
            let old = self.region.read(0, self.byte_len)?;
            region.write(0, &old)?;
            region.flush(0, self.byte_len)?;
            region.fence()?;
        }

        let new_granule_capacity = granules_for_bytes(new_byte_capacity);
        self.granules
            .resize(new_granule_capacity, TMemoryGranuleInfo::default());

        self.region = region;
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

fn block_region_persistence_mode(
    persistence_mode: TMemoryPersistenceMode,
) -> block_region::PersistenceMode {
    match persistence_mode {
        TMemoryPersistenceMode::ResearchPretendPmem => {
            block_region::PersistenceMode::ResearchPretendPmem
        }
        TMemoryPersistenceMode::RequireHardwarePmem => {
            block_region::PersistenceMode::RequireHardwarePmem
        }
    }
}

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
        let byte_capacity = match requested_max_pages {
            Some(max_pages) => pages_to_bytes(max_pages)?,
            None => byte_len,
        };
        let granule_capacity = granules_for_bytes(byte_capacity);

        Ok(Self {
            region: TMemoryRegion::new_file_backed(
                byte_capacity,
                file_backed_region_mode(&file_backing),
            )?,
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

        let old = if self.byte_len > 0 {
            self.region.read(0, self.byte_len)?
        } else {
            Vec::new()
        };
        let mut region = TMemoryRegion::new_file_backed(
            new_byte_capacity,
            file_backed_region_mode(&self.file_backing),
        )?;
        if !old.is_empty() {
            region.write(0, &old)?;
            region.flush(0, old.len())?;
            region.fence()?;
        }

        let new_granule_capacity = granules_for_bytes(new_byte_capacity);
        self.granules
            .resize(new_granule_capacity, TMemoryGranuleInfo::default());

        self.region = region;
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

fn file_backed_region_mode(
    file_backing: &TMemoryFileBacking,
) -> block_region::FileBackedRegionMode {
    match file_backing {
        TMemoryFileBacking::Temp => block_region::FileBackedRegionMode::Temp,
        TMemoryFileBacking::Path(path) => block_region::FileBackedRegionMode::Path(path.clone()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn tmemory_uses_configured_vmemory_backend() {
        let memory = TMemory::new(TransactionConfig::default(), 1, Some(1)).unwrap();

        assert_eq!(memory.backend(), TMemoryBackend::VMemory);
        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE);
        assert_eq!(memory.granule_len(), GRANULES_PER_WASM_PAGE);
    }

    #[test]
    fn tmemory_can_construct_nvmemory_in_research_mode() {
        let memory =
            TMemory::new_with_backend_limits(TMemoryBackend::NVMemory, 1, Some(1)).unwrap();

        assert_eq!(memory.backend(), TMemoryBackend::NVMemory);
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
    fn nvmemory_commit_range_writes_and_versions_granules() {
        let mut memory =
            TMemory::new_with_backend_limits(TMemoryBackend::NVMemory, 1, Some(1)).unwrap();

        memory.commit_range(4, &[10, 11, 12, 13]).unwrap();

        assert_eq!(memory.read_committed(4..8).unwrap(), vec![10, 11, 12, 13]);
        assert_eq!(memory.granule_version(0).unwrap(), 1);
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
    fn nvmemory_direct_constructor_with_limits_reserves_capacity_and_grows() {
        let mut memory =
            TMemory::new_with_backend_limits(TMemoryBackend::NVMemory, 1, Some(2)).unwrap();

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
    fn nvmemory_grow_beyond_capacity_preserves_bytes_and_granule_metadata() {
        let mut memory =
            TMemory::new_with_backend_limits(TMemoryBackend::NVMemory, 1, None).unwrap();
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

    #[test]
    fn nvmemory_hardware_pmem_mode_is_reachable_from_transaction_config() {
        let config = TransactionConfig::with_nvmemory_persistence_mode(
            TMemoryPersistenceMode::RequireHardwarePmem,
        )
        .unwrap();
        let result = TMemory::new(config, 1, Some(1));

        #[cfg(target_arch = "x86_64")]
        {
            if block_region::PersistEngine::hardware_flush_available() {
                let memory = result.unwrap();
                assert_eq!(memory.backend(), TMemoryBackend::NVMemory);
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

    #[test]
    #[ignore = "requires real PMEM hardware and WASMTIME_TEST_REAL_PMEM=1"]
    fn nvmemory_requires_real_pmem_for_restart_persistence() {
        if std::env::var_os("WASMTIME_TEST_REAL_PMEM").is_none() {
            eprintln!("set WASMTIME_TEST_REAL_PMEM=1 on a PMEM machine to run this test");
            return;
        }

        panic!("real PMEM restart recovery test is not implemented in this backend wave");
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
