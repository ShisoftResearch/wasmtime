//! Wizard-style block/chunk region layout for transactional storage.

#![allow(dead_code)]

use crate::prelude::*;
use core::{mem::size_of, ops::Range};

pub(super) const BLOCK_SIZE: usize = 512 * 1024;
pub(super) const IMMIX_LINE_SIZE: usize = 256;

pub(super) const PW_REGION_HEADER_SIZE: usize = size_of::<PWRegionHeader>();
pub(super) const META_DATA_DESC_SIZE: usize = size_of::<MetaDataDesc>();
pub(super) const BLOCK_ENTRY_SIZE: usize = size_of::<BlockEntry>();
pub(super) const CHUNK_HEADER_SIZE: usize = size_of::<ChunkHeader>();
pub(super) const LINE_MARK_SIZE: usize = size_of::<LineMark>();

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct PWRegionHeader {
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
pub(super) struct MetaDataDesc {
    pub kind: u64,
    pub fixed_bytes: u32,
    pub unit_size: u32,
    pub bytes_per_unit: u32,
    padding: u32,
    pub offset: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct BlockEntry {
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
pub(super) struct ChunkHeader {
    pub offset: u64,
    pub limit: u64,
    pub chunk_limit: u64,
    pub linemarks: u64,
}

#[repr(i16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ListKind {
    Metadata = -1,
    None = -2,
    Used = -3,
    SmallFree = 0,
    LargeFree = 1,
}

pub(super) struct BlockLists;

impl BlockLists {
    pub(super) const MAX_LISTS: usize = 8;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct LineMark {
    pub mark: u8,
}

pub(super) const IMMIX_LINE_MARK_META_KIND: u64 = 0x0000_0001_0000_0000;
pub(super) const IMMIX_LINE_MARK_RESET_VALUE: u8 = 0;
pub(super) const LINE_FREE: u8 = 0;
pub(super) const LINE_MARKED: u8 = 1;

pub(super) trait BlockRegionBackend {
    fn block_size(&self) -> usize;
    fn num_blocks(&self) -> usize;
    fn bytes_len(&self) -> usize;
    fn alloc_chunk(&mut self, block_count: usize) -> Result<RegionChunk>;
    fn flush(&self, offset: usize, len: usize) -> Result<()>;
    fn fence(&self) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RegionChunk {
    start_block: usize,
    block_count: usize,
}

impl RegionChunk {
    pub(super) fn start_block(&self) -> usize {
        self.start_block
    }

    pub(super) fn block_count(&self) -> usize {
        self.block_count
    }

    pub(super) fn byte_range(&self) -> Range<usize> {
        let start = self.start_block * BLOCK_SIZE;
        start..start + self.block_count * BLOCK_SIZE
    }
}

#[derive(Debug)]
pub(super) struct VMemoryBlockRegion {
    data: Vec<u8>,
    block_entries: Vec<BlockEntry>,
    line_marks: Vec<LineMark>,
}

impl VMemoryBlockRegion {
    pub(super) fn new(num_blocks: usize) -> Result<Self> {
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

    pub(super) fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    pub(super) fn num_blocks(&self) -> usize {
        self.block_entries.len()
    }

    pub(super) fn bytes_len(&self) -> usize {
        self.data.len()
    }

    pub(super) fn lines_per_block(&self) -> usize {
        BLOCK_SIZE / IMMIX_LINE_SIZE
    }

    pub(super) fn line_count(&self) -> usize {
        self.line_marks.len()
    }

    pub(super) fn alloc_chunk(&mut self, block_count: usize) -> Result<RegionChunk> {
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

    pub(super) fn line_mark(&self, line_index: usize) -> Result<u8> {
        self.line_marks
            .get(line_index)
            .map(|mark| mark.mark)
            .context("transactional line mark index out of bounds")
    }

    pub(super) fn mark_line(&mut self, line_index: usize) -> Result<()> {
        let line = self
            .line_marks
            .get_mut(line_index)
            .context("transactional line mark index out of bounds")?;
        line.mark = LINE_MARKED;
        Ok(())
    }

    pub(super) fn reset_line_marks(&mut self, chunk: RegionChunk) -> Result<()> {
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

    pub(super) fn flush(&self, offset: usize, len: usize) -> Result<()> {
        let end = offset
            .checked_add(len)
            .context("transactional block flush range overflow")?;
        ensure!(
            end <= self.data.len(),
            "transactional block flush range out of bounds"
        );
        Ok(())
    }

    pub(super) fn fence(&self) -> Result<()> {
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
}
