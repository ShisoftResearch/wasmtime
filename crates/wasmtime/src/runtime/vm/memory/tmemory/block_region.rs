//! Wizard-style block/chunk region layout for transactional storage.

#![allow(dead_code)]

use super::{
    DATA_CHUNK_MAGIC, DataChunkClass, DataChunkHeader, DataRecordLocation, LOG_BLOCK_MAGIC,
    LogBlockHeader, NO_NEXT_BLOCK, SMALL_DATA_LIMIT, TxLogEntry,
};
use crate::prelude::*;
use crate::runtime::vm::SendSyncPtr;
use core::{
    mem::size_of,
    ops::Range,
    ptr::NonNull,
    sync::atomic::{Ordering, compiler_fence},
};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

pub(crate) const BLOCK_SIZE: usize = 512 * 1024;
pub(crate) const IMMIX_LINE_SIZE: usize = 256;
pub(crate) const PMEM_CACHE_LINE_SIZE: usize = 64;
static FILE_BACKED_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FileBackedRegionMode {
    Temp,
    Path(PathBuf),
}

#[derive(Debug)]
pub(crate) struct FileBackedMapping {
    file: File,
    path: PathBuf,
    unlink_on_drop: bool,
    memory: SendSyncPtr<[u8]>,
}

impl FileBackedMapping {
    pub(crate) fn new(mode: FileBackedRegionMode, len: usize) -> Result<Self> {
        match mode {
            FileBackedRegionMode::Temp => Self::new_temp(len),
            FileBackedRegionMode::Path(path) => Self::new_path(path, len),
        }
    }

    pub(crate) fn new_temp(len: usize) -> Result<Self> {
        new_temp_file_backed_mapping(len)
    }

    pub(crate) fn new_path(path: PathBuf, len: usize) -> Result<Self> {
        Self::new_path_with_unlink(path, len, false)
    }

    fn new_path_with_unlink(path: PathBuf, len: usize, unlink_on_drop: bool) -> Result<Self> {
        new_file_backed_mapping(path, len, unlink_on_drop)
    }

    pub(crate) fn len(&self) -> usize {
        self.memory.len()
    }

    pub(crate) fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        let end = offset
            .checked_add(len)
            .context("file-backed mapping read range overflow")?;
        ensure!(
            end <= self.len(),
            "file-backed mapping read range out of bounds"
        );
        let slice = unsafe { self.memory.as_ref() };
        Ok(slice[offset..end].to_vec())
    }

    pub(crate) fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        let end = offset
            .checked_add(bytes.len())
            .context("file-backed mapping write range overflow")?;
        ensure!(
            end <= self.len(),
            "file-backed mapping write range out of bounds"
        );
        let slice = unsafe { self.memory.as_mut() };
        slice[offset..end].copy_from_slice(bytes);
        Ok(())
    }

    pub(crate) fn flush(&self, offset: usize, len: usize) -> Result<()> {
        let end = offset
            .checked_add(len)
            .context("file-backed mapping flush range overflow")?;
        ensure!(
            end <= self.len(),
            "file-backed mapping flush range out of bounds"
        );
        if len == 0 {
            return Ok(());
        }
        flush_file_backed_mapping(self.memory, offset, len)
    }

    pub(crate) fn fence_data(&self) -> Result<()> {
        self.file
            .sync_data()
            .context("failed to sync_data file-backed tmemory")
    }

    pub(crate) fn fence_all(&self) -> Result<()> {
        self.file
            .sync_all()
            .context("failed to sync_all file-backed tmemory")
    }

    pub(crate) fn remap_len(&mut self, new_len: usize) -> Result<()> {
        #[cfg(unix)]
        {
            self.file
                .set_len(u64::try_from(new_len).context("file-backed tmemory length overflow")?)
                .with_context(|| {
                    format!(
                        "failed to resize file-backed tmemory file {}",
                        self.path.display()
                    )
                })?;
            let new_memory = unsafe { map_shared_file(&self.file, new_len)? };
            let old_memory = core::mem::replace(&mut self.memory, new_memory);
            unsafe { unmap_shared_file(old_memory)? };
            self.fence_all()
        }
        #[cfg(not(unix))]
        {
            let _ = new_len;
            bail!("FileBackedMemory remap is unsupported on this target")
        }
    }
}

