//! Logical linear view over transactional block-region chunks.

#![allow(dead_code)]

use crate::prelude::*;

use super::block_region::{
    BLOCK_SIZE, BlockRegionBackend, ChunkList, FileBackedMapping, FileBackedRegionMode,
    IMMIX_LINE_SIZE, NVMemoryBlockRegion, PersistenceMode, RegionChunk, VMemoryBlockRegion,
};

pub(super) trait LinearRegionBackend:
    BlockRegionBackend + core::fmt::Debug + Send + Sync
{
}

impl<T> LinearRegionBackend for T where T: BlockRegionBackend + core::fmt::Debug + Send + Sync {}

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
            linear: MappedLinearRegion::new(Box::new(backend), chunks),
        })
    }

    pub(super) fn new_nvmemory(byte_capacity: usize, mode: PersistenceMode) -> Result<Self> {
        let block_count = byte_capacity.div_ceil(BLOCK_SIZE);
        let mut backend = NVMemoryBlockRegion::new(block_count, mode)?;
        let chunks = if block_count == 0 {
            Vec::new()
        } else {
            vec![backend.alloc_chunk(block_count)?]
        };
        let chunks = ChunkList::from_chunks(chunks)?;
        Ok(Self {
            linear: MappedLinearRegion::new(Box::new(backend), chunks),
        })
    }

    pub(super) fn new_file_backed(
        byte_capacity: usize,
        mode: FileBackedRegionMode,
    ) -> Result<Self> {
        let block_count = byte_capacity.div_ceil(BLOCK_SIZE);
        let mut backend = FileBackedLinearRegion::new(block_count, byte_capacity, mode)?;
        let chunks = if block_count == 0 {
            Vec::new()
        } else {
            vec![backend.alloc_chunk(block_count)?]
        };
        let chunks = ChunkList::from_chunks(chunks)?;
        Ok(Self {
            linear: MappedLinearRegion::new(Box::new(backend), chunks),
        })
    }

    pub(super) fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        self.linear.read(offset, len)
    }

    pub(super) fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        self.linear.write(offset, bytes)
    }

    pub(super) fn flush(&self, offset: usize, len: usize) -> Result<()> {
        self.linear.flush(offset, len)
    }

    pub(super) fn fence(&self) -> Result<()> {
        self.linear.fence()
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

    pub(super) fn reserve_file_backed_capacity(&mut self, byte_capacity: usize) -> Result<()> {
        self.linear.reserve_file_backed_capacity(byte_capacity)
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
struct FileBackedLinearRegion {
    mapping: FileBackedMapping,
    block_count: usize,
    next_free_block: usize,
}

impl FileBackedLinearRegion {
    fn new(block_count: usize, bytes_len: usize, mode: FileBackedRegionMode) -> Result<Self> {
        Ok(Self {
            mapping: FileBackedMapping::new(mode, bytes_len)?,
            block_count,
            next_free_block: 0,
        })
    }
}

impl BlockRegionBackend for FileBackedLinearRegion {
    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    fn num_blocks(&self) -> usize {
        self.block_count
    }

    fn bytes_len(&self) -> usize {
        self.mapping.len()
    }

    fn line_mark_count(&self) -> usize {
        self.block_count * (BLOCK_SIZE / IMMIX_LINE_SIZE)
    }

    fn alloc_chunk(&mut self, block_count: usize) -> Result<RegionChunk> {
        ensure!(
            block_count > 0,
            "transactional linear file-backed chunk must contain a block"
        );
        let start = self.next_free_block;
        let end = start
            .checked_add(block_count)
            .context("transactional linear file-backed chunk range overflow")?;
        ensure!(
            end <= self.num_blocks(),
            "transactional linear file-backed region is out of contiguous chunks"
        );
        self.next_free_block = end;
        Ok(RegionChunk::new(start, block_count))
    }

    fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        self.mapping.read(offset, len)
    }

    fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        self.mapping.write(offset, bytes)
    }

    fn flush(&self, offset: usize, len: usize) -> Result<()> {
        self.mapping.flush(offset, len)
    }

    fn fence(&self) -> Result<()> {
        self.mapping.fence_data()
    }

    fn resize_bytes(&mut self, new_len: usize) -> Result<()> {
        ensure!(
            new_len >= self.mapping.len(),
            "transactional linear file-backed region cannot shrink"
        );
        if new_len == self.mapping.len() {
            return Ok(());
        }
        self.mapping.remap_len(new_len)
    }

    fn grow_to_blocks(&mut self, new_block_count: usize) -> Result<Option<RegionChunk>> {
        let old_block_count = self.block_count;
        ensure!(
            new_block_count >= old_block_count,
            "transactional linear file-backed region cannot shrink"
        );
        if new_block_count == old_block_count {
            return Ok(None);
        }

        let additional_blocks = new_block_count - old_block_count;
        ensure!(
            self.next_free_block == old_block_count,
            "transactional linear file-backed region has unallocated gap before grow"
        );
        self.block_count = new_block_count;
        self.next_free_block = new_block_count;
        Ok(Some(RegionChunk::new(old_block_count, additional_blocks)))
    }
}

