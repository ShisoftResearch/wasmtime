//! Storage-only prototype for Wizard-style transactional memories.
//!
//! SHISOFT-TWASM-MOCK: storage prototype scaffold. This module is intentionally
//! not wired into instance allocation yet. It establishes the mmap-backed
//! storage boundary and granule helpers that transactional lowering will need
//! once parser/runtime support exists.

#![allow(dead_code)]

use crate::prelude::*;
use crate::runtime::transaction::{TMemoryBackend, TransactionConfig};
use crate::runtime::vm::{HostAlignedByteCount, Mmap, mmap::AlignedLength};
use wasmtime_environ::MemoryIndex;

pub(crate) const WASM_PAGE_SIZE: usize = 64 * 1024;

/// Wizard-compatible transactional memory granule shift.
pub const TMEMORY_GRANULE_SHIFT: usize = 8;

/// Wizard-compatible transactional memory granule size in bytes.
pub const TMEMORY_GRANULE_SIZE: usize = 1 << TMEMORY_GRANULE_SHIFT;

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
    fn grow_to_pages(&mut self, new_pages: u64) -> Result<()>;
    fn granule_info(&self, granule: usize) -> Result<TMemoryGranuleInfo>;
    fn set_granule_info(&mut self, granule: usize, info: TMemoryGranuleInfo) -> Result<()>;
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
        Self::new_with_backend_limits(config.tmemory_backend(), min_pages, max_pages)
    }

    pub(crate) fn new_with_backend(backend: TMemoryBackend, min_pages: u64) -> Result<Self> {
        Self::new_with_backend_limits(backend, min_pages, Some(min_pages))
    }

    pub(crate) fn new_with_backend_limits(
        backend: TMemoryBackend,
        min_pages: u64,
        max_pages: Option<u64>,
    ) -> Result<Self> {
        Self::new_for_backend(backend, min_pages, max_pages)
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

    pub(crate) fn granule_info(&self, granule: usize) -> Result<TMemoryGranuleInfo> {
        self.storage.granule_info(granule)
    }

    pub(crate) fn set_granule_info(
        &mut self,
        granule: usize,
        info: TMemoryGranuleInfo,
    ) -> Result<()> {
        self.storage.set_granule_info(granule, info)
    }

    fn new_for_backend(
        backend: TMemoryBackend,
        min_pages: u64,
        max_pages: Option<u64>,
    ) -> Result<Self> {
        let storage: Box<dyn TMemoryBackendStorage> = match backend {
            TMemoryBackend::VMemory => Box::new(VMemory::new(min_pages, max_pages)?),
            TMemoryBackend::FileBackedMemory | TMemoryBackend::NVMemory => {
                return Err(unsupported_backend_error(backend));
            }
        };
        Ok(Self { storage })
    }
}

/// Volatile anonymous-mmap transactional memory storage.
#[derive(Debug)]
pub(crate) struct VMemory {
    data: Mmap<AlignedLength>,
    granules: Mmap<AlignedLength>,
    byte_len: usize,
    byte_capacity: usize,
    granule_capacity: usize,
}

impl VMemory {
    pub(crate) fn new(min_pages: u64, max_pages: Option<u64>) -> Result<Self> {
        let byte_len = pages_to_bytes(min_pages)?;
        let byte_capacity = pages_to_bytes(max_pages.unwrap_or(min_pages))?;
        ensure!(byte_len <= byte_capacity, "tmemory minimum exceeds maximum");

        let granule_capacity = granules_for_bytes(byte_capacity);
        let granule_len = granules_for_bytes(byte_len);

        let data_mapping_size = HostAlignedByteCount::new_rounded_up(byte_capacity)?;
        let data_accessible = HostAlignedByteCount::new_rounded_up(byte_len)?;
        let data = Mmap::accessible_reserved(data_accessible, data_mapping_size)?;

        let granule_mapping_bytes = granule_capacity
            .checked_mul(size_of::<TMemoryGranuleInfo>())
            .context("tmemory granule metadata size overflow")?;
        let granule_accessible_bytes = granule_len
            .checked_mul(size_of::<TMemoryGranuleInfo>())
            .context("tmemory granule metadata size overflow")?;
        let granules = Mmap::accessible_reserved(
            HostAlignedByteCount::new_rounded_up(granule_accessible_bytes)?,
            HostAlignedByteCount::new_rounded_up(granule_mapping_bytes)?,
        )?;

        Ok(Self {
            data,
            granules,
            byte_len,
            byte_capacity,
            granule_capacity,
        })
    }