impl Drop for FileBackedMapping {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            let _ = unmap_shared_file(self.memory);
        }
        if self.unlink_on_drop {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn unique_temp_file_path() -> PathBuf {
    let counter = FILE_BACKED_TEMP_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    std::env::temp_dir().join(format!(
        "wasmtime-transaction-tmemory-{}-{counter}.bin",
        std::process::id()
    ))
}

fn empty_mapping_memory() -> SendSyncPtr<[u8]> {
    SendSyncPtr::new(NonNull::slice_from_raw_parts(NonNull::<u8>::dangling(), 0))
}

#[cfg(unix)]
fn new_file_backed_mapping(
    path: PathBuf,
    len: usize,
    unlink_on_drop: bool,
) -> Result<FileBackedMapping> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .with_context(|| format!("failed to open file-backed tmemory file {}", path.display()))?;
    finish_file_backed_mapping(file, path, len, unlink_on_drop)
}

#[cfg(unix)]
fn new_temp_file_backed_mapping(len: usize) -> Result<FileBackedMapping> {
    const MAX_ATTEMPTS: usize = 16;

    for _ in 0..MAX_ATTEMPTS {
        let path = unique_temp_file_path();
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                let mut mapping = finish_file_backed_mapping(file, path, len, true)?;
                std::fs::remove_file(&mapping.path).with_context(|| {
                    format!(
                        "failed to unlink temp file-backed tmemory file {}",
                        mapping.path.display()
                    )
                })?;
                mapping.unlink_on_drop = false;
                return Ok(mapping);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to create temp file-backed tmemory file {}",
                        path.display()
                    )
                });
            }
        }
    }

    bail!("failed to create unique temp file-backed tmemory file after {MAX_ATTEMPTS} attempts")
}

#[cfg(unix)]
fn finish_file_backed_mapping(
    file: File,
    path: PathBuf,
    len: usize,
    unlink_on_drop: bool,
) -> Result<FileBackedMapping> {
    if let Err(error) = file
        .set_len(u64::try_from(len).context("file-backed tmemory length overflow")?)
        .with_context(|| {
            format!(
                "failed to resize file-backed tmemory file {}",
                path.display()
            )
        })
    {
        if unlink_on_drop {
            let _ = std::fs::remove_file(&path);
        }
        return Err(error);
    }
    let memory = match unsafe { map_shared_file(&file, len) } {
        Ok(memory) => memory,
        Err(error) => {
            if unlink_on_drop {
                let _ = std::fs::remove_file(&path);
            }
            return Err(error);
        }
    };
    Ok(FileBackedMapping {
        file,
        path,
        unlink_on_drop,
        memory,
    })
}

#[cfg(unix)]
unsafe fn map_shared_file(file: &File, len: usize) -> Result<SendSyncPtr<[u8]>> {
    if len == 0 {
        return Ok(empty_mapping_memory());
    }
    let ptr = unsafe {
        rustix::mm::mmap(
            core::ptr::null_mut(),
            len,
            rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
            rustix::mm::MapFlags::SHARED,
            file,
            0,
        )
        .context("failed to mmap shared file-backed tmemory region")?
    };
    let slice = core::ptr::slice_from_raw_parts_mut(ptr.cast::<u8>(), len);
    Ok(SendSyncPtr::new(NonNull::new(slice).unwrap()))
}

#[cfg(unix)]
unsafe fn unmap_shared_file(memory: SendSyncPtr<[u8]>) -> Result<()> {
    let len = memory.len();
    if len == 0 {
        return Ok(());
    }
    unsafe {
        rustix::mm::munmap(memory.as_non_null().cast::<u8>().as_ptr().cast(), len)
            .context("failed to munmap file-backed tmemory region")?;
    }
    Ok(())
}

#[cfg(unix)]
fn flush_file_backed_mapping(memory: SendSyncPtr<[u8]>, offset: usize, len: usize) -> Result<()> {
    let page_size = rustix::param::page_size();
    let (aligned_start, aligned_len) =
        file_backed_flush_range(memory.len(), offset, len, page_size)?;
    unsafe {
        rustix::mm::msync(
            memory
                .as_non_null()
                .cast::<u8>()
                .as_ptr()
                .add(aligned_start)
                .cast(),
            aligned_len,
            rustix::mm::MsyncFlags::SYNC,
        )
        .context("failed to msync file-backed tmemory range")?;
    }
    Ok(())
}

fn file_backed_flush_range(
    mapping_len: usize,
    offset: usize,
    len: usize,
    page_size: usize,
) -> Result<(usize, usize)> {
    ensure!(
        page_size > 0,
        "file-backed mapping page size must be nonzero"
    );
    let end = offset
        .checked_add(len)
        .context("file-backed mapping flush range overflow")?;
    ensure!(
        end <= mapping_len,
        "file-backed mapping flush range out of bounds"
    );
    if len == 0 {
        return Ok((offset, 0));
    }
    let aligned_start = (offset / page_size) * page_size;
    Ok((aligned_start, end - aligned_start))
}

