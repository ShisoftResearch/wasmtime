//! Wizard-style block/chunk region layout for transactional storage.

#![allow(dead_code)]

use crate::prelude::*;
use core::{mem::size_of, ops::Range};

pub(crate) const BLOCK_SIZE: usize = 512 * 1024;
pub(crate) const IMMIX_LINE_SIZE: usize = 256;
pub(crate) const PMEM_CACHE_LINE_SIZE: usize = 64;

pub(crate) const PW_REGION_HEADER_SIZE: usize = size_of::<PWRegionHeader>();
pub(crate) const META_DATA_DESC_SIZE: usize = size_of::<MetaDataDesc>();
pub(crate) const BLOCK_ENTRY_SIZE: usize = size_of::<BlockEntry>();
pub(crate) const CHUNK_HEADER_SIZE: usize = size_of::<ChunkHeader>();
pub(crate) const LINE_MARK_SIZE: usize = size_of::<LineMark>();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PersistenceMode {
    ResearchPretendPmem,
    RequireHardwarePmem,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PersistFlushKind {
    NoopResearch,
    X86Clwb,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PersistEngine {
    mode: PersistenceMode,
    flush_kind: PersistFlushKind,
}

impl PersistEngine {
    pub(crate) fn for_mode(mode: PersistenceMode) -> Result<Self> {
        let flush_kind = match mode {
            PersistenceMode::ResearchPretendPmem => PersistFlushKind::NoopResearch,
            PersistenceMode::RequireHardwarePmem => hardware_flush_kind()?,
        };
        Ok(Self { mode, flush_kind })
    }

    pub(crate) fn mode(&self) -> PersistenceMode {
        self.mode
    }

    pub(crate) fn flush(&self, ptr: core::ptr::NonNull<u8>, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        match self.flush_kind {
            PersistFlushKind::NoopResearch => Ok(()),
            PersistFlushKind::X86Clwb => unsafe { flush_clwb_range(ptr, len) },
        }
    }

    pub(crate) fn fence(&self) -> Result<()> {
        match self.flush_kind {
            PersistFlushKind::NoopResearch => Ok(()),
            PersistFlushKind::X86Clwb => unsafe { sfence() },
        }
    }

    #[cfg(target_arch = "x86_64")]
    pub(crate) fn hardware_flush_available() -> bool {
        let leaf = core::arch::x86_64::__cpuid_count(7, 0);
        leaf.ebx & (1 << 24) != 0
    }

    #[cfg(not(target_arch = "x86_64"))]
    pub(crate) fn hardware_flush_available() -> bool {
        false
    }
}

fn hardware_flush_kind() -> Result<PersistFlushKind> {
    #[cfg(target_arch = "x86_64")]
    {
        if PersistEngine::hardware_flush_available() {
            return Ok(PersistFlushKind::X86Clwb);
        }
        bail!("NVMemory requires CLWB support for hardware PMEM mode");
    }

    #[cfg(not(target_arch = "x86_64"))]
    bail!("NVMemory is unsupported on this target until a PMEM flush implementation exists")
}

#[cfg(target_arch = "x86_64")]
unsafe fn flush_clwb_range(ptr: core::ptr::NonNull<u8>, len: usize) -> Result<()> {
    let start = ptr.as_ptr() as usize;
    let end = start
        .checked_add(len)
        .context("pmem flush range overflow")?;
    let mut cursor = start & !(PMEM_CACHE_LINE_SIZE - 1);
    while cursor < end {
        unsafe {
            core::arch::asm!(
                "clwb [{}]",
                in(reg) cursor as *const u8,
                options(nostack, preserves_flags)
            );
        }
        cursor = cursor
            .checked_add(PMEM_CACHE_LINE_SIZE)
            .context("pmem flush cursor overflow")?;
    }
    Ok(())
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn flush_clwb_range(_ptr: core::ptr::NonNull<u8>, _len: usize) -> Result<()> {
    bail!("NVMemory CLWB flush is unsupported on this target")
}

#[cfg(target_arch = "x86_64")]
unsafe fn sfence() -> Result<()> {
    unsafe {
        core::arch::asm!("sfence", options(nostack, preserves_flags));
    }
    Ok(())
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn sfence() -> Result<()> {
    bail!("NVMemory SFENCE is unsupported on this target")
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PWRegionHeader {
    pub block_table: u64,
    pub sentinels: u64,
    pub num_blocks: u64,
    pub num_bytes: u64,
    pub num_descs: u64,
    pub metadata: u64,
    pub metadata_descs: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MetaDataDesc {
    pub kind: u64,
    pub fixed_bytes: u32,
    pub unit_size: u32,
    pub bytes_per_unit: u32,
    padding: u32,
    pub offset: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct BlockEntry {
    pub list_num: i16,
    pub used: u8,
    padding: [u8; 5],
    pub prev: i64,
    pub next: i64,
    pub list_prev: i64,
    pub list_next: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ChunkHeader {
    pub offset: u64,
    pub limit: u64,
    pub chunk_limit: u64,
    pub linemarks: u64,
}

#[repr(i16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ListKind {
    Metadata = -1,
    None = -2,
    Used = -3,
    SmallFree = 0,
    LargeFree = 1,
}

pub(crate) struct BlockLists;

impl BlockLists {
    pub(crate) const MAX_LISTS: usize = 8;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct LineMark {
    pub mark: u8,
}

pub(crate) const IMMIX_LINE_MARK_META_KIND: u64 = 0x0000_0001_0000_0000;
pub(crate) const IMMIX_LINE_MARK_RESET_VALUE: u8 = 0;
pub(crate) const LINE_FREE: u8 = 0;
pub(crate) const LINE_MARKED: u8 = 1;

pub(crate) trait BlockRegionBackend {
    fn block_size(&self) -> usize;
    fn num_blocks(&self) -> usize;
    fn bytes_len(&self) -> usize;
    fn alloc_chunk(&mut self, block_count: usize) -> Result<RegionChunk>;
    fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>>;
    fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()>;
    fn flush(&self, offset: usize, len: usize) -> Result<()>;
    fn fence(&self) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RegionChunk {
    start_block: usize,
    block_count: usize,
}

impl RegionChunk {
    pub(crate) fn start_block(&self) -> usize {
        self.start_block
    }

    pub(crate) fn block_count(&self) -> usize {
        self.block_count
    }

    pub(crate) fn byte_range(&self) -> Range<usize> {
        let start = self.start_block * BLOCK_SIZE;
        start..start + self.block_count * BLOCK_SIZE
    }

    pub(crate) fn byte_len(&self) -> usize {
        self.block_count * BLOCK_SIZE
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ChunkList {
    chunks: Vec<RegionChunk>,
    logical_len: usize,
}

impl ChunkList {
    pub(crate) fn from_chunks(chunks: Vec<RegionChunk>) -> Result<Self> {
        let mut logical_len = 0usize;
        for chunk in &chunks {
            ensure!(
                chunk.block_count > 0,
                "transactional chunk list cannot contain empty chunks"
            );
            logical_len = logical_len
                .checked_add(chunk.byte_len())
                .context("transactional chunk list byte length overflow")?;
        }
        Ok(Self {
            chunks,
            logical_len,
        })
    }

    pub(crate) fn chunks(&self) -> &[RegionChunk] {
        &self.chunks
    }

    pub(crate) fn logical_len(&self) -> usize {
        self.logical_len
    }
}

#[derive(Debug)]
pub(crate) struct VMemoryBlockRegion {
    data: Vec<u8>,
    block_entries: Vec<BlockEntry>,
    line_marks: Vec<LineMark>,
}

impl VMemoryBlockRegion {
    pub(crate) fn new(num_blocks: usize) -> Result<Self> {
        let bytes_len = num_blocks
            .checked_mul(BLOCK_SIZE)
            .context("transactional block region size overflow")?;
        let line_count = bytes_len / IMMIX_LINE_SIZE;
        Ok(Self {
            data: vec![0; bytes_len],
            block_entries: vec![BlockEntry::default(); num_blocks],
            line_marks: vec![
                LineMark {
                    mark: IMMIX_LINE_MARK_RESET_VALUE,
                };
                line_count
            ],
        })
    }

    pub(crate) fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    pub(crate) fn num_blocks(&self) -> usize {
        self.block_entries.len()
    }

    pub(crate) fn bytes_len(&self) -> usize {
        self.data.len()
    }

    pub(crate) fn lines_per_block(&self) -> usize {
        BLOCK_SIZE / IMMIX_LINE_SIZE
    }

    pub(crate) fn line_count(&self) -> usize {
        self.line_marks.len()
    }

    pub(crate) fn alloc_chunk(&mut self, block_count: usize) -> Result<RegionChunk> {
        ensure!(block_count > 0, "transactional chunk must contain a block");
        ensure!(
            block_count <= self.num_blocks(),
            "transactional chunk exceeds region size"
        );

        let last_start = self.num_blocks() - block_count;
        for start in 0..=last_start {
            let end = start + block_count;
            if self.block_entries[start..end]
                .iter()
                .all(|entry| entry.used == 0)
            {
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

        bail!("transactional block region is out of contiguous chunks")
    }

    pub(crate) fn line_mark(&self, line_index: usize) -> Result<u8> {
        self.line_marks
            .get(line_index)
            .map(|mark| mark.mark)
            .context("transactional line mark index out of bounds")
    }

    pub(crate) fn mark_line(&mut self, line_index: usize) -> Result<()> {
        let line = self
            .line_marks
            .get_mut(line_index)
            .context("transactional line mark index out of bounds")?;
        line.mark = LINE_MARKED;
        Ok(())
    }

    pub(crate) fn reset_line_marks(&mut self, chunk: RegionChunk) -> Result<()> {
        let start = chunk
            .start_block
            .checked_mul(self.lines_per_block())
            .context("transactional line mark range overflow")?;
        let len = chunk
            .block_count
            .checked_mul(self.lines_per_block())
            .context("transactional line mark range overflow")?;
        let end = start
            .checked_add(len)
            .context("transactional line mark range overflow")?;
        ensure!(
            end <= self.line_marks.len(),
            "transactional line mark range out of bounds"
        );
        for line in &mut self.line_marks[start..end] {
            line.mark = IMMIX_LINE_MARK_RESET_VALUE;
        }
        Ok(())
    }

    pub(crate) fn flush(&self, offset: usize, len: usize) -> Result<()> {
        let end = offset
            .checked_add(len)
            .context("transactional block flush range overflow")?;
        ensure!(
            end <= self.data.len(),
            "transactional block flush range out of bounds"
        );
        Ok(())
    }

    pub(crate) fn fence(&self) -> Result<()> {
        Ok(())
    }

    pub(crate) fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        let end = offset
            .checked_add(len)
            .context("transactional block read range overflow")?;
        ensure!(
            end <= self.data.len(),
            "transactional block read range out of bounds"
        );
        Ok(self.data[offset..end].to_vec())
    }

    pub(crate) fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        let end = offset
            .checked_add(bytes.len())
            .context("transactional block write range overflow")?;
        ensure!(
            end <= self.data.len(),
            "transactional block write range out of bounds"
        );
        self.data[offset..end].copy_from_slice(bytes);
        Ok(())
    }
}

impl BlockRegionBackend for VMemoryBlockRegion {
    fn block_size(&self) -> usize {
        self.block_size()
    }

    fn num_blocks(&self) -> usize {
        self.num_blocks()
    }

    fn bytes_len(&self) -> usize {
        self.bytes_len()
    }

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

#[derive(Debug)]
pub(crate) struct NVMemoryBlockRegion {
    data: Vec<u8>,
    block_entries: Vec<BlockEntry>,
    line_marks: Vec<LineMark>,
    persist: PersistEngine,
}

impl NVMemoryBlockRegion {
    pub(crate) fn new(num_blocks: usize, mode: PersistenceMode) -> Result<Self> {
        let bytes_len = num_blocks
            .checked_mul(BLOCK_SIZE)
            .context("transactional NVMemory block region size overflow")?;
        let line_count = bytes_len / IMMIX_LINE_SIZE;
        Ok(Self {
            data: vec![0; bytes_len],
            block_entries: vec![BlockEntry::default(); num_blocks],
            line_marks: vec![
                LineMark {
                    mark: IMMIX_LINE_MARK_RESET_VALUE,
                };
                line_count
            ],
            persist: PersistEngine::for_mode(mode)?,
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(num_blocks: usize, mode: PersistenceMode) -> Result<Self> {
        Self::new(num_blocks, mode)
    }

    pub(crate) fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    pub(crate) fn num_blocks(&self) -> usize {
        self.block_entries.len()
    }

    pub(crate) fn bytes_len(&self) -> usize {
        self.data.len()
    }

    pub(crate) fn line_count(&self) -> usize {
        self.line_marks.len()
    }

    pub(crate) fn alloc_chunk(&mut self, block_count: usize) -> Result<RegionChunk> {
        ensure!(
            block_count > 0,
            "transactional NVMemory chunk must contain a block"
        );
        ensure!(
            block_count <= self.num_blocks(),
            "transactional NVMemory chunk exceeds region size"
        );

        let last_start = self.num_blocks() - block_count;
        for start in 0..=last_start {
            let end = start + block_count;
            if self.block_entries[start..end]
                .iter()
                .all(|entry| entry.used == 0)
            {
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

        bail!("transactional NVMemory block region is out of contiguous chunks")
    }

    pub(crate) fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        let end = offset
            .checked_add(len)
            .context("transactional NVMemory block read range overflow")?;
        ensure!(
            end <= self.data.len(),
            "transactional NVMemory block read range out of bounds"
        );
        Ok(self.data[offset..end].to_vec())
    }

    pub(crate) fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        let end = offset
            .checked_add(bytes.len())
            .context("transactional NVMemory block write range overflow")?;
        ensure!(
            end <= self.data.len(),
            "transactional NVMemory block write range out of bounds"
        );
        self.data[offset..end].copy_from_slice(bytes);
        Ok(())
    }

    pub(crate) fn flush(&self, offset: usize, len: usize) -> Result<()> {
        let end = offset
            .checked_add(len)
            .context("transactional NVMemory block flush range overflow")?;
        ensure!(
            end <= self.data.len(),
            "transactional NVMemory block flush range out of bounds"
        );
        if len == 0 {
            return Ok(());
        }
        let ptr = core::ptr::NonNull::from(&self.data[offset]).cast::<u8>();
        self.persist.flush(ptr, len)
    }

    pub(crate) fn fence(&self) -> Result<()> {
        self.persist.fence()
    }
}

impl BlockRegionBackend for NVMemoryBlockRegion {
    fn block_size(&self) -> usize {
        self.block_size()
    }

    fn num_blocks(&self) -> usize {
        self.num_blocks()
    }

    fn bytes_len(&self) -> usize {
        self.bytes_len()
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vmemory_block_region_allocates_single_and_multi_block_chunks() {
        let mut region = VMemoryBlockRegion::new(8).unwrap();

        assert_eq!(region.block_size(), BLOCK_SIZE);
        assert_eq!(region.num_blocks(), 8);
        assert_eq!(region.bytes_len(), 8 * BLOCK_SIZE);

        let single = region.alloc_chunk(1).unwrap();
        assert_eq!(single.start_block(), 0);
        assert_eq!(single.block_count(), 1);
        assert_eq!(single.byte_range(), 0..BLOCK_SIZE);

        let multi = region.alloc_chunk(3).unwrap();
        assert_eq!(multi.start_block(), 1);
        assert_eq!(multi.block_count(), 3);
        assert_eq!(multi.byte_range(), BLOCK_SIZE..(4 * BLOCK_SIZE));
    }

    #[test]
    fn vmemory_block_region_initializes_line_marks() {
        let mut region = VMemoryBlockRegion::new(2).unwrap();
        let chunk = region.alloc_chunk(1).unwrap();

        assert_eq!(region.lines_per_block(), BLOCK_SIZE / IMMIX_LINE_SIZE);
        assert_eq!(region.line_count(), 2 * (BLOCK_SIZE / IMMIX_LINE_SIZE));
        assert_eq!(region.line_mark(0).unwrap(), LINE_FREE);

        region.mark_line(0).unwrap();
        assert_eq!(region.line_mark(0).unwrap(), LINE_MARKED);

        region.reset_line_marks(chunk).unwrap();
        assert_eq!(region.line_mark(0).unwrap(), LINE_FREE);
    }

    #[test]
    fn vmemory_block_region_flush_and_fence_are_supported_noops() {
        let region = VMemoryBlockRegion::new(1).unwrap();

        region.flush(0, 64).unwrap();
        region.fence().unwrap();
    }

    #[test]
    fn pmem_research_mode_allows_non_durable_flush_engine() {
        let engine = PersistEngine::for_mode(PersistenceMode::ResearchPretendPmem).unwrap();

        assert_eq!(engine.mode(), PersistenceMode::ResearchPretendPmem);
        engine.flush(core::ptr::NonNull::<u8>::dangling(), 0).unwrap();
        engine.fence().unwrap();
    }

    #[test]
    fn pmem_research_mode_allows_nonzero_non_durable_flush() {
        let engine = PersistEngine::for_mode(PersistenceMode::ResearchPretendPmem).unwrap();
        let mut bytes = [1u8, 2, 3, 4];
        let ptr = core::ptr::NonNull::from(&mut bytes[0]);

        engine.flush(ptr, bytes.len()).unwrap();
        engine.fence().unwrap();
        assert_eq!(bytes, [1, 2, 3, 4]);
    }

    #[test]
    fn pmem_real_mode_reports_platform_availability() {
        let result = PersistEngine::for_mode(PersistenceMode::RequireHardwarePmem);

        #[cfg(target_arch = "x86_64")]
        {
            if PersistEngine::hardware_flush_available() {
                assert!(result.is_ok());
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
    fn nvmemory_block_region_allocates_and_persists_writes_in_research_mode() {
        let mut region =
            NVMemoryBlockRegion::new_for_test(2, PersistenceMode::ResearchPretendPmem).unwrap();
        let chunk = region.alloc_chunk(1).unwrap();
        let range = chunk.byte_range();

        region.write(range.start, &[1, 2, 3, 4]).unwrap();
        region.flush(range.start, 4).unwrap();
        region.fence().unwrap();

        assert_eq!(region.read(range.start, 4).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(region.block_size(), BLOCK_SIZE);
        assert_eq!(region.num_blocks(), 2);
    }

    #[test]
    fn nvmemory_block_region_rejects_out_of_bounds_flush() {
        let region =
            NVMemoryBlockRegion::new_for_test(1, PersistenceMode::ResearchPretendPmem).unwrap();

        let error = region.flush(BLOCK_SIZE, 1).unwrap_err().to_string();
        assert!(error.contains("flush range out of bounds"));
    }
}