    pub(crate) fn byte_len(&self) -> usize {
        self.byte_len
    }

    pub(crate) fn granule_len(&self) -> usize {
        granules_for_bytes(self.byte_len)
    }

    pub(crate) fn granule_capacity(&self) -> usize {
        self.granule_capacity
    }

    pub(crate) fn granule_index(addr: u64) -> Result<usize> {
        let index = addr >> TMEMORY_GRANULE_SHIFT;
        usize::try_from(index).context("tmemory address does not fit host usize")
    }

    pub(crate) fn grow_to_pages(&mut self, new_pages: u64) -> Result<()> {
        let new_byte_len = pages_to_bytes(new_pages)?;
        ensure!(
            new_byte_len <= self.byte_capacity,
            "tmemory growth exceeds reserved capacity"
        );
        if new_byte_len < self.byte_len {
            bail!("tmemory grow cannot shrink");
        }
        if new_byte_len == self.byte_len {
            return Ok(());
        }

        let old_accessible = HostAlignedByteCount::new_rounded_up(self.byte_len)?;
        let new_accessible = HostAlignedByteCount::new_rounded_up(new_byte_len)?;
        let data_delta = new_accessible
            .checked_sub(old_accessible)
            .context("tmemory accessible data underflow")?;
        // SAFETY: this is newly live memory that has not been handed out.
        unsafe {
            self.data.make_accessible(old_accessible, data_delta)?;
        }

        let old_granule_bytes = self
            .granule_len()
            .checked_mul(size_of::<TMemoryGranuleInfo>())
            .context("tmemory granule metadata size overflow")?;
        let new_granule_bytes = granules_for_bytes(new_byte_len)
            .checked_mul(size_of::<TMemoryGranuleInfo>())
            .context("tmemory granule metadata size overflow")?;
        let old_granule_accessible = HostAlignedByteCount::new_rounded_up(old_granule_bytes)?;
        let new_granule_accessible = HostAlignedByteCount::new_rounded_up(new_granule_bytes)?;
        let granule_delta = new_granule_accessible
            .checked_sub(old_granule_accessible)
            .context("tmemory accessible granule metadata underflow")?;
        // SAFETY: this is newly live metadata that has not been handed out.
        unsafe {
            self.granules
                .make_accessible(old_granule_accessible, granule_delta)?;
        }

        self.byte_len = new_byte_len;
        Ok(())
    }