#[cfg(not(unix))]
fn new_file_backed_mapping(
    _path: PathBuf,
    _len: usize,
    _unlink_on_drop: bool,
) -> Result<FileBackedMapping> {
    bail!(
        "FileBackedMemory is unsupported on this target until shared writable mmap support exists"
    )
}

#[cfg(not(unix))]
fn new_temp_file_backed_mapping(_len: usize) -> Result<FileBackedMapping> {
    bail!(
        "FileBackedMemory is unsupported on this target until shared writable mmap support exists"
    )
}

#[cfg(not(unix))]
fn flush_file_backed_mapping(
    _memory: SendSyncPtr<[u8]>,
    _offset: usize,
    _len: usize,
) -> Result<()> {
    bail!(
        "FileBackedMemory is unsupported on this target until shared writable mmap support exists"
    )
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

    // Keep ordinary stores ordered before the persistence flush loop. Rust
    // `asm!` is side-effecting unless marked `pure`/`nomem`/`readonly`; the
    // compiler fences make the intended PMEM publication boundary explicit.
    compiler_fence(Ordering::SeqCst);
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
    compiler_fence(Ordering::SeqCst);
    Ok(())
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn flush_clwb_range(_ptr: core::ptr::NonNull<u8>, _len: usize) -> Result<()> {
    bail!("NVMemory CLWB flush is unsupported on this target")
}

#[cfg(target_arch = "x86_64")]
unsafe fn sfence() -> Result<()> {
    compiler_fence(Ordering::SeqCst);
    unsafe {
        core::arch::asm!("sfence", options(nostack, preserves_flags));
    }
    compiler_fence(Ordering::SeqCst);
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
    fn line_mark_count(&self) -> usize;
    fn alloc_chunk(&mut self, block_count: usize) -> Result<RegionChunk>;
    fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>>;
    fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()>;
    fn flush(&self, offset: usize, len: usize) -> Result<()>;
    fn fence(&self) -> Result<()>;
    fn grow_to_blocks(&mut self, _new_block_count: usize) -> Result<Option<RegionChunk>> {
        bail!("transactional block region backend cannot grow")
    }
}

pub(crate) struct BlockRegionBackendView<'a> {
    backend: &'a dyn BlockRegionBackend,
}

impl<'a> BlockRegionBackendView<'a> {
    pub(crate) fn new(backend: &'a dyn BlockRegionBackend) -> Self {
        Self { backend }
    }

    pub(crate) fn block_size(&self) -> usize {
        self.backend.block_size()
    }

    pub(crate) fn num_blocks(&self) -> usize {
        self.backend.num_blocks()
    }

    pub(crate) fn block_magic(&self, start_block: u32) -> Result<u32> {
        let bytes = self.read_header_bytes(start_block, size_of::<u32>())?;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    }

    pub(crate) fn log_block_header(&self, start_block: u32) -> Result<LogBlockHeader> {
        LogBlockHeader::from_bytes(
            self.read_header_bytes(start_block, size_of::<LogBlockHeader>())?,
        )
    }

    pub(crate) fn data_chunk_header(&self, start_block: u32) -> Result<DataChunkHeader> {
        DataChunkHeader::from_bytes(
            self.read_header_bytes(start_block, size_of::<DataChunkHeader>())?,
        )
    }

    pub(crate) fn log_block_entries(&self, start_block: u32) -> Result<Vec<TxLogEntry>> {
        let header = self.log_block_header(start_block)?;
        ensure!(
            header.magic == LOG_BLOCK_MAGIC,
            "transactional log block {start_block} has invalid magic"
        );
        let entry_count = usize::try_from(header.entry_count)
            .context("transactional log block entry count overflow")?;
        ensure!(
            entry_count <= self.log_entry_capacity(),
            "transactional log block {start_block} entry count exceeds capacity"
        );

        let mut entries = Vec::with_capacity(entry_count);
        let mut offset = self
            .block_offset(start_block)?
            .checked_add(size_of::<LogBlockHeader>())
            .context("transactional log block entry area overflow")?;
        for _ in 0..entry_count {
            let bytes = self.backend.read(offset, size_of::<TxLogEntry>())?;
            entries.push(decode_tx_log_entry(bytes)?);
            offset = offset
                .checked_add(size_of::<TxLogEntry>())
                .context("transactional log block entry offset overflow")?;
        }
        Ok(entries)
    }

