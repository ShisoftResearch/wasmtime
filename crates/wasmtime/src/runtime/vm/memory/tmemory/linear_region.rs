//! Logical linear view over transactional block-region chunks.

#![allow(dead_code)]

use crate::prelude::*;
use crate::runtime::transaction::{TMemoryDaxPmemBacking, TMemoryRegionConfig};
use std::sync::Mutex;

#[cfg(not(target_os = "linux"))]
use super::block_region::FIXED_WINDOW_TMEMORY_UNSUPPORTED_MESSAGE;
use super::block_region::{
    BLOCK_SIZE, BlockRegionBackend, ChunkList, DaxPmemBlockRegion, FileBackedMapping,
    FileBackedRegionMode, IMMIX_LINE_SIZE, MappedAddressWindow, RegionChunk, VMemoryBlockRegion,
};
use super::metadata::reserved_metadata_blocks;

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

    pub(super) fn new_dax_pmem(
        byte_capacity: usize,
        backing: TMemoryDaxPmemBacking,
    ) -> Result<Self> {
        let block_count = byte_capacity.div_ceil(BLOCK_SIZE);
        let (mut backend, use_existing_payload_chunk) = match backing {
            TMemoryDaxPmemBacking::ResearchTemp => {
                (DaxPmemBlockRegion::new_research(block_count)?, false)
            }
            TMemoryDaxPmemBacking::FsDaxPath(path) => (
                DaxPmemBlockRegion::create_fsdax_path(path, block_count)?,
                false,
            ),
            TMemoryDaxPmemBacking::ExistingFsDaxPath(path) => (
                DaxPmemBlockRegion::open_fsdax_path(path, block_count)?,
                true,
            ),
            TMemoryDaxPmemBacking::FsDaxRegions(regions) => {
                return Self::new_dax_pmem_regions(byte_capacity, regions, false);
            }
            TMemoryDaxPmemBacking::ExistingFsDaxRegions(regions) => {
                return Self::new_dax_pmem_regions(byte_capacity, regions, true);
            }
        };
        let chunks = if use_existing_payload_chunk {
            backend
                .existing_payload_chunk_from_image(block_count)?
                .into_iter()
                .collect()
        } else if block_count == 0 {
            Vec::new()
        } else {
            vec![backend.alloc_chunk(block_count)?]
        };
        let chunks = ChunkList::from_chunks(chunks)?;
        Ok(Self {
            linear: MappedLinearRegion::new(Box::new(backend), chunks),
        })
    }

    pub(super) fn new_dax_pmem_regions(
        byte_capacity: usize,
        regions: Vec<TMemoryRegionConfig>,
        existing: bool,
    ) -> Result<Self> {
        let mut children = Vec::with_capacity(regions.len());
        let mut chunks = Vec::new();
        let mut global_byte_base = 0usize;

        for region in regions {
            let max_region_blocks = region.reserved_len / BLOCK_SIZE;
            if max_region_blocks == 0 {
                continue;
            }

            let reserved_blocks = reserved_metadata_blocks(max_region_blocks)?;
            ensure!(
                max_region_blocks >= reserved_blocks,
                "multi-region DAX PMEM tmemory region {} cannot fit metadata into reserved window",
                region.path.display()
            );
            let payload_blocks = max_region_blocks - reserved_blocks;
            if payload_blocks == 0 {
                continue;
            }

            let window = MappedAddressWindow {
                base: region.address_base,
                reserved_len: region.reserved_len,
            };
            let mut backend = if existing {
                DaxPmemBlockRegion::open_fsdax_path_in_window(region.path, payload_blocks, window)?
            } else {
                DaxPmemBlockRegion::create_fsdax_path_in_window(
                    region.path,
                    payload_blocks,
                    window,
                )?
            };
            let child_chunk = backend
                .existing_payload_chunk_from_image(payload_blocks)?
                .context("multi-region DAX PMEM tmemory region is missing payload chunk")?;
            let global_block_base = global_byte_base / BLOCK_SIZE;
            chunks.push(RegionChunk::new(
                global_block_base + child_chunk.start_block(),
                child_chunk.block_count(),
            ));
            global_byte_base = global_byte_base
                .checked_add(backend.bytes_len())
                .context("multi-region DAX PMEM tmemory backend byte range overflow")?;
            children.push(CompositeLinearRegionChild::new(Box::new(backend)));
        }

        Self::new_multi_region(byte_capacity, children, chunks, "DAX PMEM", existing)
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

    pub(super) fn new_file_backed_regions(
        byte_capacity: usize,
        regions: Vec<TMemoryRegionConfig>,
        existing: bool,
    ) -> Result<Self> {
        let mut children = Vec::with_capacity(regions.len());
        let mut chunks = Vec::new();
        let mut global_byte_base = 0usize;

        for region in regions {
            let usable_blocks = region.reserved_len / BLOCK_SIZE;
            if usable_blocks == 0 {
                continue;
            }

            let window = MappedAddressWindow {
                base: region.address_base,
                reserved_len: region.reserved_len,
            };
            let mut backend = if existing {
                FileBackedLinearRegion::open_existing_in_window(region.path, window)?
            } else {
                FileBackedLinearRegion::create_in_window(region.path, usable_blocks, window)?
            };
            if backend.num_blocks() == 0 {
                continue;
            }
            let child_chunk = if existing {
                RegionChunk::new(0, backend.num_blocks())
            } else {
                backend.alloc_chunk(backend.num_blocks())?
            };
            let global_block_base = global_byte_base / BLOCK_SIZE;
            chunks.push(RegionChunk::new(
                global_block_base + child_chunk.start_block(),
                child_chunk.block_count(),
            ));
            global_byte_base = global_byte_base
                .checked_add(backend.bytes_len())
                .context("multi-region file-backed tmemory backend byte range overflow")?;
            children.push(CompositeLinearRegionChild::new(Box::new(backend)));
        }

        Self::new_multi_region(byte_capacity, children, chunks, "file-backed", existing)
    }

    fn new_multi_region(
        byte_capacity: usize,
        children: Vec<CompositeLinearRegionChild>,
        payload_chunks: Vec<RegionChunk>,
        backend_name: &str,
        existing: bool,
    ) -> Result<Self> {
        let payload_chunks = ChunkList::from_chunks(payload_chunks)?;
        ensure!(
            byte_capacity <= payload_chunks.logical_len(),
            "multi-region {backend_name} tmemory requested capacity {byte_capacity} exceeds configured logical capacity {}",
            payload_chunks.logical_len()
        );
        let exposed_payload_blocks = if existing {
            payload_chunks.logical_len() / BLOCK_SIZE
        } else {
            byte_capacity.div_ceil(BLOCK_SIZE)
        };
        let chunks = payload_chunk_prefix(&payload_chunks, exposed_payload_blocks)?;
        Ok(Self {
            linear: MappedLinearRegion::new(
                Box::new(CompositeLinearRegion::new(
                    children,
                    payload_chunks,
                    exposed_payload_blocks,
                )),
                chunks,
            ),
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

    pub(super) fn logical_len(&self) -> usize {
        self.linear.logical_len()
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

    pub(super) fn reserve_capacity(&mut self, byte_capacity: usize) -> Result<()> {
        self.linear.reserve_capacity(byte_capacity)
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
        Ok(Self::with_mapping(
            FileBackedMapping::new(mode, bytes_len)?,
            block_count,
        ))
    }

    fn with_mapping(mapping: FileBackedMapping, block_count: usize) -> Self {
        Self {
            mapping,
            block_count,
            next_free_block: 0,
        }
    }

    #[cfg(target_os = "linux")]
    fn create_in_window(
        path: std::path::PathBuf,
        block_count: usize,
        window: MappedAddressWindow,
    ) -> Result<Self> {
        let bytes_len = block_count
            .checked_mul(BLOCK_SIZE)
            .context("transactional linear file-backed region size overflow")?;
        Ok(Self::with_mapping(
            FileBackedMapping::new_path_in_window(path, bytes_len, window)?,
            block_count,
        ))
    }

    #[cfg(not(target_os = "linux"))]
    fn create_in_window(
        _path: std::path::PathBuf,
        _block_count: usize,
        _window: MappedAddressWindow,
    ) -> Result<Self> {
        bail!("{FIXED_WINDOW_TMEMORY_UNSUPPORTED_MESSAGE}")
    }

    #[cfg(target_os = "linux")]
    fn open_existing_in_window(
        path: std::path::PathBuf,
        window: MappedAddressWindow,
    ) -> Result<Self> {
        let mapping = FileBackedMapping::open_existing_path_in_window(path, window)?;
        ensure!(
            mapping.len() % BLOCK_SIZE == 0,
            "existing multi-region file-backed tmemory length is not block-aligned"
        );
        let block_count = mapping.len() / BLOCK_SIZE;
        Ok(Self::with_mapping(mapping, block_count))
    }

    #[cfg(not(target_os = "linux"))]
    fn open_existing_in_window(
        _path: std::path::PathBuf,
        _window: MappedAddressWindow,
    ) -> Result<Self> {
        bail!("{FIXED_WINDOW_TMEMORY_UNSUPPORTED_MESSAGE}")
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

fn payload_chunk_prefix(payload_chunks: &ChunkList, payload_blocks: usize) -> Result<ChunkList> {
    let mut remaining_blocks = payload_blocks;
    let mut chunks = Vec::new();
    for chunk in payload_chunks.chunks() {
        if remaining_blocks == 0 {
            break;
        }
        let take = remaining_blocks.min(chunk.block_count());
        chunks.push(RegionChunk::new(chunk.start_block(), take));
        remaining_blocks -= take;
    }
    ensure!(
        remaining_blocks == 0,
        "transactional linear multi-region payload exposure exceeds configured chunk capacity"
    );
    ChunkList::from_chunks(chunks)
}

#[derive(Debug)]
struct CompositeLinearRegionChild {
    start: usize,
    span_bytes: usize,
    backend: Box<dyn LinearRegionBackend>,
}

impl CompositeLinearRegionChild {
    fn new(backend: Box<dyn LinearRegionBackend>) -> Self {
        Self {
            start: 0,
            span_bytes: backend.bytes_len(),
            backend,
        }
    }
}

#[derive(Debug)]
struct CompositeLinearRegion {
    children: Vec<CompositeLinearRegionChild>,
    dirty_children: Mutex<Vec<bool>>,
    bytes_len: usize,
    line_mark_count: usize,
    payload_chunks: ChunkList,
    exposed_payload_blocks: usize,
}

impl CompositeLinearRegion {
    fn new(
        mut children: Vec<CompositeLinearRegionChild>,
        payload_chunks: ChunkList,
        exposed_payload_blocks: usize,
    ) -> Self {
        let mut bytes_len = 0usize;
        let mut line_mark_count = 0usize;
        for child in &mut children {
            child.start = bytes_len;
            bytes_len += child.span_bytes;
            line_mark_count += child.backend.line_mark_count();
        }
        let dirty_children = Mutex::new(vec![false; children.len()]);
        Self {
            children,
            dirty_children,
            bytes_len,
            line_mark_count,
            payload_chunks,
            exposed_payload_blocks,
        }
    }

    fn child_segment(&self, offset: usize) -> Option<(usize, usize, usize)> {
        self.children.iter().enumerate().find_map(|(index, child)| {
            let end = child.start.checked_add(child.span_bytes)?;
            if child.start <= offset && offset < end {
                Some((index, offset - child.start, end - offset))
            } else {
                None
            }
        })
    }

    fn payload_block_capacity(&self) -> usize {
        self.payload_chunks.logical_len() / BLOCK_SIZE
    }
}

impl BlockRegionBackend for CompositeLinearRegion {
    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    fn num_blocks(&self) -> usize {
        self.bytes_len / BLOCK_SIZE
    }

    fn bytes_len(&self) -> usize {
        self.bytes_len
    }

    fn line_mark_count(&self) -> usize {
        self.line_mark_count
    }

    fn alloc_chunk(&mut self, _block_count: usize) -> Result<RegionChunk> {
        bail!("transactional linear multi-region backend does not support chunk allocation")
    }

    fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        let end = offset
            .checked_add(len)
            .context("transactional linear multi-region read range overflow")?;
        ensure!(
            end <= self.bytes_len,
            "transactional linear multi-region read range out of bounds"
        );

        let mut out = Vec::with_capacity(len);
        let mut cursor = offset;
        while cursor < end {
            let (index, local_offset, available) = self
                .child_segment(cursor)
                .context("transactional linear multi-region read segment missing")?;
            let take = available.min(end - cursor);
            out.extend_from_slice(&self.children[index].backend.read(local_offset, take)?);
            cursor += take;
        }
        Ok(out)
    }

    fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        let end = offset
            .checked_add(bytes.len())
            .context("transactional linear multi-region write range overflow")?;
        ensure!(
            end <= self.bytes_len,
            "transactional linear multi-region write range out of bounds"
        );

        let mut cursor = offset;
        let mut input = 0usize;
        while cursor < end {
            let (index, local_offset, available) = self
                .child_segment(cursor)
                .context("transactional linear multi-region write segment missing")?;
            let take = available.min(end - cursor);
            self.children[index]
                .backend
                .write(local_offset, &bytes[input..input + take])?;
            cursor += take;
            input += take;
        }
        Ok(())
    }

    fn flush(&self, offset: usize, len: usize) -> Result<()> {
        let end = offset
            .checked_add(len)
            .context("transactional linear multi-region flush range overflow")?;
        ensure!(
            end <= self.bytes_len,
            "transactional linear multi-region flush range out of bounds"
        );

        let mut cursor = offset;
        let mut touched = self.dirty_children.lock().map_err(|_| {
            crate::format_err!("transactional linear multi-region dirty set poisoned")
        })?;
        while cursor < end {
            let (index, local_offset, available) = self
                .child_segment(cursor)
                .context("transactional linear multi-region flush segment missing")?;
            let take = available.min(end - cursor);
            self.children[index].backend.flush(local_offset, take)?;
            touched[index] = true;
            cursor += take;
        }
        Ok(())
    }

    fn fence(&self) -> Result<()> {
        let dirty_indices = {
            let mut dirty = self.dirty_children.lock().map_err(|_| {
                crate::format_err!("transactional linear multi-region dirty set poisoned")
            })?;
            let indices = dirty
                .iter()
                .enumerate()
                .filter_map(|(index, is_dirty)| (*is_dirty).then_some(index))
                .collect::<Vec<_>>();
            for index in &indices {
                dirty[*index] = false;
            }
            indices
        };

        for (position, index) in dirty_indices.iter().copied().enumerate() {
            if let Err(error) = self.children[index].backend.fence() {
                let mut dirty = self.dirty_children.lock().map_err(|_| {
                    crate::format_err!("transactional linear multi-region dirty set poisoned")
                })?;
                for pending in dirty_indices[position..].iter().copied() {
                    dirty[pending] = true;
                }
                return Err(error);
            }
        }
        Ok(())
    }

    fn resize_bytes(&mut self, new_len: usize) -> Result<()> {
        ensure!(
            new_len <= self.bytes_len,
            "transactional linear multi-region backend cannot grow beyond configured capacity"
        );
        Ok(())
    }

    fn grow_linear_payload_to_blocks(
        &mut self,
        new_payload_block_count: usize,
    ) -> Result<Option<RegionChunk>> {
        ensure!(
            new_payload_block_count >= self.exposed_payload_blocks,
            "transactional linear multi-region backend cannot shrink exposed payload"
        );
        ensure!(
            new_payload_block_count <= self.payload_block_capacity(),
            "transactional linear multi-region backend cannot grow beyond configured payload capacity"
        );
        if new_payload_block_count == self.exposed_payload_blocks {
            return Ok(None);
        }

        let mut logical_block_base = 0usize;
        for chunk in self.payload_chunks.chunks() {
            let chunk_end = logical_block_base
                .checked_add(chunk.block_count())
                .context("transactional linear multi-region payload block range overflow")?;
            if self.exposed_payload_blocks < chunk_end {
                let offset_in_chunk = self
                    .exposed_payload_blocks
                    .saturating_sub(logical_block_base);
                let remaining_in_chunk = chunk.block_count() - offset_in_chunk;
                let needed_blocks = new_payload_block_count - self.exposed_payload_blocks;
                let grow_blocks = remaining_in_chunk.min(needed_blocks);
                let grown = RegionChunk::new(chunk.start_block() + offset_in_chunk, grow_blocks);
                self.exposed_payload_blocks += grow_blocks;
                return Ok(Some(grown));
            }
            logical_block_base = chunk_end;
        }

        bail!("transactional linear multi-region backend cannot expose requested payload blocks")
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

    pub(super) fn reserve_capacity(&mut self, byte_capacity: usize) -> Result<()> {
        self.reserve_backend_capacity(byte_capacity)
    }

    fn reserve_backend_capacity(&mut self, byte_capacity: usize) -> Result<()> {
        let new_block_count = byte_capacity.div_ceil(BLOCK_SIZE);
        let mut current_block_count = self.chunks.logical_len().div_ceil(BLOCK_SIZE);
        ensure!(
            new_block_count >= current_block_count,
            "transactional linear region reserve cannot shrink"
        );
        self.backend.resize_bytes(byte_capacity)?;
        if new_block_count == current_block_count {
            return Ok(());
        }

        while current_block_count < new_block_count {
            let chunk = self
                .backend
                .grow_linear_payload_to_blocks(new_block_count)?
                .context("transactional linear region backend did not expose requested capacity")?;
            let grown_block_count = chunk.block_count();
            let mut chunks = self.chunks.chunks().to_vec();
            chunks.push(chunk);
            self.chunks = ChunkList::from_chunks(chunks)?;
            current_block_count = current_block_count
                .checked_add(grown_block_count)
                .context("transactional linear region block count overflow")?;
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
        BLOCK_SIZE, ChunkList, DaxPmemBlockRegion, FileBackedRegionMode, RegionChunk,
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
    fn mapped_linear_region_supports_dax_pmem_backend() {
        let mut backend = DaxPmemBlockRegion::new_for_test(2).unwrap();
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
    fn composite_linear_region_fences_only_children_touched_by_flush() {
        let (first, first_flushes, first_fences) = RecordingBackend::new(1).unwrap();
        let (second, second_flushes, second_fences) = RecordingBackend::new(1).unwrap();
        let chunks =
            ChunkList::from_chunks(vec![RegionChunk::new(0, 1), RegionChunk::new(1, 1)]).unwrap();
        let region = MappedLinearRegion::new(
            Box::new(CompositeLinearRegion::new(
                vec![
                    CompositeLinearRegionChild::new(Box::new(first)),
                    CompositeLinearRegionChild::new(Box::new(second)),
                ],
                chunks.clone(),
                2,
            )),
            chunks,
        );

        region.flush(8, 4).unwrap();
        region.fence().unwrap();

        assert_eq!(*first_flushes.lock().unwrap(), vec![8..12]);
        assert!(second_flushes.lock().unwrap().is_empty());
        assert_eq!(first_fences.load(Ordering::Relaxed), 1);
        assert_eq!(second_fences.load(Ordering::Relaxed), 0);

        region.flush(BLOCK_SIZE + 8, 4).unwrap();
        region.fence().unwrap();

        assert_eq!(*second_flushes.lock().unwrap(), vec![8..12]);
        assert_eq!(first_fences.load(Ordering::Relaxed), 1);
        assert_eq!(second_fences.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn composite_linear_region_grows_into_later_child_from_partial_initial_exposure() {
        let full_chunks =
            ChunkList::from_chunks(vec![RegionChunk::new(0, 1), RegionChunk::new(1, 1)]).unwrap();
        let initial_chunks = payload_chunk_prefix(&full_chunks, 1).unwrap();
        let region_backend = CompositeLinearRegion::new(
            vec![
                CompositeLinearRegionChild::new(Box::new(VMemoryBlockRegion::new(1).unwrap())),
                CompositeLinearRegionChild::new(Box::new(VMemoryBlockRegion::new(1).unwrap())),
            ],
            full_chunks,
            1,
        );
        let mut region = MappedLinearRegion::new(Box::new(region_backend), initial_chunks);

        assert_eq!(region.logical_len(), BLOCK_SIZE);
        region.reserve_capacity(BLOCK_SIZE + 1).unwrap();
        assert_eq!(region.logical_len(), 2 * BLOCK_SIZE);

        region.write(BLOCK_SIZE + 8, &[5, 6, 7, 8]).unwrap();
        assert_eq!(region.read(BLOCK_SIZE + 8, 4).unwrap(), vec![5, 6, 7, 8]);
    }

    #[test]
    fn composite_linear_region_growth_preserves_gapped_backend_payload_layout() {
        let full_chunks =
            ChunkList::from_chunks(vec![RegionChunk::new(2, 1), RegionChunk::new(5, 1)]).unwrap();
        let initial_chunks = payload_chunk_prefix(&full_chunks, 1).unwrap();
        let region_backend = CompositeLinearRegion::new(
            vec![
                CompositeLinearRegionChild::new(Box::new(VMemoryBlockRegion::new(4).unwrap())),
                CompositeLinearRegionChild::new(Box::new(VMemoryBlockRegion::new(2).unwrap())),
            ],
            full_chunks,
            1,
        );
        let mut region = MappedLinearRegion::new(Box::new(region_backend), initial_chunks);

        region.write(0, &[1, 2, 3, 4]).unwrap();
        region.reserve_capacity(BLOCK_SIZE + 1).unwrap();
        region.write(BLOCK_SIZE + 8, &[5, 6, 7, 8]).unwrap();

        assert_eq!(region.read(0, 4).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(region.read(BLOCK_SIZE + 8, 4).unwrap(), vec![5, 6, 7, 8]);
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
    fn dax_pmem_region_reports_backend_line_mark_count() {
        let empty = TMemoryRegion::new_dax_pmem(0, TMemoryDaxPmemBacking::ResearchTemp).unwrap();
        assert_eq!(empty.line_mark_count_for_test(), 0);

        let one_byte = TMemoryRegion::new_dax_pmem(1, TMemoryDaxPmemBacking::ResearchTemp).unwrap();
        assert_eq!(
            one_byte.line_mark_count_for_test(),
            5 * (BLOCK_SIZE / IMMIX_LINE_SIZE)
        );
    }

    #[test]
    fn dax_pmem_region_flush_and_fence_paths_are_supported() {
        let mut region =
            TMemoryRegion::new_dax_pmem(BLOCK_SIZE, TMemoryDaxPmemBacking::ResearchTemp).unwrap();

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
        region.reserve_capacity(2 * BLOCK_SIZE).unwrap();

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