    pub(crate) fn shrink_to_pages(&mut self, pages: u64) -> Result<()> {
        let new_byte_len = pages_to_bytes(pages)?;
        ensure!(new_byte_len <= self.byte_len, "tmemory shrink cannot grow");
        let old_byte_len = self.byte_len;
        let old_granule_len = self.granule_len();
        let new_granule_len = granules_for_bytes(new_byte_len);

        if new_byte_len < old_byte_len {
            // SAFETY: the truncated byte range is currently accessible and we
            // have exclusive access to the storage.
            unsafe {
                self.data.slice_mut(new_byte_len..old_byte_len).fill(0);
            }
        }

        if new_granule_len < old_granule_len {
            let start = new_granule_len
                .checked_mul(size_of::<TMemoryGranuleInfo>())
                .context("tmemory granule metadata offset overflow")?;
            let end = old_granule_len
                .checked_mul(size_of::<TMemoryGranuleInfo>())
                .context("tmemory granule metadata offset overflow")?;
            // SAFETY: the truncated metadata range is currently accessible and
            // we have exclusive access to the storage.
            unsafe {
                self.granules.slice_mut(start..end).fill(0);
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
        let start = granule
            .checked_mul(size_of::<TMemoryGranuleInfo>())
            .context("tmemory granule metadata offset overflow")?;
        let end = start + size_of::<TMemoryGranuleInfo>();
        // SAFETY: bounds are checked above and the metadata range is live.
        let bytes = unsafe { self.granules.slice(start..end) };
        Ok(read_granule_info(bytes))
    }

    pub(crate) fn copy_granule(&self, granule: usize) -> Result<Vec<u8>> {
        ensure!(
            granule < self.granule_len(),
            "tmemory granule out of bounds"
        );
        let range = self.granule_range(granule)?;
        // SAFETY: bounds are checked by `granule_range`.
        Ok(unsafe { self.data.slice(range) }.to_vec())
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
        // SAFETY: bounds are checked by `granule_range` and we have `&mut self`.
        unsafe {
            self.data.slice_mut(range).copy_from_slice(bytes);
        }
        Ok(())
    }

    pub(crate) fn set_txn_info(&mut self, granule: usize, info: TMemoryGranuleInfo) -> Result<()> {
        ensure!(
            granule < self.granule_len(),
            "tmemory granule out of bounds"
        );
        let start = granule
            .checked_mul(size_of::<TMemoryGranuleInfo>())
            .context("tmemory granule metadata offset overflow")?;
        let end = start + size_of::<TMemoryGranuleInfo>();
        // SAFETY: bounds are checked above and the metadata range is live.
        unsafe {
            write_granule_info(self.granules.slice_mut(start..end), info);
        }
        Ok(())
    }

    pub(crate) fn fill(&mut self, range: core::ops::Range<usize>, byte: u8) -> Result<()> {
        ensure!(range.start <= range.end, "tmemory write invalid range");
        ensure!(range.end <= self.byte_len, "tmemory write out of bounds");
        // SAFETY: bounds are checked above and we have `&mut self`.
        unsafe {
            self.data.slice_mut(range).fill(byte);
        }
        Ok(())
    }

    pub(crate) fn read(&self, range: core::ops::Range<usize>) -> Result<Vec<u8>> {
        ensure!(range.start <= range.end, "tmemory read invalid range");
        ensure!(range.end <= self.byte_len, "tmemory read out of bounds");
        // SAFETY: bounds are checked above.
        Ok(unsafe { self.data.slice(range) }.to_vec())
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
        ensure!(end <= self.byte_len, "tmemory write out of bounds");
        // SAFETY: bounds are checked above and we have `&mut self`.
        unsafe {
            self.data.slice_mut(addr..end).copy_from_slice(bytes);
        }
        Ok(())
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

fn read_granule_info(bytes: &[u8]) -> TMemoryGranuleInfo {
    debug_assert_eq!(bytes.len(), size_of::<TMemoryGranuleInfo>());
    let owner = u64::from_ne_bytes(bytes[0..8].try_into().unwrap());
    let version = u64::from_ne_bytes(bytes[8..16].try_into().unwrap());
    let hash = u64::from_ne_bytes(bytes[16..24].try_into().unwrap());
    TMemoryGranuleInfo {
        owner,
        version,
        hash,
    }
}

fn write_granule_info(bytes: &mut [u8], info: TMemoryGranuleInfo) {
    debug_assert_eq!(bytes.len(), size_of::<TMemoryGranuleInfo>());
    bytes[0..8].copy_from_slice(&info.owner.to_ne_bytes());
    bytes[8..16].copy_from_slice(&info.version.to_ne_bytes());
    bytes[16..24].copy_from_slice(&info.hash.to_ne_bytes());
}

fn unsupported_backend_error(backend: TMemoryBackend) -> Error {
    Error::msg(format!("tmemory backend is not implemented: {backend:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmemory_uses_configured_vmemory_backend() {
        let memory = TMemory::new(TransactionConfig::default(), 1, Some(1)).unwrap();

        assert_eq!(memory.backend(), TMemoryBackend::VMemory);
        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE);
        assert_eq!(memory.granule_len(), 256);
    }

    #[test]
    fn vmemory_commit_and_read_committed_bytes() {
        let mut memory = TMemory::new_vmemory(2).unwrap();

        assert_eq!(memory.backend_kind(), TMemoryBackend::VMemory);
        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE * 2);
        assert_eq!(memory.granule_count(), 512);

        memory.commit_range(8, &[0xaa, 0xbb, 0xcc, 0xdd]).unwrap();

        assert_eq!(
            memory.read_committed(6..14).unwrap(),
            vec![0x00, 0x00, 0xaa, 0xbb, 0xcc, 0xdd, 0x00, 0x00]
        );
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
    fn vmemory_grow_initializes_new_granule_metadata() {
        let mut memory = TMemory::new(TransactionConfig::default(), 1, Some(2)).unwrap();

        assert_eq!(memory.granule_count(), 256);

        memory.grow_to_pages(2).unwrap();

        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE * 2);
        assert_eq!(memory.granule_count(), 512);
        assert_eq!(
            memory.granule_info(256).unwrap(),
            TMemoryGranuleInfo::default()
        );
        assert_eq!(
            memory.granule_info(511).unwrap(),
            TMemoryGranuleInfo::default()
        );
    }

    #[test]
    fn file_backed_and_nv_backends_are_explicitly_unsupported() {
        for backend in [TMemoryBackend::FileBackedMemory, TMemoryBackend::NVMemory] {
            let error = TMemory::new_with_backend(backend, 1).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("tmemory backend is not implemented")
            );
        }
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

        assert_eq!(TMEMORY_GRANULE_SIZE, 256);
        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE);
        assert_eq!(memory.granule_len(), 256);
        assert_eq!(memory.granule_capacity(), 256);
        assert_eq!(VMemory::granule_index(0).unwrap(), 0);
        assert_eq!(VMemory::granule_index(255).unwrap(), 0);
        assert_eq!(VMemory::granule_index(256).unwrap(), 1);
    }

    #[test]
    fn growing_one_page_adds_granule_metadata() {
        let mut memory = VMemory::new(1, Some(2)).unwrap();

        assert_eq!(memory.granule_len(), 256);
        assert_eq!(memory.granule_capacity(), 512);

        memory.grow_to_pages(2).unwrap();

        assert_eq!(memory.byte_len(), WASM_PAGE_SIZE * 2);
        assert_eq!(memory.granule_len(), 512);
        assert_eq!(memory.txn_info(511).unwrap(), TMemoryGranuleInfo::default());
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

        let second_page_metadata = 256 * size_of::<TMemoryGranuleInfo>();
        let second_page_metadata_end = second_page_metadata + size_of::<TMemoryGranuleInfo>();
        // SAFETY: the second page's metadata is live after growing to two
        // pages, and tests have exclusive access to the storage.
        unsafe {
            memory
                .granules
                .slice_mut(second_page_metadata..second_page_metadata_end)
                .fill(0xff);
        }
        assert_ne!(memory.txn_info(256).unwrap(), TMemoryGranuleInfo::default());

        memory.shrink_to_pages(1).unwrap();
        memory.grow_to_pages(2).unwrap();

        assert_eq!(
            memory.read(WASM_PAGE_SIZE..WASM_PAGE_SIZE + 8).unwrap(),
            [0; 8]
        );
        assert_eq!(memory.txn_info(256).unwrap(), TMemoryGranuleInfo::default());
    }
}