#[derive(Debug)]
pub(super) struct MappedLinearRegion {
    backend: Box<dyn LinearRegionBackend>,
    chunks: ChunkList,
}

impl MappedLinearRegion {
    pub(super) fn new(backend: Box<dyn LinearRegionBackend>, chunks: ChunkList) -> Self {
        Self { backend, chunks }
    }

    pub(super) fn logical_len(&self) -> usize {
        self.chunks.logical_len()
    }

    pub(super) fn line_mark_count_for_test(&self) -> usize {
        self.backend.line_mark_count()
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

    pub(super) fn flush(&self, offset: usize, len: usize) -> Result<()> {
        self.check_range(offset, len, "flush")?;
        let mut cursor = offset;
        let end = offset + len;

        while cursor < end {
            let segment = self
                .logical_segment(cursor)
                .context("transactional linear flush segment missing")?;
            let take = segment.available.min(end - cursor);
            self.backend.flush(segment.backend_offset, take)?;
            cursor += take;
        }

        Ok(())
    }

    pub(super) fn fence(&self) -> Result<()> {
        self.backend.fence()
    }

    pub(super) fn reserve_file_backed_capacity(&mut self, byte_capacity: usize) -> Result<()> {
        let new_block_count = byte_capacity.div_ceil(BLOCK_SIZE);
        ensure!(
            new_block_count >= self.backend.num_blocks(),
            "transactional linear file-backed reserve cannot shrink"
        );
        self.backend.resize_bytes(byte_capacity)?;
        if new_block_count == self.backend.num_blocks() {
            return Ok(());
        }

        if let Some(chunk) = self.backend.grow_to_blocks(new_block_count)? {
            let mut chunks = self.chunks.chunks().to_vec();
            chunks.push(chunk);
            self.chunks = ChunkList::from_chunks(chunks)?;
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
        BLOCK_SIZE, ChunkList, FileBackedRegionMode, NVMemoryBlockRegion, PersistenceMode,
        VMemoryBlockRegion,
    };
    use core::ops::Range;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[derive(Debug)]
    struct TestBackend {
        bytes_len: usize,
        line_mark_count: usize,
    }

    impl BlockRegionBackend for TestBackend {
        fn block_size(&self) -> usize {
            BLOCK_SIZE
        }

        fn num_blocks(&self) -> usize {
            self.bytes_len.div_ceil(BLOCK_SIZE)
        }

        fn bytes_len(&self) -> usize {
            self.bytes_len
        }

        fn line_mark_count(&self) -> usize {
            self.line_mark_count
        }

        fn alloc_chunk(
            &mut self,
            _block_count: usize,
        ) -> Result<super::super::block_region::RegionChunk> {
            unreachable!("test backend does not allocate chunks")
        }

        fn read(&self, _offset: usize, _len: usize) -> Result<Vec<u8>> {
            unreachable!("test backend does not read data")
        }

        fn write(&mut self, _offset: usize, _bytes: &[u8]) -> Result<()> {
            unreachable!("test backend does not write data")
        }

        fn flush(&self, _offset: usize, _len: usize) -> Result<()> {
            Ok(())
        }

        fn fence(&self) -> Result<()> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct RecordingBackend {
        inner: VMemoryBlockRegion,
        flushes: Arc<Mutex<Vec<Range<usize>>>>,
        fence_count: Arc<AtomicUsize>,
    }

    impl RecordingBackend {
        fn new(
            num_blocks: usize,
        ) -> Result<(Self, Arc<Mutex<Vec<Range<usize>>>>, Arc<AtomicUsize>)> {
            let flushes = Arc::new(Mutex::new(Vec::new()));
            let fence_count = Arc::new(AtomicUsize::new(0));
            Ok((
                Self {
                    inner: VMemoryBlockRegion::new(num_blocks)?,
                    flushes: Arc::clone(&flushes),
                    fence_count: Arc::clone(&fence_count),
                },
                flushes,
                fence_count,
            ))
        }
    }

    impl BlockRegionBackend for RecordingBackend {
        fn block_size(&self) -> usize {
            self.inner.block_size()
        }

        fn num_blocks(&self) -> usize {
            self.inner.num_blocks()
        }

        fn bytes_len(&self) -> usize {
            self.inner.bytes_len()
        }

        fn line_mark_count(&self) -> usize {
            self.inner.line_mark_count()
        }

        fn alloc_chunk(
            &mut self,
            block_count: usize,
        ) -> Result<super::super::block_region::RegionChunk> {
            self.inner.alloc_chunk(block_count)
        }

        fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
            self.inner.read(offset, len)
        }

        fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
            self.inner.write(offset, bytes)
        }

        fn flush(&self, offset: usize, len: usize) -> Result<()> {
            self.flushes.lock().unwrap().push(offset..offset + len);
            Ok(())
        }

        fn fence(&self) -> Result<()> {
            self.fence_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn mapped_linear_region_reads_across_non_contiguous_chunks() {
        let mut backend = VMemoryBlockRegion::new(4).unwrap();
        let first = backend.alloc_chunk(1).unwrap();
        let _gap = backend.alloc_chunk(1).unwrap();
        let second = backend.alloc_chunk(1).unwrap();
        let chunks = ChunkList::from_chunks(vec![first, second]).unwrap();
        let mut region = MappedLinearRegion::new(Box::new(backend), chunks);

        region.write(0, &[1, 2, 3]).unwrap();
        region.write(BLOCK_SIZE - 2, &[4, 5, 6, 7]).unwrap();

        assert_eq!(region.logical_len(), 2 * BLOCK_SIZE);
        assert_eq!(region.read(0, 3).unwrap(), vec![1, 2, 3]);
        assert_eq!(region.read(BLOCK_SIZE - 2, 4).unwrap(), vec![4, 5, 6, 7]);
    }

    #[test]
    fn mapped_linear_region_supports_nvmemory_backend() {
        let mut backend =
            NVMemoryBlockRegion::new_for_test(2, PersistenceMode::ResearchPretendPmem).unwrap();
        let chunk = backend.alloc_chunk(1).unwrap();
        let chunks = ChunkList::from_chunks(vec![chunk]).unwrap();
        let mut region = MappedLinearRegion::new(Box::new(backend), chunks);

        region.write(0, &[9, 8, 7]).unwrap();
        assert_eq!(region.read(0, 3).unwrap(), vec![9, 8, 7]);
    }

    #[test]
    fn mapped_linear_region_reports_backend_owned_line_mark_count() {
        let chunk = VMemoryBlockRegion::new(1).unwrap().alloc_chunk(1).unwrap();
        let chunks = ChunkList::from_chunks(vec![chunk]).unwrap();
        let region = MappedLinearRegion::new(
            Box::new(TestBackend {
                bytes_len: BLOCK_SIZE,
                line_mark_count: (BLOCK_SIZE / IMMIX_LINE_SIZE) - 1,
            }),
            chunks,
        );

        assert_eq!(
            region.line_mark_count_for_test(),
            (BLOCK_SIZE / IMMIX_LINE_SIZE) - 1
        );
    }

    #[test]
    fn mapped_linear_region_flushes_across_chunk_boundaries() {
        let (mut backend, flushes, _) = RecordingBackend::new(4).unwrap();
        let first = backend.alloc_chunk(1).unwrap();
        let _gap = backend.alloc_chunk(1).unwrap();
        let second = backend.alloc_chunk(1).unwrap();
        let expected = vec![
            (BLOCK_SIZE - 2)..BLOCK_SIZE,
            (2 * BLOCK_SIZE)..(2 * BLOCK_SIZE + 2),
        ];
        let chunks = ChunkList::from_chunks(vec![first, second]).unwrap();
        let region = MappedLinearRegion::new(Box::new(backend), chunks);

        region.flush(BLOCK_SIZE - 2, 4).unwrap();

        assert_eq!(*flushes.lock().unwrap(), expected);
    }

    #[test]
    fn mapped_linear_region_flush_zero_len_and_fence_are_forwarded() {
        let (mut backend, flushes, fence_count) = RecordingBackend::new(1).unwrap();
        let chunk = backend.alloc_chunk(1).unwrap();
        let chunks = ChunkList::from_chunks(vec![chunk]).unwrap();
        let region = MappedLinearRegion::new(Box::new(backend), chunks);

        region.flush(BLOCK_SIZE, 0).unwrap();
        region.fence().unwrap();

        assert!(flushes.lock().unwrap().is_empty());
        assert_eq!(fence_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn mapped_linear_region_rejects_out_of_bounds_flush() {
        let (mut backend, _, _) = RecordingBackend::new(1).unwrap();
        let chunk = backend.alloc_chunk(1).unwrap();
        let chunks = ChunkList::from_chunks(vec![chunk]).unwrap();
        let region = MappedLinearRegion::new(Box::new(backend), chunks);

        let error = region.flush(BLOCK_SIZE, 1).unwrap_err().to_string();

        assert!(error.contains("transactional linear flush range out of bounds"));
    }

    #[test]
    fn vmemory_region_reports_backend_line_mark_count() {
        let empty = TMemoryRegion::new(0).unwrap();
        assert_eq!(empty.line_mark_count_for_test(), 0);

        let one_byte = TMemoryRegion::new(1).unwrap();
        assert_eq!(
            one_byte.line_mark_count_for_test(),
            BLOCK_SIZE / IMMIX_LINE_SIZE
        );
    }

    #[test]
    fn nvmemory_region_reports_backend_line_mark_count() {
        let empty = TMemoryRegion::new_nvmemory(0, PersistenceMode::ResearchPretendPmem).unwrap();
        assert_eq!(empty.line_mark_count_for_test(), 0);

        let one_byte =
            TMemoryRegion::new_nvmemory(1, PersistenceMode::ResearchPretendPmem).unwrap();
        assert_eq!(
            one_byte.line_mark_count_for_test(),
            BLOCK_SIZE / IMMIX_LINE_SIZE
        );
    }

    #[test]
    fn nvmemory_region_flush_and_fence_paths_are_supported() {
        let mut region =
            TMemoryRegion::new_nvmemory(BLOCK_SIZE, PersistenceMode::ResearchPretendPmem).unwrap();

        region.write(0, &[1, 2, 3, 4]).unwrap();
        region.flush(0, 4).unwrap();
        region.flush(BLOCK_SIZE, 0).unwrap();
        region.fence().unwrap();

        let error = region.flush(BLOCK_SIZE, 1).unwrap_err().to_string();
        assert!(error.contains("transactional linear flush range out of bounds"));
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_region_reports_backend_line_mark_count() {
        let empty = TMemoryRegion::new_file_backed(0, FileBackedRegionMode::Temp).unwrap();
        assert_eq!(empty.line_mark_count_for_test(), 0);

        let one_byte = TMemoryRegion::new_file_backed(1, FileBackedRegionMode::Temp).unwrap();
        assert_eq!(
            one_byte.line_mark_count_for_test(),
            BLOCK_SIZE / IMMIX_LINE_SIZE
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_region_flush_and_fence_paths_are_supported() {
        let mut region =
            TMemoryRegion::new_file_backed(BLOCK_SIZE, FileBackedRegionMode::Temp).unwrap();

        region.write(0, &[1, 2, 3, 4]).unwrap();
        region.flush(0, 4).unwrap();
        region.fence().unwrap();

        assert_eq!(region.read(0, 4).unwrap(), vec![1, 2, 3, 4]);
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_region_reserve_capacity_extends_existing_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("linear-grow.tmemory");
        let mut region =
            TMemoryRegion::new_file_backed(BLOCK_SIZE, FileBackedRegionMode::Path(path.clone()))
                .unwrap();

        region.write(16, &[1, 2, 3, 4]).unwrap();
        region.flush(16, 4).unwrap();
        region.reserve_file_backed_capacity(2 * BLOCK_SIZE).unwrap();

        assert_eq!(
            region.line_mark_count_for_test(),
            2 * (BLOCK_SIZE / IMMIX_LINE_SIZE)
        );
        assert_eq!(region.read(16, 4).unwrap(), vec![1, 2, 3, 4]);

        region.write(BLOCK_SIZE, &[5, 6, 7, 8]).unwrap();
        region.flush(BLOCK_SIZE, 4).unwrap();
        region.fence().unwrap();

        let bytes = std::fs::read(path).unwrap();
        assert_eq!(&bytes[16..20], &[1, 2, 3, 4]);
        assert_eq!(&bytes[BLOCK_SIZE..BLOCK_SIZE + 4], &[5, 6, 7, 8]);
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_region_reopens_raw_linear_payload_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("linear-reopen.tmemory");
        {
            let mut region = TMemoryRegion::new_file_backed(
                BLOCK_SIZE,
                FileBackedRegionMode::Path(path.clone()),
            )
            .unwrap();
            region.write(32, &[1, 2, 3, 4]).unwrap();
            region.flush(32, 4).unwrap();
            region.fence().unwrap();
        }

        let region = TMemoryRegion::new_file_backed(
            BLOCK_SIZE,
            FileBackedRegionMode::OpenExistingPath(path),
        )
        .unwrap();

        assert_eq!(region.read(32, 4).unwrap(), vec![1, 2, 3, 4]);
    }
}
