//! Logical linear view over transactional block-region chunks.

#![allow(dead_code)]

use crate::prelude::*;

use super::block_region::{BLOCK_SIZE, ChunkList, IMMIX_LINE_SIZE, VMemoryBlockRegion};

#[derive(Debug)]
pub(super) struct TMemoryRegion {
    linear: MappedLinearRegion,
}

impl TMemoryRegion {
    pub(super) fn new(byte_capacity: usize) -> Result<Self> {
        let block_count = byte_capacity.div_ceil(BLOCK_SIZE);
        let mut backend = VMemoryBlockRegion::new(block_count)?;
        let chunks = if block_count == 0 {
            Vec::new()
        } else {
            vec![backend.alloc_chunk(block_count)?]
        };
        let chunks = ChunkList::from_chunks(chunks)?;
        Ok(Self {
            linear: MappedLinearRegion::new(backend, chunks),
        })
    }

    pub(super) fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        self.linear.read(offset, len)
    }

    pub(super) fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        self.linear.write(offset, bytes)
    }

    pub(super) fn fill(&mut self, range: core::ops::Range<usize>, byte: u8) -> Result<()> {
        ensure!(
            range.start <= range.end,
            "transactional linear fill invalid range"
        );
        let len = range.end - range.start;
        if len == 0 {
            return Ok(());
        }
        self.linear.write(range.start, &vec![byte; len])
    }

    pub(super) fn block_size_for_test(&self) -> usize {
        BLOCK_SIZE
    }

    pub(super) fn immix_line_size_for_test(&self) -> usize {
        IMMIX_LINE_SIZE
    }

    pub(super) fn line_mark_count_for_test(&self) -> usize {
        self.linear.line_mark_count_for_test()
    }
}

#[derive(Debug)]
pub(super) struct MappedLinearRegion {
    backend: VMemoryBlockRegion,
    chunks: ChunkList,
}

impl MappedLinearRegion {
    pub(super) fn new(backend: VMemoryBlockRegion, chunks: ChunkList) -> Self {
        Self { backend, chunks }
    }

    pub(super) fn logical_len(&self) -> usize {
        self.chunks.logical_len()
    }

    pub(super) fn line_mark_count_for_test(&self) -> usize {
        self.backend.line_count()
    }

    pub(super) fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        self.check_range(offset, len, "read")?;
        let mut out = Vec::with_capacity(len);
        let mut cursor = offset;
        let end = offset + len;

        while cursor < end {
            let segment = self
                .logical_segment(cursor)
                .context("transactional linear read segment missing")?;
            let take = segment.available.min(end - cursor);
            out.extend_from_slice(&self.backend.read(segment.backend_offset, take)?);
            cursor += take;
        }

        Ok(out)
    }

    pub(super) fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        self.check_range(offset, bytes.len(), "write")?;
        let mut cursor = offset;
        let mut input = 0usize;
        let end = offset + bytes.len();

        while cursor < end {
            let segment = self
                .logical_segment(cursor)
                .context("transactional linear write segment missing")?;
            let take = segment.available.min(end - cursor);
            self.backend
                .write(segment.backend_offset, &bytes[input..input + take])?;
            cursor += take;
            input += take;
        }

        Ok(())
    }

    fn check_range(&self, offset: usize, len: usize, op: &str) -> Result<()> {
        let end = offset
            .checked_add(len)
            .with_context(|| format!("transactional linear {op} range overflow"))?;
        ensure!(
            end <= self.logical_len(),
            "transactional linear {op} range out of bounds"
        );
        Ok(())
    }

    fn logical_segment(&self, offset: usize) -> Option<LogicalSegment> {
        let mut logical_base = 0usize;
        for chunk in self.chunks.chunks() {
            let chunk_len = chunk.byte_len();
            let chunk_end = logical_base.checked_add(chunk_len)?;
            if offset < chunk_end {
                let offset_in_chunk = offset - logical_base;
                return Some(LogicalSegment {
                    backend_offset: chunk.byte_range().start + offset_in_chunk,
                    available: chunk_len - offset_in_chunk,
                });
            }
            logical_base = chunk_end;
        }
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LogicalSegment {
    backend_offset: usize,
    available: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::vm::memory::tmemory::block_region::{
        BLOCK_SIZE, ChunkList, VMemoryBlockRegion,
    };

    #[test]
    fn mapped_linear_region_reads_across_non_contiguous_chunks() {
        let mut backend = VMemoryBlockRegion::new(4).unwrap();
        let first = backend.alloc_chunk(1).unwrap();
        let _gap = backend.alloc_chunk(1).unwrap();
        let second = backend.alloc_chunk(1).unwrap();
        let chunks = ChunkList::from_chunks(vec![first, second]).unwrap();
        let mut region = MappedLinearRegion::new(backend, chunks);

        region.write(0, &[1, 2, 3]).unwrap();
        region.write(BLOCK_SIZE - 2, &[4, 5, 6, 7]).unwrap();

        assert_eq!(region.logical_len(), 2 * BLOCK_SIZE);
        assert_eq!(region.read(0, 3).unwrap(), vec![1, 2, 3]);
        assert_eq!(region.read(BLOCK_SIZE - 2, 4).unwrap(), vec![4, 5, 6, 7]);
    }
}