    fn log_entry_capacity(&self) -> usize {
        (self.block_size() - size_of::<LogBlockHeader>()) / size_of::<TxLogEntry>()
    }

    fn block_offset(&self, block: u32) -> Result<usize> {
        let block = usize::try_from(block).context("transactional block index overflow")?;
        ensure!(
            block < self.num_blocks(),
            "transactional block index out of bounds"
        );
        block
            .checked_mul(self.block_size())
            .context("transactional block offset overflow")
    }

    fn read_header_bytes(&self, start_block: u32, len: usize) -> Result<Vec<u8>> {
        self.backend.read(self.block_offset(start_block)?, len)
    }
}

fn decode_tx_log_entry(bytes: impl AsRef<[u8]>) -> Result<TxLogEntry> {
    let bytes = bytes.as_ref();
    ensure!(
        bytes.len() == size_of::<TxLogEntry>(),
        "durable tx log entry length mismatch"
    );
    Ok(TxLogEntry {
        logical_id: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
        version: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
        tx_meta: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
        data_block: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
        data_offset: u32::from_le_bytes(bytes[20..24].try_into().unwrap()),
        crc32: u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
        reserved: u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
    })
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
    streams: BTreeMap<u32, StreamState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StreamCursor {
    stream_id: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct StreamState {
    next_data_chunk_seq: u32,
    current_data_chunk_start: Option<u32>,
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
            streams: BTreeMap::new(),
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(num_blocks: usize) -> Result<Self> {
        Self::new(num_blocks)
    }

    pub(crate) fn view(&self) -> BlockRegionBackendView<'_> {
        BlockRegionBackendView::new(self)
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

    pub(crate) fn alloc_log_block(&mut self, stream_id: u32, block_seq: u32) -> Result<u32> {
        let chunk = self.alloc_chunk(1)?;
        let start_block = u32::try_from(chunk.start_block())
            .context("transactional log block start block overflow")?;
        let header = LogBlockHeader {
            magic: LOG_BLOCK_MAGIC,
            stream_id,
            block_seq,
            next_block: NO_NEXT_BLOCK,
            entry_count: 0,
        };
        self.write_log_block_header(start_block, header)?;
        Ok(start_block)
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

    pub(crate) fn alloc_stream(&mut self, stream_id: u32) -> Result<StreamCursor> {
        ensure!(
            !self.streams.contains_key(&stream_id),
            "transactional stream {stream_id} already exists"
        );
        self.streams.insert(stream_id, StreamState::default());
        Ok(StreamCursor { stream_id })
    }

    pub(crate) fn append_data_record(
        &mut self,
        stream: StreamCursor,
        bytes: &[u8],
    ) -> Result<DataRecordLocation> {
        let mut state = *self.streams.get(&stream.stream_id).with_context(|| {
            format!("transactional stream {} is not allocated", stream.stream_id)
        })?;

        let chunk_start_block = if let Some(start_block) = state.current_data_chunk_start {
            let header = self.data_chunk_header(start_block)?;
            if self.chunk_remaining_capacity(header)? >= bytes.len() {
                start_block
            } else {
                let next_chunk_start = self.alloc_data_chunk(
                    stream.stream_id,
                    state.next_data_chunk_seq,
                    bytes.len(),
                )?;
                let mut previous = header;
                previous.next_chunk = next_chunk_start;
                self.write_data_chunk_header(start_block, previous)?;
                state.next_data_chunk_seq = state
                    .next_data_chunk_seq
                    .checked_add(1)
                    .context("transactional data chunk sequence overflow")?;
                state.current_data_chunk_start = Some(next_chunk_start);
                next_chunk_start
            }
        } else {
            let start_block =
                self.alloc_data_chunk(stream.stream_id, state.next_data_chunk_seq, bytes.len())?;
            state.next_data_chunk_seq = state
                .next_data_chunk_seq
                .checked_add(1)
                .context("transactional data chunk sequence overflow")?;
            state.current_data_chunk_start = Some(start_block);
            start_block
        };

        let mut header = self.data_chunk_header(chunk_start_block)?;
        let record_start = self.chunk_tail_offset(header)?;
        let record_end = record_start
            .checked_add(bytes.len())
            .context("transactional data record length overflow")?;
        ensure!(
            record_end <= self.chunk_capacity_bytes(header)?,
            "transactional data record crosses chunk boundary"
        );
        let write_offset = self
            .block_offset(chunk_start_block)?
            .checked_add(record_start)
            .context("transactional data record write offset overflow")?;
        self.write(write_offset, bytes)?;
        header.tail_block_delta = u32::try_from(record_end / BLOCK_SIZE)
            .context("transactional data chunk tail block delta overflow")?;
        header.tail_in_block = u32::try_from(record_end % BLOCK_SIZE)
            .context("transactional data chunk tail offset overflow")?;
        self.write_data_chunk_header(chunk_start_block, header)?;
        self.streams.insert(stream.stream_id, state);

        let data_block = chunk_start_block
            .checked_add(
                u32::try_from(record_start / BLOCK_SIZE)
                    .context("transactional data record block delta overflow")?,
            )
            .context("transactional data record block index overflow")?;
        let data_offset = u32::try_from(record_start % BLOCK_SIZE)
            .context("transactional data record offset overflow")?;
        Ok(DataRecordLocation {
            chunk_start_block,
            data_block,
            data_offset,
        })
    }

    pub(crate) fn log_block_header(&self, start_block: u32) -> Result<LogBlockHeader> {
        LogBlockHeader::from_bytes(
            self.read_header_bytes(start_block, size_of::<LogBlockHeader>())?,
        )
    }

    pub(crate) fn data_chunk_header(&self, start_block: u32) -> Result<DataChunkHeader> {
        DataChunkHeader::from_bytes(
            self.read_header_bytes(start_block, size_of::<DataChunkHeader>())?,
        )
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

    fn alloc_data_chunk_for_class(
        &mut self,
        stream_id: u32,
        chunk_seq: u32,
        class: DataChunkClass,
        total_bytes: usize,
    ) -> Result<u32> {
        let chunk_blocks = match class {
            DataChunkClass::Small | DataChunkClass::Medium => 1,
            DataChunkClass::Large => self.required_data_chunk_blocks(total_bytes)?,
        };
        let chunk = self.alloc_chunk(chunk_blocks)?;
        let start_block = u32::try_from(chunk.start_block())
            .context("transactional data chunk start block overflow")?;
        let header = DataChunkHeader {
            magic: DATA_CHUNK_MAGIC,
            stream_id,
            chunk_seq,
            next_chunk: NO_NEXT_BLOCK,
            chunk_blocks: u32::try_from(chunk_blocks)
                .context("transactional data chunk block count overflow")?,
            tail_block_delta: 0,
            tail_in_block: u32::try_from(size_of::<DataChunkHeader>())
                .context("transactional data chunk header size overflow")?,
        };
        self.write_data_chunk_header(start_block, header)?;
        Ok(start_block)
    }

    fn block_payload_capacity(&self) -> usize {
        BLOCK_SIZE - size_of::<DataChunkHeader>()
    }

    fn required_data_chunk_blocks(&self, total_bytes: usize) -> Result<usize> {
        let first_block_capacity = self.block_payload_capacity();
        if total_bytes <= first_block_capacity {
            return Ok(1);
        }
        let extra_bytes = total_bytes - first_block_capacity;
        1usize
            .checked_add(extra_bytes.div_ceil(BLOCK_SIZE))
            .context("transactional data chunk block count overflow")
    }

    fn chunk_tail_offset(&self, header: DataChunkHeader) -> Result<usize> {
        let block_delta = usize::try_from(header.tail_block_delta)
            .context("transactional data chunk tail block delta conversion overflow")?;
        let in_block = usize::try_from(header.tail_in_block)
            .context("transactional data chunk tail offset conversion overflow")?;
        block_delta
            .checked_mul(BLOCK_SIZE)
            .and_then(|offset| offset.checked_add(in_block))
            .context("transactional data chunk tail overflow")
    }

    fn chunk_capacity_bytes(&self, header: DataChunkHeader) -> Result<usize> {
        usize::try_from(header.chunk_blocks)
            .context("transactional data chunk block count conversion overflow")?
            .checked_mul(BLOCK_SIZE)
            .context("transactional data chunk capacity overflow")
    }

    fn chunk_remaining_capacity(&self, header: DataChunkHeader) -> Result<usize> {
        let capacity = self.chunk_capacity_bytes(header)?;
        let tail = self.chunk_tail_offset(header)?;
        ensure!(
            tail <= capacity,
            "transactional data chunk tail exceeds chunk capacity"
        );
        Ok(capacity - tail)
    }

    fn block_offset(&self, block: u32) -> Result<usize> {
        let block = usize::try_from(block).context("transactional block index overflow")?;
        ensure!(
            block < self.num_blocks(),
            "transactional block index out of bounds"
        );
        block
            .checked_mul(BLOCK_SIZE)
            .context("transactional block offset overflow")
    }

    fn read_header_bytes(&self, start_block: u32, len: usize) -> Result<Vec<u8>> {
        self.read(self.block_offset(start_block)?, len)
    }

    fn write_log_block_header(&mut self, start_block: u32, header: LogBlockHeader) -> Result<()> {
        self.write(self.block_offset(start_block)?, &header.as_bytes())
    }

    fn write_data_chunk_header(&mut self, start_block: u32, header: DataChunkHeader) -> Result<()> {
        self.write(self.block_offset(start_block)?, &header.as_bytes())
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

    fn line_mark_count(&self) -> usize {
        self.line_count()
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

    pub(crate) fn view(&self) -> BlockRegionBackendView<'_> {
        BlockRegionBackendView::new(self)
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

    fn line_mark_count(&self) -> usize {
        self.line_count()
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
pub(crate) struct FileBackedMemoryBlockRegion {
    mapping: FileBackedMapping,
    block_entries: Vec<BlockEntry>,
    line_marks: Vec<LineMark>,
}

impl FileBackedMemoryBlockRegion {
    pub(crate) fn new(num_blocks: usize, mode: FileBackedRegionMode) -> Result<Self> {
        let bytes_len = num_blocks
            .checked_mul(BLOCK_SIZE)
            .context("transactional FileBackedMemory block region size overflow")?;
        let line_count = bytes_len / IMMIX_LINE_SIZE;
        Ok(Self {
            mapping: FileBackedMapping::new(mode, bytes_len)?,
            block_entries: vec![BlockEntry::default(); num_blocks],
            line_marks: vec![
                LineMark {
                    mark: IMMIX_LINE_MARK_RESET_VALUE,
                };
                line_count
            ],
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(num_blocks: usize, mode: FileBackedRegionMode) -> Result<Self> {
        Self::new(num_blocks, mode)
    }

    pub(crate) fn view(&self) -> BlockRegionBackendView<'_> {
        BlockRegionBackendView::new(self)
    }

    pub(crate) fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    pub(crate) fn num_blocks(&self) -> usize {
        self.block_entries.len()
    }

    pub(crate) fn bytes_len(&self) -> usize {
        self.mapping.len()
    }

    pub(crate) fn line_count(&self) -> usize {
        self.line_marks.len()
    }

    pub(crate) fn alloc_chunk(&mut self, block_count: usize) -> Result<RegionChunk> {
        ensure!(
            block_count > 0,
            "transactional FileBackedMemory chunk must contain a block"
        );
        ensure!(
            block_count <= self.num_blocks(),
            "transactional FileBackedMemory chunk exceeds region size"
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

        bail!("transactional FileBackedMemory block region is out of contiguous chunks")
    }

    pub(crate) fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        self.mapping.read(offset, len)
    }

    pub(crate) fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        self.mapping.write(offset, bytes)
    }

    pub(crate) fn flush(&self, offset: usize, len: usize) -> Result<()> {
        let end = offset
            .checked_add(len)
            .context("transactional FileBackedMemory block flush range overflow")?;
        ensure!(
            end <= self.mapping.len(),
            "transactional FileBackedMemory block flush range out of bounds"
        );
        self.mapping.flush(offset, len)
    }

    pub(crate) fn fence(&self) -> Result<()> {
        self.mapping.fence_data()
    }

    pub(crate) fn grow_to_blocks(&mut self, new_block_count: usize) -> Result<Option<RegionChunk>> {
        let old_block_count = self.num_blocks();
        ensure!(
            new_block_count >= old_block_count,
            "transactional FileBackedMemory block region cannot shrink"
        );
        if new_block_count == old_block_count {
            return Ok(None);
        }

        let bytes_len = new_block_count
            .checked_mul(BLOCK_SIZE)
            .context("transactional FileBackedMemory block region size overflow")?;
        self.mapping.remap_len(bytes_len)?;

        let additional_blocks = new_block_count - old_block_count;
        self.block_entries
            .resize(new_block_count, BlockEntry::default());
        for entry in &mut self.block_entries[old_block_count..new_block_count] {
            entry.used = 1;
            entry.list_num = ListKind::Used as i16;
        }

        let line_count = bytes_len / IMMIX_LINE_SIZE;
        self.line_marks.resize(
            line_count,
            LineMark {
                mark: IMMIX_LINE_MARK_RESET_VALUE,
            },
        );

        Ok(Some(RegionChunk {
            start_block: old_block_count,
            block_count: additional_blocks,
        }))
    }
}

impl BlockRegionBackend for FileBackedMemoryBlockRegion {
    fn block_size(&self) -> usize {
        self.block_size()
    }

    fn num_blocks(&self) -> usize {
        self.num_blocks()
    }

    fn bytes_len(&self) -> usize {
        self.bytes_len()
    }

    fn line_mark_count(&self) -> usize {
        self.line_count()
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

    fn grow_to_blocks(&mut self, new_block_count: usize) -> Result<Option<RegionChunk>> {
        self.grow_to_blocks(new_block_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::vm::memory::tmemory::DataChunkHeader;
    use core::mem::size_of;
    use std::sync::{Mutex, OnceLock};

    const SMALL_DATA_RECORD_BYTES: usize = IMMIX_LINE_SIZE;
    const MEDIUM_DATA_RECORD_BYTES: usize = BLOCK_SIZE - size_of::<DataChunkHeader>();
    const LARGE_DATA_RECORD_BYTES: usize = MEDIUM_DATA_RECORD_BYTES + 1;

    fn file_backed_temp_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

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
        let small = region
            .alloc_data_chunk(7, 0, SMALL_DATA_RECORD_BYTES)
            .unwrap();
        let medium = region
            .alloc_data_chunk(7, 1, MEDIUM_DATA_RECORD_BYTES)
            .unwrap();
        let large = region
            .alloc_data_chunk(7, 2, LARGE_DATA_RECORD_BYTES)
            .unwrap();

        assert_eq!(region.data_chunk_header(small).unwrap().chunk_blocks, 1);
        assert_eq!(region.data_chunk_header(medium).unwrap().chunk_blocks, 1);
        assert!(region.data_chunk_header(large).unwrap().chunk_blocks > 1);
    }

    #[test]
    fn data_record_never_crosses_chunk_boundary() {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(9).unwrap();
        let first = region
            .append_data_record(stream, &vec![0u8; MEDIUM_DATA_RECORD_BYTES])
            .unwrap();
        let second = region
            .append_data_record(stream, &vec![0u8; MEDIUM_DATA_RECORD_BYTES])
            .unwrap();
        assert_ne!(first.chunk_start_block, second.chunk_start_block);
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
        engine
            .flush(core::ptr::NonNull::<u8>::dangling(), 0)
            .unwrap();
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

    #[cfg(unix)]
    #[test]
    fn file_backed_block_region_allocates_and_flushes_writes() {
        let mut region =
            FileBackedMemoryBlockRegion::new_for_test(2, FileBackedRegionMode::Temp).unwrap();
        let chunk = region.alloc_chunk(1).unwrap();
        let range = chunk.byte_range();

        region.write(range.start + 8, &[5, 6, 7, 8]).unwrap();
        region.flush(range.start + 8, 4).unwrap();
        region.fence().unwrap();

        assert_eq!(region.read(range.start + 8, 4).unwrap(), vec![5, 6, 7, 8]);
        assert_eq!(region.block_size(), BLOCK_SIZE);
        assert_eq!(region.num_blocks(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_block_region_rejects_out_of_bounds_flush() {
        let region =
            FileBackedMemoryBlockRegion::new_for_test(1, FileBackedRegionMode::Temp).unwrap();

        let error = region.flush(BLOCK_SIZE, 1).unwrap_err().to_string();
        assert!(error.contains("flush range out of bounds"));
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_block_region_grows_existing_mapping_and_preserves_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grow-block-region.tmemory");
        let mut region =
            FileBackedMemoryBlockRegion::new_for_test(1, FileBackedRegionMode::Path(path.clone()))
                .unwrap();
        let chunk = region.alloc_chunk(1).unwrap();
        let range = chunk.byte_range();

        region.write(range.start + 16, &[1, 2, 3, 4]).unwrap();
        region.flush(range.start + 16, 4).unwrap();
        region.fence().unwrap();

        let grown = region.grow_to_blocks(2).unwrap().unwrap();

        assert_eq!(grown.byte_range(), BLOCK_SIZE..(2 * BLOCK_SIZE));
        assert_eq!(region.num_blocks(), 2);
        assert_eq!(region.bytes_len(), 2 * BLOCK_SIZE);
        assert_eq!(region.read(range.start + 16, 4).unwrap(), vec![1, 2, 3, 4]);

        region
            .write(grown.byte_range().start, &[5, 6, 7, 8])
            .unwrap();
        region.flush(grown.byte_range().start, 4).unwrap();
        region.fence().unwrap();

        let bytes = std::fs::read(path).unwrap();
        assert_eq!(&bytes[range.start + 16..range.start + 20], &[1, 2, 3, 4]);
        assert_eq!(
            &bytes[grown.byte_range().start..grown.byte_range().start + 4],
            &[5, 6, 7, 8]
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_shared_mapping_writes_through_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mapping.tmemory");
        let mut mapping = FileBackedMapping::new_path(path.clone(), 4096).unwrap();

        mapping.write(16, &[1, 2, 3, 4]).unwrap();
        mapping.flush(16, 4).unwrap();
        mapping.fence_data().unwrap();

        let bytes = std::fs::read(path).unwrap();
        assert_eq!(&bytes[16..20], &[1, 2, 3, 4]);
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_shared_mapping_rejects_out_of_bounds_flush() {
        let _guard = file_backed_temp_test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mapping = FileBackedMapping::new_temp(4096).unwrap();

        let error = mapping.flush(4096, 1).unwrap_err().to_string();
        assert!(error.contains("file-backed mapping flush range out of bounds"));
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_shared_mapping_remap_preserves_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remap.tmemory");
        let mut mapping = FileBackedMapping::new_path(path.clone(), 4096).unwrap();

        mapping.write(32, &[9, 8, 7, 6]).unwrap();
        mapping.flush(32, 4).unwrap();
        mapping.fence_data().unwrap();
        mapping.remap_len(8192).unwrap();

        assert_eq!(mapping.read(32, 4).unwrap(), vec![9, 8, 7, 6]);
        assert_eq!(std::fs::metadata(path).unwrap().len(), 8192);
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_shared_mapping_tail_flush_range_stays_within_mapping() {
        let page_size = rustix::param::page_size();
        let (aligned_start, aligned_len) =
            file_backed_flush_range(4097, 4096, 1, page_size).unwrap();

        assert_eq!(aligned_start, 4096);
        assert_eq!(aligned_len, 1);
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_shared_mapping_flushes_tail_of_non_page_aligned_len() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tail-flush.tmemory");
        let mut mapping = FileBackedMapping::new_path(path.clone(), 4097).unwrap();

        mapping.write(4096, &[0x5a]).unwrap();
        mapping.flush(4096, 1).unwrap();
        mapping.fence_data().unwrap();

        let bytes = std::fs::read(path).unwrap();
        assert_eq!(bytes.len(), 4097);
        assert_eq!(bytes[4096], 0x5a);
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_shared_mapping_zero_len_supports_empty_operations() {
        let _guard = file_backed_temp_test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut mapping = FileBackedMapping::new_temp(0).unwrap();

        assert_eq!(mapping.len(), 0);
        assert_eq!(mapping.read(0, 0).unwrap(), Vec::<u8>::new());
        mapping.write(0, &[]).unwrap();
        mapping.flush(0, 0).unwrap();
        mapping.fence_data().unwrap();
        mapping.fence_all().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_shared_mapping_temp_mode_does_not_clobber_existing_file() {
        let _guard = file_backed_temp_test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let counter = FILE_BACKED_TEMP_COUNTER.load(AtomicOrdering::Relaxed);
        let stale_path = std::env::temp_dir().join(format!(
            "wasmtime-transaction-tmemory-{}-{counter}.bin",
            std::process::id()
        ));
        std::fs::write(&stale_path, b"stale-bytes").unwrap();

        let mapping = FileBackedMapping::new_temp(16).unwrap();

        assert_ne!(mapping.path, stale_path);
        assert_eq!(std::fs::read(&stale_path).unwrap(), b"stale-bytes");

        std::fs::remove_file(stale_path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_shared_mapping_temp_mode_unlinks_file_immediately() {
        let _guard = file_backed_temp_test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut mapping = FileBackedMapping::new_temp(16).unwrap();

        assert!(!mapping.path.exists());

        mapping.write(0, &[1, 2, 3, 4]).unwrap();
        mapping.flush(0, 4).unwrap();
        mapping.fence_data().unwrap();
        assert_eq!(mapping.read(0, 4).unwrap(), vec![1, 2, 3, 4]);
    }

    #[cfg(not(unix))]
    #[test]
    fn file_backed_shared_mapping_is_unsupported_on_non_unix() {
        let error = FileBackedMapping::new_temp(4096).unwrap_err().to_string();
        assert!(error.contains("unsupported"));
    }
}
