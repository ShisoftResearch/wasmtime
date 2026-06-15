//! Wizard-style block/chunk region layout for transactional storage.

#![allow(dead_code)]

use super::{
    DATA_CHUNK_MAGIC, DataChunkClass, DataChunkHeader, DataRecordLocation, LOG_BLOCK_MAGIC,
    LogBlockHeader, NO_NEXT_BLOCK, PackedGranuleDomain, REGION_MAGIC, RegionHeader,
    SMALL_DATA_LIMIT, TMemory, TxLogEntry, TxLogEntryRole,
    durable_log::{
        BlockKind, BlockMeta, BlockState, TxDataRecordHeader, TxDataRecordRole, TxEntryMeta,
    },
    metadata::{
        BLOCK_TABLE_START_BLOCK, METADATA_DESC_BLOCK_COUNT, REGION_HEADER_BLOCK,
        TYPE_LAYOUT_META_KIND, TYPE_LAYOUT_METADATA_BLOCK_COUNT, append_type_layout_to_block,
        block_table_block_count, empty_type_layout_metadata_block,
        load_type_layout_registry_from_block, metadata_desc_start_block, reserved_metadata_blocks,
        type_layout_metadata_start_block,
    },
    pack_object_granule_id,
};
use crate::prelude::*;
use crate::runtime::transaction::type_layout::{
    PersistentTypeLayout, StructTraceField, TraceSlotKind, TypeLayoutId, TypeLayoutRegistry,
};
use crate::runtime::transaction::{
    ObjectKind, ObjectPayload, ObjectValue, PersistentRecoveredRecordLocation,
    encode_object_record_for_recovery,
};
use crate::runtime::vm::SendSyncPtr;
use core::{
    mem::size_of,
    ops::Range,
    ptr::NonNull,
    sync::atomic::{Ordering, compiler_fence},
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
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
    OpenExistingPath(PathBuf),
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
            FileBackedRegionMode::OpenExistingPath(path) => {
                Self::open_existing_path_with_len(path, len)
            }
        }
    }

    pub(crate) fn new_temp(len: usize) -> Result<Self> {
        new_temp_file_backed_mapping(len)
    }

    pub(crate) fn new_path(path: PathBuf, len: usize) -> Result<Self> {
        Self::new_path_with_unlink(path, len, false)
    }

    pub(crate) fn open_existing_path(path: PathBuf) -> Result<Self> {
        open_existing_file_backed_mapping(path)
    }

    fn open_existing_path_with_len(path: PathBuf, len: usize) -> Result<Self> {
        let mapping = Self::open_existing_path(path)?;
        ensure!(
            mapping.len() >= len,
            "existing file-backed tmemory is smaller than requested capacity: expected at least {len} bytes, found {}",
            mapping.len()
        );
        Ok(mapping)
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
fn open_existing_file_backed_mapping(path: PathBuf) -> Result<FileBackedMapping> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| {
            format!(
                "failed to open existing file-backed tmemory file {}",
                path.display()
            )
        })?;
    let len = usize::try_from(
        file.metadata()
            .with_context(|| format!("failed to stat file-backed tmemory file {}", path.display()))?
            .len(),
    )
    .context("file-backed tmemory file length overflow")?;
    let memory = unsafe { map_shared_file(&file, len)? };
    Ok(FileBackedMapping {
        file,
        path,
        unlink_on_drop: false,
        memory,
    })
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
fn open_existing_file_backed_mapping(_path: PathBuf) -> Result<FileBackedMapping> {
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

impl MetaDataDesc {
    const BYTE_LEN: usize = 32;

    pub(crate) fn as_bytes(&self) -> [u8; Self::BYTE_LEN] {
        let mut bytes = [0u8; Self::BYTE_LEN];
        bytes[0..8].copy_from_slice(&self.kind.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.fixed_bytes.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.unit_size.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.bytes_per_unit.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.padding.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.offset.to_le_bytes());
        bytes
    }

    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() == Self::BYTE_LEN,
            "transactional metadata descriptor length mismatch"
        );
        let desc = Self {
            kind: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            fixed_bytes: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            unit_size: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
            bytes_per_unit: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            padding: u32::from_le_bytes(bytes[20..24].try_into().unwrap()),
            offset: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
        };
        ensure!(
            desc.padding == 0,
            "transactional metadata descriptor padding must be zero"
        );
        Ok(desc)
    }
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
    fn block_meta(&self, _block: u32) -> Result<BlockMeta> {
        bail!("transactional block region backend does not expose block metadata")
    }
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

    pub(crate) fn block_meta(&self, block: u32) -> Result<BlockMeta> {
        self.backend.block_meta(block)
    }

    pub(crate) fn block_generation(&self, block: u32) -> Result<u32> {
        Ok(self.block_meta(block)?.generation)
    }

    pub(crate) fn valid_data_chunk_header(
        &self,
        start_block: u32,
    ) -> Result<Option<DataChunkHeader>> {
        let header = self.data_chunk_header(start_block)?;
        Ok((header.magic == DATA_CHUNK_MAGIC).then_some(header))
    }

    pub(crate) fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        self.backend.read(offset, len)
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
        entry_meta: u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
    })
}

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

fn next_chunk_generation(generation: u32) -> Result<u32> {
    ensure!(
        generation < TxEntryMeta::MAX_DATA_BLOCK_GENERATION,
        "durable block generation exhausted for chunk reuse"
    );
    Ok(generation + 1)
}

fn reusable_chunk_generation(
    block_metas: &[BlockMeta],
    start: usize,
    block_count: usize,
) -> Result<Option<u32>> {
    let end = start
        .checked_add(block_count)
        .context("transactional chunk metadata range overflow")?;
    let chunk_start = u32::try_from(start).context("transactional chunk start block overflow")?;
    let chunk_blocks =
        u32::try_from(block_count).context("transactional chunk block count overflow")?;
    let mut chunk_state = None;
    let mut generation = None;

    for meta in &block_metas[start..end] {
        let state = meta.state()?;
        match state {
            BlockState::Free | BlockState::Retired => {}
            BlockState::Active | BlockState::Sealed => return Ok(None),
        }
        if let Some(expected_state) = chunk_state {
            if expected_state != state {
                return Ok(None);
            }
        } else {
            chunk_state = Some(state);
        }

        let block_generation = match state {
            BlockState::Free => meta.generation,
            BlockState::Retired => {
                if meta.chunk_start != chunk_start || meta.chunk_blocks != chunk_blocks {
                    return Ok(None);
                }
                next_chunk_generation(meta.generation)?
            }
            BlockState::Active | BlockState::Sealed => unreachable!(),
        };
        if let Some(expected_generation) = generation {
            if expected_generation != block_generation {
                return Ok(None);
            }
        } else {
            generation = Some(block_generation);
        }
    }

    Ok(generation)
}

fn data_record_block_range(
    data_block: u32,
    data_offset: u32,
    record_len: u64,
) -> Result<Range<usize>> {
    ensure!(
        record_len > 0,
        "transactional data record length must be nonzero"
    );

    let start_block =
        usize::try_from(data_block).context("transactional data record block overflow")?;
    let start_offset =
        usize::try_from(data_offset).context("transactional data record offset overflow")?;
    ensure!(
        start_offset < BLOCK_SIZE,
        "transactional data record offset exceeds block size"
    );
    let record_len =
        usize::try_from(record_len).context("transactional data record length overflow")?;
    let end_offset = start_offset
        .checked_add(record_len)
        .context("transactional data record length overflow")?;
    let covered_blocks = end_offset
        .checked_sub(1)
        .context("transactional data record length must be nonzero")?
        / BLOCK_SIZE;
    let end_block = start_block
        .checked_add(covered_blocks)
        .and_then(|last_block| last_block.checked_add(1))
        .context("transactional data record block range overflow")?;
    Ok(start_block..end_block)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RegionChunk {
    start_block: usize,
    block_count: usize,
}

impl RegionChunk {
    pub(crate) fn new(start_block: usize, block_count: usize) -> Self {
        Self {
            start_block,
            block_count,
        }
    }

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
    block_metas: Vec<BlockMeta>,
    line_marks: Vec<LineMark>,
    streams: BTreeMap<u32, StreamState>,
    metadata_initialized: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StreamCursor {
    stream_id: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct StreamState {
    next_log_block_seq: u32,
    current_log_block_start: Option<u32>,
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
            block_metas: vec![BlockMeta::free(); num_blocks],
            line_marks: vec![
                LineMark {
                    mark: IMMIX_LINE_MARK_RESET_VALUE,
                };
                line_count
            ],
            streams: BTreeMap::new(),
            metadata_initialized: false,
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
        let chunk = self.alloc_chunk_for_kind(1, BlockKind::Log, stream_id)?;
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
        self.alloc_data_chunk_for_kind(stream_id, chunk_seq, BlockKind::ObjectData, total_bytes)
    }

    fn alloc_data_chunk_for_kind(
        &mut self,
        stream_id: u32,
        chunk_seq: u32,
        kind: BlockKind,
        total_bytes: usize,
    ) -> Result<u32> {
        let class = if total_bytes <= SMALL_DATA_LIMIT {
            DataChunkClass::Small
        } else if total_bytes <= self.block_payload_capacity() {
            DataChunkClass::Medium
        } else {
            DataChunkClass::Large
        };
        self.alloc_data_chunk_for_class(stream_id, chunk_seq, class, kind, total_bytes)
    }

    pub(crate) fn alloc_stream(&mut self, stream_id: u32) -> Result<StreamCursor> {
        ensure!(
            !self.streams.contains_key(&stream_id),
            "transactional stream {stream_id} already exists"
        );
        self.streams.insert(stream_id, StreamState::default());
        Ok(StreamCursor { stream_id })
    }

    pub(crate) fn stream_cursor(&mut self, stream_id: u32) -> StreamCursor {
        self.streams.entry(stream_id).or_default();
        StreamCursor { stream_id }
    }

    pub(crate) fn append_data_record(
        &mut self,
        stream: StreamCursor,
        bytes: &[u8],
    ) -> Result<DataRecordLocation> {
        let kind = block_kind_for_data_record(bytes)?;
        let mut state = *self.streams.get(&stream.stream_id).with_context(|| {
            format!("transactional stream {} is not allocated", stream.stream_id)
        })?;

        let chunk_start_block = if let Some(start_block) = state.current_data_chunk_start {
            let header = self.data_chunk_header(start_block)?;
            let current_kind = self.block_meta(start_block)?.kind()?;
            if current_kind == kind && self.chunk_remaining_capacity(header)? >= bytes.len() {
                start_block
            } else {
                let next_chunk_start = self.alloc_data_chunk_for_kind(
                    stream.stream_id,
                    state.next_data_chunk_seq,
                    kind,
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
            let start_block = self.alloc_data_chunk_for_kind(
                stream.stream_id,
                state.next_data_chunk_seq,
                kind,
                bytes.len(),
            )?;
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

    pub(crate) fn append_log_entry(
        &mut self,
        stream: StreamCursor,
        entry: TxLogEntry,
    ) -> Result<u32> {
        let mut state = *self.streams.get(&stream.stream_id).with_context(|| {
            format!("transactional stream {} is not allocated", stream.stream_id)
        })?;

        let log_block = if let Some(start_block) = state.current_log_block_start {
            let header = self.log_block_header(start_block)?;
            if usize::try_from(header.entry_count)
                .context("transactional log entry count overflow")?
                < self.log_entry_capacity()
            {
                start_block
            } else {
                let next_block =
                    self.alloc_log_block(stream.stream_id, state.next_log_block_seq)?;
                let mut previous = header;
                previous.next_block = next_block;
                self.write_log_block_header(start_block, previous)?;
                state.next_log_block_seq = state
                    .next_log_block_seq
                    .checked_add(1)
                    .context("transactional log block sequence overflow")?;
                state.current_log_block_start = Some(next_block);
                next_block
            }
        } else {
            let start_block = self.alloc_log_block(stream.stream_id, state.next_log_block_seq)?;
            state.next_log_block_seq = state
                .next_log_block_seq
                .checked_add(1)
                .context("transactional log block sequence overflow")?;
            state.current_log_block_start = Some(start_block);
            start_block
        };

        let mut header = self.log_block_header(log_block)?;
        let entry_index = usize::try_from(header.entry_count)
            .context("transactional log entry count overflow")?;
        ensure!(
            entry_index < self.log_entry_capacity(),
            "transactional log block is full"
        );
        let write_offset = self
            .block_offset(log_block)?
            .checked_add(size_of::<LogBlockHeader>())
            .and_then(|offset| offset.checked_add(entry_index * size_of::<TxLogEntry>()))
            .context("transactional log entry write offset overflow")?;
        self.write(write_offset, &encode_tx_log_entry(entry))?;
        header.entry_count = header
            .entry_count
            .checked_add(1)
            .context("transactional log entry count overflow")?;
        self.write_log_block_header(log_block, header)?;
        self.streams.insert(stream.stream_id, state);

        Ok(log_block)
    }

    pub(crate) fn flush_data_chunk(&self, chunk_start_block: u32) -> Result<()> {
        let header = self.data_chunk_header(chunk_start_block)?;
        let chunk_blocks = usize::try_from(header.chunk_blocks)
            .context("transactional data chunk block count overflow")?;
        let flush_len = chunk_blocks
            .checked_mul(BLOCK_SIZE)
            .context("transactional data chunk flush length overflow")?;
        self.flush(self.block_offset(chunk_start_block)?, flush_len)
    }

    pub(crate) fn flush_log_block(&self, log_block: u32) -> Result<()> {
        self.flush(self.block_offset(log_block)?, BLOCK_SIZE)
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

    pub(crate) fn load_type_layout_metadata(&self) -> Result<TypeLayoutRegistry> {
        if !self.metadata_initialized {
            return Ok(TypeLayoutRegistry::default());
        }
        let desc = self.validated_type_layout_metadata_desc()?;
        let offset =
            usize::try_from(desc.offset).context("transactional metadata offset overflow")?;
        let len = usize::try_from(TYPE_LAYOUT_METADATA_BLOCK_COUNT)
            .context("transactional metadata block count overflow")?
            .checked_mul(BLOCK_SIZE)
            .context("transactional metadata byte length overflow")?;
        load_type_layout_registry_from_block(&self.read(offset, len)?)
    }

    pub(crate) fn append_type_layout_metadata(
        &mut self,
        layout: &PersistentTypeLayout,
    ) -> Result<()> {
        self.initialize_metadata_blocks()?;
        let desc = self.validated_type_layout_metadata_desc()?;
        let offset =
            usize::try_from(desc.offset).context("transactional metadata offset overflow")?;
        let len = usize::try_from(TYPE_LAYOUT_METADATA_BLOCK_COUNT)
            .context("transactional metadata block count overflow")?
            .checked_mul(BLOCK_SIZE)
            .context("transactional metadata byte length overflow")?;
        let mut bytes = self.read(offset, len)?;
        if append_type_layout_to_block(&mut bytes, layout)? {
            self.write(offset, &bytes)?;
        }
        Ok(())
    }

    pub(crate) fn alloc_chunk(&mut self, block_count: usize) -> Result<RegionChunk> {
        self.alloc_chunk_for_kind(block_count, BlockKind::ObjectData, 0)
    }

    fn alloc_chunk_for_kind(
        &mut self,
        block_count: usize,
        kind: BlockKind,
        owner_thread: u32,
    ) -> Result<RegionChunk> {
        ensure!(block_count > 0, "transactional chunk must contain a block");
        ensure!(
            block_count <= self.num_blocks(),
            "transactional chunk exceeds region size"
        );
        ensure!(
            kind != BlockKind::Free,
            "transactional chunk allocations require a durable non-free kind"
        );

        let last_start = self.num_blocks() - block_count;
        for start in 0..=last_start {
            let end = start + block_count;
            if self.block_entries[start..end]
                .iter()
                .all(|entry| entry.used == 0)
            {
                let Some(generation) =
                    reusable_chunk_generation(&self.block_metas, start, block_count)?
                else {
                    continue;
                };
                let chunk_start =
                    u32::try_from(start).context("transactional chunk start block overflow")?;
                let chunk_blocks = u32::try_from(block_count)
                    .context("transactional chunk block count overflow")?;
                for entry in &mut self.block_entries[start..end] {
                    entry.used = 1;
                    entry.list_num = ListKind::Used as i16;
                }
                let meta =
                    BlockMeta::active(kind, generation, owner_thread, chunk_start, chunk_blocks);
                for block_meta in &mut self.block_metas[start..end] {
                    *block_meta = meta;
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

    pub(crate) fn grow_to_blocks(&mut self, new_block_count: usize) -> Result<Option<RegionChunk>> {
        let old_block_count = self.num_blocks();
        ensure!(
            new_block_count >= old_block_count,
            "transactional block region cannot shrink"
        );
        if new_block_count == old_block_count {
            return Ok(None);
        }

        let bytes_len = new_block_count
            .checked_mul(BLOCK_SIZE)
            .context("transactional block region size overflow")?;
        let additional_blocks = new_block_count - old_block_count;
        self.data.resize(bytes_len, 0);
        self.block_entries
            .resize(new_block_count, BlockEntry::default());
        self.block_metas.resize(new_block_count, BlockMeta::free());
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

    fn alloc_data_chunk_for_class(
        &mut self,
        stream_id: u32,
        chunk_seq: u32,
        class: DataChunkClass,
        kind: BlockKind,
        total_bytes: usize,
    ) -> Result<u32> {
        let chunk_blocks = match class {
            DataChunkClass::Small | DataChunkClass::Medium => 1,
            DataChunkClass::Large => self.required_data_chunk_blocks(total_bytes)?,
        };
        let chunk = self.alloc_chunk_for_kind(chunk_blocks, kind, stream_id)?;
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

    fn log_entry_capacity(&self) -> usize {
        (BLOCK_SIZE - size_of::<LogBlockHeader>()) / size_of::<TxLogEntry>()
    }

    fn initialize_metadata_blocks(&mut self) -> Result<()> {
        if self.metadata_initialized {
            return Ok(());
        }
        let reserved_metadata_blocks = reserved_metadata_blocks(self.num_blocks())?;
        ensure!(
            self.num_blocks() >= reserved_metadata_blocks,
            "transactional block region needs at least {reserved_metadata_blocks} blocks for metadata"
        );
        self.mark_blocks_used(0, reserved_metadata_blocks)?;
        self.write_metadata_desc_table()?;
        self.write_type_layout_metadata_block(&empty_type_layout_metadata_block())?;
        self.metadata_initialized = true;
        Ok(())
    }

    fn mark_blocks_used(&mut self, start: usize, block_count: usize) -> Result<()> {
        let end = start
            .checked_add(block_count)
            .context("transactional used-block range overflow")?;
        ensure!(
            end <= self.block_entries.len(),
            "transactional used-block range out of bounds"
        );
        ensure!(
            self.block_entries[start..end]
                .iter()
                .all(|entry| entry.used == 0),
            "transactional used-block range overlaps existing chunk"
        );
        for entry in &mut self.block_entries[start..end] {
            entry.used = 1;
            entry.list_num = ListKind::Used as i16;
        }
        Ok(())
    }

    fn type_layout_metadata_desc(&self) -> Result<MetaDataDesc> {
        let block = usize::try_from(metadata_desc_start_block(self.num_blocks())?)
            .context("transactional metadata descriptor start block overflow")?;
        let offset = block
            .checked_mul(BLOCK_SIZE)
            .context("transactional metadata descriptor offset overflow")?;
        MetaDataDesc::from_bytes(&self.read(offset, MetaDataDesc::BYTE_LEN)?)
    }

    fn validated_type_layout_metadata_desc(&self) -> Result<MetaDataDesc> {
        let desc = self.type_layout_metadata_desc()?;
        validate_type_layout_metadata_desc(desc, self.num_blocks())?;
        Ok(desc)
    }

    fn write_metadata_desc_table(&mut self) -> Result<()> {
        let block = usize::try_from(metadata_desc_start_block(self.num_blocks())?)
            .context("transactional metadata descriptor start block overflow")?;
        let offset = block
            .checked_mul(BLOCK_SIZE)
            .context("transactional metadata descriptor offset overflow")?;
        self.write(
            offset,
            &type_layout_metadata_desc(self.num_blocks())?.as_bytes(),
        )
    }

    fn write_type_layout_metadata_block(&mut self, bytes: &[u8]) -> Result<()> {
        let block = usize::try_from(type_layout_metadata_start_block(self.num_blocks())?)
            .context("transactional metadata block start overflow")?;
        let offset = block
            .checked_mul(BLOCK_SIZE)
            .context("transactional metadata block offset overflow")?;
        self.write(offset, bytes)
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

    pub(crate) fn block_meta(&self, block: u32) -> Result<BlockMeta> {
        let block = usize::try_from(block).context("transactional block index overflow")?;
        self.block_metas
            .get(block)
            .copied()
            .context("transactional block metadata index out of bounds")
    }

    #[cfg(test)]
    pub(crate) fn block_meta_for_test(&self, block: usize) -> Result<BlockMeta> {
        self.block_metas
            .get(block)
            .copied()
            .context("test block metadata index out of bounds")
    }

    #[cfg(test)]
    pub(crate) fn bump_block_generation_for_test(&mut self, block: u32) -> Result<()> {
        let block = usize::try_from(block).context("test block index overflow")?;
        let meta = self
            .block_metas
            .get_mut(block)
            .context("test block metadata index out of bounds")?;
        meta.generation = meta
            .generation
            .checked_add(1)
            .context("test block generation overflow")?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn write_block_meta_for_test(&mut self, block: u32, meta: BlockMeta) -> Result<()> {
        let block = usize::try_from(block).context("test block index overflow")?;
        let dest = self
            .block_metas
            .get_mut(block)
            .context("test block metadata index out of bounds")?;
        *dest = meta;
        Ok(())
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

    fn block_meta(&self, block: u32) -> Result<BlockMeta> {
        self.block_meta(block)
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

#[derive(Debug)]
pub(crate) struct NVMemoryBlockRegion {
    data: Vec<u8>,
    block_entries: Vec<BlockEntry>,
    block_metas: Vec<BlockMeta>,
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
            block_metas: vec![BlockMeta::free(); num_blocks],
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
        self.alloc_chunk_for_kind(block_count, BlockKind::ObjectData, 0)
    }

    fn alloc_chunk_for_kind(
        &mut self,
        block_count: usize,
        kind: BlockKind,
        owner_thread: u32,
    ) -> Result<RegionChunk> {
        ensure!(
            block_count > 0,
            "transactional NVMemory chunk must contain a block"
        );
        ensure!(
            block_count <= self.num_blocks(),
            "transactional NVMemory chunk exceeds region size"
        );
        ensure!(
            kind != BlockKind::Free,
            "transactional NVMemory chunk allocations require a durable non-free kind"
        );

        let last_start = self.num_blocks() - block_count;
        for start in 0..=last_start {
            let end = start + block_count;
            if self.block_entries[start..end]
                .iter()
                .all(|entry| entry.used == 0)
            {
                let Some(generation) =
                    reusable_chunk_generation(&self.block_metas, start, block_count)?
                else {
                    continue;
                };
                let chunk_start =
                    u32::try_from(start).context("transactional chunk start block overflow")?;
                let chunk_blocks = u32::try_from(block_count)
                    .context("transactional chunk block count overflow")?;
                for entry in &mut self.block_entries[start..end] {
                    entry.used = 1;
                    entry.list_num = ListKind::Used as i16;
                }
                let meta =
                    BlockMeta::active(kind, generation, owner_thread, chunk_start, chunk_blocks);
                for block_meta in &mut self.block_metas[start..end] {
                    *block_meta = meta;
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

    pub(crate) fn block_meta(&self, block: u32) -> Result<BlockMeta> {
        let block = usize::try_from(block).context("transactional block index overflow")?;
        self.block_metas
            .get(block)
            .copied()
            .context("transactional NVMemory block metadata index out of bounds")
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

    fn block_meta(&self, block: u32) -> Result<BlockMeta> {
        self.block_meta(block)
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
    block_metas: Vec<BlockMeta>,
    line_marks: Vec<LineMark>,
    streams: BTreeMap<u32, StreamState>,
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
            block_metas: vec![BlockMeta::free(); num_blocks],
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

    pub(crate) fn create_for_test(path: &Path, num_blocks: u32) -> Result<Self> {
        let num_blocks = usize::try_from(num_blocks)
            .context("transactional file-backed region size overflow")?;
        let reserved_metadata_blocks = reserved_metadata_blocks(num_blocks)?;
        ensure!(
            num_blocks >= reserved_metadata_blocks,
            "transactional file-backed region must have at least {reserved_metadata_blocks} blocks"
        );
        let mut region = Self::new(num_blocks, FileBackedRegionMode::Path(path.to_path_buf()))?;
        region.initialize_region_image()?;
        Ok(region)
    }

    pub(crate) fn open_for_test(path: &Path) -> Result<Self> {
        let mapping = FileBackedMapping::open_existing_path(path.to_path_buf())?;
        ensure!(
            mapping.len() >= BLOCK_SIZE,
            "transactional file-backed region image is smaller than one block"
        );
        ensure!(
            mapping.len() % BLOCK_SIZE == 0,
            "transactional file-backed region image length is not block-aligned"
        );

        let num_blocks = mapping.len() / BLOCK_SIZE;
        let line_count = mapping.len() / IMMIX_LINE_SIZE;
        let mut region = Self {
            mapping,
            block_entries: vec![BlockEntry::default(); num_blocks],
            block_metas: vec![BlockMeta::free(); num_blocks],
            line_marks: vec![
                LineMark {
                    mark: IMMIX_LINE_MARK_RESET_VALUE,
                };
                line_count
            ],
            streams: BTreeMap::new(),
        };
        let header = region.region_header()?;
        region.validate_region_header(header)?;
        region.load_block_meta_table(header)?;
        region.rebuild_state_from_image()?;
        Ok(region)
    }

    pub(crate) fn alloc_chunk(&mut self, block_count: usize) -> Result<RegionChunk> {
        self.alloc_chunk_for_kind(block_count, BlockKind::ObjectData, 0)
    }

    fn alloc_chunk_for_kind(
        &mut self,
        block_count: usize,
        kind: BlockKind,
        owner_thread: u32,
    ) -> Result<RegionChunk> {
        ensure!(
            block_count > 0,
            "transactional FileBackedMemory chunk must contain a block"
        );
        ensure!(
            block_count <= self.num_blocks(),
            "transactional FileBackedMemory chunk exceeds region size"
        );
        ensure!(
            kind != BlockKind::Free,
            "transactional FileBackedMemory chunk allocations require a durable non-free kind"
        );

        let last_start = self.num_blocks() - block_count;
        for start in 0..=last_start {
            let end = start + block_count;
            if self.block_entries[start..end]
                .iter()
                .all(|entry| entry.used == 0)
            {
                let Some(generation) =
                    reusable_chunk_generation(&self.block_metas, start, block_count)?
                else {
                    continue;
                };
                let chunk_start =
                    u32::try_from(start).context("transactional chunk start block overflow")?;
                let chunk_blocks = u32::try_from(block_count)
                    .context("transactional chunk block count overflow")?;
                for entry in &mut self.block_entries[start..end] {
                    entry.used = 1;
                    entry.list_num = ListKind::Used as i16;
                }
                let meta =
                    BlockMeta::active(kind, generation, owner_thread, chunk_start, chunk_blocks);
                for block in start..end {
                    self.write_block_meta(block, meta)?;
                }
                self.flush_block_meta_range(start, block_count)?;
                self.fence()?;
                return Ok(RegionChunk {
                    start_block: start,
                    block_count,
                });
            }
        }

        bail!("transactional FileBackedMemory block region is out of contiguous chunks")
    }

    pub(crate) fn alloc_log_block(&mut self, stream_id: u32, block_seq: u32) -> Result<u32> {
        let chunk = self.alloc_chunk_for_kind(1, BlockKind::Log, stream_id)?;
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
        self.alloc_data_chunk_for_kind(stream_id, chunk_seq, BlockKind::ObjectData, total_bytes)
    }

    fn alloc_data_chunk_for_kind(
        &mut self,
        stream_id: u32,
        chunk_seq: u32,
        kind: BlockKind,
        total_bytes: usize,
    ) -> Result<u32> {
        let class = if total_bytes <= SMALL_DATA_LIMIT {
            DataChunkClass::Small
        } else if total_bytes <= self.block_payload_capacity() {
            DataChunkClass::Medium
        } else {
            DataChunkClass::Large
        };
        self.alloc_data_chunk_for_class(stream_id, chunk_seq, class, kind, total_bytes)
    }

    pub(crate) fn alloc_stream(&mut self, stream_id: u32) -> Result<StreamCursor> {
        ensure!(
            !self.streams.contains_key(&stream_id),
            "transactional stream {stream_id} already exists"
        );
        self.streams.insert(stream_id, StreamState::default());
        Ok(StreamCursor { stream_id })
    }

    pub(crate) fn stream_cursor(&mut self, stream_id: u32) -> StreamCursor {
        self.streams.entry(stream_id).or_default();
        StreamCursor { stream_id }
    }

    pub(crate) fn append_data_record(
        &mut self,
        stream: StreamCursor,
        bytes: &[u8],
    ) -> Result<DataRecordLocation> {
        let kind = block_kind_for_data_record(bytes)?;
        let mut state = *self.streams.get(&stream.stream_id).with_context(|| {
            format!("transactional stream {} is not allocated", stream.stream_id)
        })?;

        let chunk_start_block = if let Some(start_block) = state.current_data_chunk_start {
            let header = self.data_chunk_header(start_block)?;
            let current_kind = self.block_meta(start_block)?.kind()?;
            if current_kind == kind && self.chunk_remaining_capacity(header)? >= bytes.len() {
                start_block
            } else {
                let next_chunk_start = self.alloc_data_chunk_for_kind(
                    stream.stream_id,
                    state.next_data_chunk_seq,
                    kind,
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
            let start_block = self.alloc_data_chunk_for_kind(
                stream.stream_id,
                state.next_data_chunk_seq,
                kind,
                bytes.len(),
            )?;
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

    pub(crate) fn append_log_entry(
        &mut self,
        stream: StreamCursor,
        entry: TxLogEntry,
    ) -> Result<u32> {
        let mut state = *self.streams.get(&stream.stream_id).with_context(|| {
            format!("transactional stream {} is not allocated", stream.stream_id)
        })?;

        let log_block = if let Some(start_block) = state.current_log_block_start {
            let header = self.log_block_header(start_block)?;
            if usize::try_from(header.entry_count)
                .context("transactional log entry count overflow")?
                < self.log_entry_capacity()
            {
                start_block
            } else {
                let next_block =
                    self.alloc_log_block(stream.stream_id, state.next_log_block_seq)?;
                let mut previous = header;
                previous.next_block = next_block;
                self.write_log_block_header(start_block, previous)?;
                state.next_log_block_seq = state
                    .next_log_block_seq
                    .checked_add(1)
                    .context("transactional log block sequence overflow")?;
                state.current_log_block_start = Some(next_block);
                next_block
            }
        } else {
            let start_block = self.alloc_log_block(stream.stream_id, state.next_log_block_seq)?;
            state.next_log_block_seq = state
                .next_log_block_seq
                .checked_add(1)
                .context("transactional log block sequence overflow")?;
            state.current_log_block_start = Some(start_block);
            start_block
        };

        let mut header = self.log_block_header(log_block)?;
        let entry_index = usize::try_from(header.entry_count)
            .context("transactional log entry count overflow")?;
        ensure!(
            entry_index < self.log_entry_capacity(),
            "transactional log block is full"
        );
        let write_offset = self
            .block_offset(log_block)?
            .checked_add(size_of::<LogBlockHeader>())
            .and_then(|offset| offset.checked_add(entry_index * size_of::<TxLogEntry>()))
            .context("transactional log entry write offset overflow")?;
        self.write(write_offset, &encode_tx_log_entry(entry))?;
        header.entry_count = header
            .entry_count
            .checked_add(1)
            .context("transactional log entry count overflow")?;
        self.write_log_block_header(log_block, header)?;
        self.streams.insert(stream.stream_id, state);

        Ok(log_block)
    }

    pub(crate) fn flush_data_chunk(&self, chunk_start_block: u32) -> Result<()> {
        let header = self.data_chunk_header(chunk_start_block)?;
        let chunk_blocks = usize::try_from(header.chunk_blocks)
            .context("transactional data chunk block count overflow")?;
        let flush_len = chunk_blocks
            .checked_mul(BLOCK_SIZE)
            .context("transactional data chunk flush length overflow")?;
        self.flush(self.block_offset(chunk_start_block)?, flush_len)
    }

    pub(crate) fn flush_log_block(&self, log_block: u32) -> Result<()> {
        self.flush(self.block_offset(log_block)?, BLOCK_SIZE)
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

    fn retire_chunk(&mut self, chunk_start_block: u32, expected_kind: BlockKind) -> Result<()> {
        let meta = self.block_meta(chunk_start_block)?;
        if !meta.is_active_or_sealed()? || meta.kind()? != expected_kind {
            return Ok(());
        }
        ensure!(
            meta.chunk_start == chunk_start_block,
            "transactional retired chunk input must point at the chunk start block"
        );

        let chunk_start =
            usize::try_from(chunk_start_block).context("retired chunk start block overflow")?;
        let chunk_blocks =
            usize::try_from(meta.chunk_blocks).context("retired chunk block count overflow")?;
        let chunk_end = chunk_start
            .checked_add(chunk_blocks)
            .context("retired chunk block range overflow")?;
        ensure!(
            chunk_end <= self.num_blocks(),
            "retired chunk metadata range is out of bounds"
        );

        let retired_meta = BlockMeta {
            state: BlockState::Retired as u8,
            kind: meta.kind,
            reserved0: 0,
            generation: meta.generation,
            owner_thread: meta.owner_thread,
            chunk_start: meta.chunk_start,
            chunk_blocks: meta.chunk_blocks,
            reserved1: 0,
        };
        for block in chunk_start..chunk_end {
            self.block_entries[block] = BlockEntry::default();
            self.block_entries[block].list_num = ListKind::LargeFree as i16;
            self.write_block_meta(block, retired_meta)?;
        }
        self.flush_block_meta_range(chunk_start, chunk_blocks)
    }

    pub(crate) fn retire_linear_undo_chunks<I>(&mut self, chunk_starts: I) -> Result<Vec<u32>>
    where
        I: IntoIterator<Item = u32>,
    {
        let mut retired = Vec::new();
        let mut seen = BTreeSet::new();
        for chunk_start_block in chunk_starts {
            if !seen.insert(chunk_start_block) {
                continue;
            }
            let meta = self.block_meta(chunk_start_block)?;
            if !meta.is_active_or_sealed()? || meta.kind()? != BlockKind::LinearUndo {
                continue;
            }
            self.retire_chunk(chunk_start_block, BlockKind::LinearUndo)?;
            retired.push(chunk_start_block);
        }

        if !retired.is_empty() {
            self.fence()?;
        }

        Ok(retired)
    }

    pub(crate) fn retire_whole_dead_object_chunks(
        &mut self,
        reachable: &[PersistentRecoveredRecordLocation],
        unreachable: &[PersistentRecoveredRecordLocation],
    ) -> Result<Vec<u32>> {
        let mut live_blocks = vec![false; self.num_blocks()];
        for location in reachable {
            let range = data_record_block_range(
                location.data_block,
                location.data_offset,
                location.record_len,
            )?;
            ensure!(
                range.end <= self.num_blocks(),
                "reachable object record blocks exceed region bounds"
            );
            for block in range {
                live_blocks[block] = true;
            }
        }

        let mut candidate_chunks = BTreeMap::<u32, usize>::new();
        for location in unreachable {
            let record_range = data_record_block_range(
                location.data_block,
                location.data_offset,
                location.record_len,
            )?;
            ensure!(
                record_range.end <= self.num_blocks(),
                "unreachable object record blocks exceed region bounds"
            );

            let meta = self.block_meta(location.data_block)?;
            if !meta.is_active_or_sealed()? || meta.kind()? != BlockKind::ObjectData {
                continue;
            }

            let chunk_start =
                usize::try_from(meta.chunk_start).context("retired chunk start block overflow")?;
            let chunk_blocks =
                usize::try_from(meta.chunk_blocks).context("retired chunk block count overflow")?;
            let chunk_end = chunk_start
                .checked_add(chunk_blocks)
                .context("retired chunk block range overflow")?;
            ensure!(
                chunk_end <= self.num_blocks(),
                "retired chunk metadata range is out of bounds"
            );
            ensure!(
                record_range.start >= chunk_start && record_range.end <= chunk_end,
                "recovered object record extends outside chunk metadata"
            );

            candidate_chunks.insert(meta.chunk_start, chunk_blocks);
        }

        let mut retired = Vec::new();
        for (chunk_start_block, chunk_blocks) in candidate_chunks {
            let chunk_start =
                usize::try_from(chunk_start_block).context("retired chunk start block overflow")?;
            let chunk_end = chunk_start
                .checked_add(chunk_blocks)
                .context("retired chunk block range overflow")?;
            if live_blocks[chunk_start..chunk_end].iter().any(|live| *live) {
                continue;
            }

            let meta = self.block_metas[chunk_start];
            if !meta.is_active_or_sealed()? || meta.kind()? != BlockKind::ObjectData {
                continue;
            }

            self.retire_chunk(chunk_start_block, BlockKind::ObjectData)?;
            retired.push(chunk_start_block);
        }

        if !retired.is_empty() {
            self.fence()?;
        }

        Ok(retired)
    }

    pub(crate) fn load_type_layout_metadata(&self) -> Result<TypeLayoutRegistry> {
        let desc = self.load_validated_type_layout_metadata_desc()?;
        let offset =
            usize::try_from(desc.offset).context("transactional metadata offset overflow")?;
        let len = usize::try_from(TYPE_LAYOUT_METADATA_BLOCK_COUNT)
            .context("transactional metadata block count overflow")?
            .checked_mul(BLOCK_SIZE)
            .context("transactional metadata byte length overflow")?;
        load_type_layout_registry_from_block(&self.read(offset, len)?)
    }

    pub(crate) fn append_type_layout_metadata(
        &mut self,
        layout: &PersistentTypeLayout,
    ) -> Result<()> {
        let desc = self.load_validated_type_layout_metadata_desc()?;
        let offset =
            usize::try_from(desc.offset).context("transactional metadata offset overflow")?;
        let len = usize::try_from(TYPE_LAYOUT_METADATA_BLOCK_COUNT)
            .context("transactional metadata block count overflow")?
            .checked_mul(BLOCK_SIZE)
            .context("transactional metadata byte length overflow")?;
        let mut bytes = self.read(offset, len)?;
        if append_type_layout_to_block(&mut bytes, layout)? {
            self.write(offset, &bytes)?;
            self.flush(offset, len)?;
            self.fence()?;
        }
        Ok(())
    }

    fn log_entry_capacity(&self) -> usize {
        (BLOCK_SIZE - size_of::<LogBlockHeader>()) / size_of::<TxLogEntry>()
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
        ensure!(
            table_blocks > 0,
            "transactional region has no block metadata table"
        );
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

    pub(crate) fn grow_to_blocks(&mut self, new_block_count: usize) -> Result<Option<RegionChunk>> {
        let old_block_count = self.num_blocks();
        ensure!(
            new_block_count >= old_block_count,
            "transactional FileBackedMemory block region cannot shrink"
        );
        if new_block_count == old_block_count {
            return Ok(None);
        }
        ensure!(
            block_table_block_count(old_block_count)? == block_table_block_count(new_block_count)?
                && metadata_desc_start_block(old_block_count)?
                    == metadata_desc_start_block(new_block_count)?
                && type_layout_metadata_start_block(old_block_count)?
                    == type_layout_metadata_start_block(new_block_count)?
                && reserved_metadata_blocks(old_block_count)?
                    == reserved_metadata_blocks(new_block_count)?,
            "file-backed block region grow would require metadata table relocation"
        );

        let bytes_len = new_block_count
            .checked_mul(BLOCK_SIZE)
            .context("transactional FileBackedMemory block region size overflow")?;
        self.mapping.remap_len(bytes_len)?;

        let additional_blocks = new_block_count - old_block_count;
        self.block_entries
            .resize(new_block_count, BlockEntry::default());
        self.block_metas.resize(new_block_count, BlockMeta::free());
        for entry in &mut self.block_entries[old_block_count..new_block_count] {
            entry.used = 1;
            entry.list_num = ListKind::Used as i16;
        }
        for block in old_block_count..new_block_count {
            self.write_block_meta(block, BlockMeta::free())?;
        }

        let line_count = bytes_len / IMMIX_LINE_SIZE;
        self.line_marks.resize(
            line_count,
            LineMark {
                mark: IMMIX_LINE_MARK_RESET_VALUE,
            },
        );
        self.write_region_header(region_header_for_num_blocks(new_block_count)?)?;
        self.flush_block_meta_range(old_block_count, additional_blocks)?;
        self.flush(0, size_of::<RegionHeader>())?;
        self.fence()?;

        Ok(Some(RegionChunk {
            start_block: old_block_count,
            block_count: additional_blocks,
        }))
    }

    #[cfg(test)]
    pub(crate) fn block_meta_for_test(&self, block: usize) -> Result<BlockMeta> {
        self.block_metas
            .get(block)
            .copied()
            .context("test block metadata index out of bounds")
    }

    fn initialize_region_image(&mut self) -> Result<()> {
        self.reset_recovered_state();
        let reserved_metadata_blocks = reserved_metadata_blocks(self.num_blocks())?;
        ensure!(
            self.num_blocks() >= reserved_metadata_blocks,
            "transactional file-backed region needs at least {reserved_metadata_blocks} blocks for metadata"
        );
        self.mark_blocks_used(0, reserved_metadata_blocks)?;
        self.write_region_header(region_header_for_num_blocks(self.num_blocks())?)?;
        for block in 0..self.num_blocks() {
            self.write_block_meta(block, BlockMeta::free())?;
        }
        for block in 0..reserved_metadata_blocks {
            self.write_block_meta(
                block,
                BlockMeta::active(
                    super::durable_log::BlockKind::Metadata,
                    0,
                    0,
                    u32::try_from(block).unwrap(),
                    1,
                ),
            )?;
        }
        self.write_metadata_desc_table()?;
        self.write_type_layout_metadata_block(&empty_type_layout_metadata_block())?;
        self.flush_block_meta_range(0, self.num_blocks())?;
        self.flush(
            REGION_HEADER_BLOCK as usize * BLOCK_SIZE,
            reserved_metadata_blocks * BLOCK_SIZE,
        )?;
        self.fence()
    }

    fn region_header(&self) -> Result<RegionHeader> {
        RegionHeader::from_bytes(self.mapping.read(0, size_of::<RegionHeader>())?)
    }

    fn write_region_header(&mut self, header: RegionHeader) -> Result<()> {
        self.mapping.write(0, &header.as_bytes())
    }

    fn validate_region_header(&self, header: RegionHeader) -> Result<()> {
        ensure!(
            header.magic == REGION_MAGIC,
            "transactional file-backed region image has invalid magic"
        );
        ensure!(
            usize::try_from(header.block_size).ok() == Some(BLOCK_SIZE),
            "transactional file-backed region image has unexpected block size"
        );
        // Research file-backed region images intentionally require the current
        // single type-layout descriptor shape; legacy zero-metadata images are rejected.
        ensure!(
            header.block_table_start_block == BLOCK_TABLE_START_BLOCK
                && header.block_table_block_count == block_table_block_count(self.num_blocks())?
                && header.metadata_descs_start_block
                    == metadata_desc_start_block(self.num_blocks())?
                && header.metadata_descs_block_count == METADATA_DESC_BLOCK_COUNT
                && header.num_descs == 1,
            "transactional file-backed region image has malformed metadata descriptor table header"
        );
        let expected_blocks = usize::try_from(header.num_blocks)
            .context("transactional file-backed region block count overflow")?;
        ensure!(
            expected_blocks == self.num_blocks(),
            "transactional file-backed region image length does not match header"
        );
        let descs = self.read_metadata_descs(header)?;
        ensure!(
            descs.len() == 1,
            "transactional file-backed region image must contain exactly one metadata descriptor"
        );
        validate_type_layout_metadata_desc(descs[0], self.num_blocks())?;
        Ok(())
    }

    fn rebuild_state_from_image(&mut self) -> Result<()> {
        self.reset_recovered_state();
        let reserved_metadata_blocks = reserved_metadata_blocks(self.num_blocks())?;
        self.mark_blocks_used(0, reserved_metadata_blocks)?;

        let mut chunk_tails = BTreeMap::<u32, (u32, u32)>::new();
        let mut log_tails = BTreeMap::<u32, (u32, u32)>::new();
        let mut covered_blocks = vec![false; self.num_blocks()];
        let mut block = reserved_metadata_blocks;
        while block < self.num_blocks() {
            let start_block = u32::try_from(block).context("transactional block index overflow")?;
            let meta = self.block_metas[block];
            if !meta.is_active_or_sealed()? {
                block += 1;
                continue;
            }
            match self.block_magic(start_block)? {
                LOG_BLOCK_MAGIC => {
                    if meta.kind()? != BlockKind::Log {
                        block += 1;
                        continue;
                    }
                    let header = self.log_block_header(start_block)?;
                    covered_blocks[block] = true;
                    self.mark_blocks_used(block, 1)?;
                    let state = self.streams.entry(header.stream_id).or_default();
                    let next_log_seq = header
                        .block_seq
                        .checked_add(1)
                        .context("transactional log block sequence overflow")?;
                    state.next_log_block_seq = state.next_log_block_seq.max(next_log_seq);
                    match log_tails.get(&header.stream_id) {
                        Some((current_seq, _)) if *current_seq >= header.block_seq => {}
                        _ => {
                            log_tails.insert(header.stream_id, (header.block_seq, start_block));
                        }
                    }
                    block += 1;
                }
                DATA_CHUNK_MAGIC => {
                    if !matches!(meta.kind()?, BlockKind::ObjectData | BlockKind::LinearUndo) {
                        block += 1;
                        continue;
                    }
                    let header = self.data_chunk_header(start_block)?;
                    let chunk_blocks = usize::try_from(header.chunk_blocks)
                        .context("transactional data chunk block count overflow")?;
                    ensure!(
                        chunk_blocks > 0,
                        "transactional data chunk at block {start_block} has zero blocks"
                    );
                    ensure!(
                        block + chunk_blocks <= self.num_blocks(),
                        "transactional data chunk at block {start_block} exceeds region"
                    );
                    covered_blocks[block..block + chunk_blocks].fill(true);
                    self.mark_blocks_used(block, chunk_blocks)?;
                    let state = self.streams.entry(header.stream_id).or_default();
                    let next_chunk_seq = header
                        .chunk_seq
                        .checked_add(1)
                        .context("transactional data chunk sequence overflow")?;
                    state.next_data_chunk_seq = state.next_data_chunk_seq.max(next_chunk_seq);
                    match chunk_tails.get(&header.stream_id) {
                        Some((current_seq, _)) if *current_seq >= header.chunk_seq => {}
                        _ => {
                            chunk_tails.insert(header.stream_id, (header.chunk_seq, start_block));
                        }
                    }
                    block += chunk_blocks;
                }
                _ => {
                    block += 1;
                }
            }
        }

        let mut repaired_runs = Vec::new();
        let mut repair_start = None;
        let mut repair_len = 0usize;
        for (block, covered) in covered_blocks
            .iter()
            .copied()
            .enumerate()
            .skip(reserved_metadata_blocks)
        {
            let meta = self.block_metas[block];
            let state = meta.state()?;
            let kind = meta.kind()?;
            let orphaned_reservation = !covered
                && kind != BlockKind::Metadata
                && matches!(state, BlockState::Active | BlockState::Sealed);
            if orphaned_reservation {
                self.write_block_meta(block, BlockMeta::free())?;
                self.block_entries[block] = BlockEntry::default();
                if let Some(start) = repair_start {
                    if start + repair_len == block {
                        repair_len += 1;
                    } else {
                        repaired_runs.push((start, repair_len));
                        repair_start = Some(block);
                        repair_len = 1;
                    }
                } else {
                    repair_start = Some(block);
                    repair_len = 1;
                }
            }
        }
        if let Some(start) = repair_start {
            repaired_runs.push((start, repair_len));
        }
        if !repaired_runs.is_empty() {
            for (start, len) in repaired_runs {
                self.flush_block_meta_range(start, len)?;
            }
            self.fence()?;
        }

        for (stream_id, (_, start_block)) in chunk_tails {
            self.streams
                .entry(stream_id)
                .or_default()
                .current_data_chunk_start = Some(start_block);
        }
        for (stream_id, (_, start_block)) in log_tails {
            self.streams
                .entry(stream_id)
                .or_default()
                .current_log_block_start = Some(start_block);
        }

        Ok(())
    }

    fn reset_recovered_state(&mut self) {
        for entry in &mut self.block_entries {
            *entry = BlockEntry::default();
        }
        self.streams.clear();
    }

    fn mark_blocks_used(&mut self, start: usize, block_count: usize) -> Result<()> {
        let end = start
            .checked_add(block_count)
            .context("transactional used-block range overflow")?;
        ensure!(
            end <= self.block_entries.len(),
            "transactional used-block range out of bounds"
        );
        ensure!(
            self.block_entries[start..end]
                .iter()
                .all(|entry| entry.used == 0),
            "transactional used-block range overlaps existing chunk"
        );
        for entry in &mut self.block_entries[start..end] {
            entry.used = 1;
            entry.list_num = ListKind::Used as i16;
        }
        Ok(())
    }

    fn block_magic(&self, start_block: u32) -> Result<u32> {
        let bytes = self.read_header_bytes(start_block, size_of::<u32>())?;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn alloc_data_chunk_for_class(
        &mut self,
        stream_id: u32,
        chunk_seq: u32,
        class: DataChunkClass,
        kind: BlockKind,
        total_bytes: usize,
    ) -> Result<u32> {
        let chunk_blocks = match class {
            DataChunkClass::Small | DataChunkClass::Medium => 1,
            DataChunkClass::Large => self.required_data_chunk_blocks(total_bytes)?,
        };
        let chunk = self.alloc_chunk_for_kind(chunk_blocks, kind, stream_id)?;
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

    fn load_validated_type_layout_metadata_desc(&self) -> Result<MetaDataDesc> {
        let header = self.region_header()?;
        self.validate_region_header(header)?;
        let mut descs = self.read_metadata_descs(header)?;
        Ok(descs.remove(0))
    }

    fn read_metadata_descs(&self, header: RegionHeader) -> Result<Vec<MetaDataDesc>> {
        let start_block = usize::try_from(header.metadata_descs_start_block)
            .context("transactional metadata descriptor start block overflow")?;
        let offset = start_block
            .checked_mul(BLOCK_SIZE)
            .context("transactional metadata descriptor offset overflow")?;
        let desc_count = usize::try_from(header.num_descs)
            .context("transactional metadata descriptor count overflow")?;
        let byte_len = desc_count
            .checked_mul(MetaDataDesc::BYTE_LEN)
            .context("transactional metadata descriptor table length overflow")?;
        let table_capacity = usize::try_from(header.metadata_descs_block_count)
            .context("transactional metadata descriptor block count overflow")?
            .checked_mul(BLOCK_SIZE)
            .context("transactional metadata descriptor table capacity overflow")?;
        ensure!(
            byte_len <= table_capacity,
            "transactional metadata descriptor table exceeds reserved blocks"
        );

        let bytes = self.read(offset, byte_len)?;
        let mut descs = Vec::with_capacity(desc_count);
        for chunk in bytes.chunks_exact(MetaDataDesc::BYTE_LEN) {
            descs.push(MetaDataDesc::from_bytes(chunk)?);
        }
        Ok(descs)
    }

    fn write_metadata_desc_table(&mut self) -> Result<()> {
        let offset = self.block_offset(metadata_desc_start_block(self.num_blocks())?)?;
        let mut bytes = vec![0; BLOCK_SIZE];
        bytes[..MetaDataDesc::BYTE_LEN]
            .copy_from_slice(&type_layout_metadata_desc(self.num_blocks())?.as_bytes());
        self.write(offset, &bytes)
    }

    fn write_type_layout_metadata_block(&mut self, bytes: &[u8]) -> Result<()> {
        let offset = self.block_offset(type_layout_metadata_start_block(self.num_blocks())?)?;
        self.write(offset, bytes)
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

    pub(crate) fn block_meta(&self, block: u32) -> Result<BlockMeta> {
        let block = usize::try_from(block).context("transactional block index overflow")?;
        self.block_metas
            .get(block)
            .copied()
            .context("transactional FileBackedMemory block metadata index out of bounds")
    }

    pub(crate) fn block_generation(&self, block: u32) -> Result<u32> {
        Ok(self.block_meta(block)?.generation)
    }

    fn write_log_block_header(&mut self, start_block: u32, header: LogBlockHeader) -> Result<()> {
        self.write(self.block_offset(start_block)?, &header.as_bytes())
    }

    fn write_data_chunk_header(&mut self, start_block: u32, header: DataChunkHeader) -> Result<()> {
        self.write(self.block_offset(start_block)?, &header.as_bytes())
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

    fn block_meta(&self, block: u32) -> Result<BlockMeta> {
        self.block_meta(block)
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

fn type_layout_metadata_desc(num_blocks: usize) -> Result<MetaDataDesc> {
    Ok(MetaDataDesc {
        kind: TYPE_LAYOUT_META_KIND,
        fixed_bytes: 4,
        unit_size: 1,
        bytes_per_unit: 1,
        padding: 0,
        offset: u64::try_from(
            usize::try_from(type_layout_metadata_start_block(num_blocks)?).unwrap() * BLOCK_SIZE,
        )
        .unwrap(),
    })
}

fn region_header_for_num_blocks(num_blocks: usize) -> Result<RegionHeader> {
    Ok(RegionHeader {
        magic: REGION_MAGIC,
        block_size: u32::try_from(BLOCK_SIZE).unwrap(),
        num_blocks: u32::try_from(num_blocks)
            .context("transactional file-backed region block count overflow")?,
        block_table_start_block: BLOCK_TABLE_START_BLOCK,
        block_table_block_count: block_table_block_count(num_blocks)?,
        metadata_descs_start_block: metadata_desc_start_block(num_blocks)?,
        metadata_descs_block_count: METADATA_DESC_BLOCK_COUNT,
        num_descs: 1,
    })
}

fn validate_type_layout_metadata_desc(desc: MetaDataDesc, num_blocks: usize) -> Result<()> {
    match desc.kind {
        TYPE_LAYOUT_META_KIND => {}
        kind => {
            bail!(
                "transactional file-backed region image has unknown metadata descriptor kind {kind:#x}"
            )
        }
    }
    ensure!(
        desc.fixed_bytes == 4 && desc.unit_size == 1 && desc.bytes_per_unit == 1,
        "transactional file-backed region image has malformed type-layout metadata descriptor"
    );

    let start = usize::try_from(desc.offset).context("transactional metadata offset overflow")?;
    ensure!(
        start % BLOCK_SIZE == 0,
        "transactional metadata descriptor offset must be block-aligned"
    );
    let start_block = start / BLOCK_SIZE;
    let block_count = usize::try_from(TYPE_LAYOUT_METADATA_BLOCK_COUNT)
        .context("transactional metadata block count overflow")?;
    let end_block = start_block
        .checked_add(block_count)
        .context("transactional metadata descriptor block range overflow")?;
    ensure!(
        end_block <= num_blocks,
        "transactional metadata descriptor exceeds region bounds"
    );
    ensure!(
        start_block >= 1,
        "transactional metadata descriptor overlaps region header block"
    );
    let type_layout_metadata_start_block =
        usize::try_from(type_layout_metadata_start_block(num_blocks)?)
            .context("transactional metadata start block overflow")?;
    let reserved_metadata_blocks = reserved_metadata_blocks(num_blocks)?;
    ensure!(
        start_block == type_layout_metadata_start_block && end_block <= reserved_metadata_blocks,
        "transactional metadata descriptor overlaps log or data blocks"
    );
    Ok(())
}

/// Narrow recovered-winner summary exported for file-backed persistence tests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionPersistenceRecoveredWinner {
    /// Logical granule or object identifier chosen by recovery.
    pub logical_id: u64,
    /// Committed version chosen by recovery.
    pub version: u32,
}

/// Narrow recovered object-winner summary exported for file-backed persistence tests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionPersistenceRecoveredObjectWinner {
    /// Persistent object identity selected by recovery.
    pub object_id: u64,
    /// Committed object version selected by recovery.
    pub version: u32,
    /// Serialized object record bytes selected by recovery.
    pub record_bytes: Vec<u8>,
}

/// Narrow recovered linear-memory undo rollback summary exported for
/// file-backed persistence tests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionPersistenceRecoveredTMemoryUndoRollback {
    /// Logical tmemory granule identifier selected for rollback.
    pub logical_id: u64,
    /// Undo-record version selected for rollback.
    pub version: u32,
    /// Old committed granule bytes that should be restored.
    pub old_granule_bytes: Vec<u8>,
}

/// Narrow recovered-region summary exported for file-backed persistence tests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionPersistenceRecoveredRegion {
    /// Winners selected by chunk-scan plus log replay recovery.
    pub winners: Vec<TransactionPersistenceRecoveredWinner>,
    /// Persistent object winners selected by recovery.
    pub object_winners: Vec<TransactionPersistenceRecoveredObjectWinner>,
    /// Persistent object ids recovered from root-bearing globals/tables.
    pub root_object_ids: Vec<u64>,
    /// TMemory undo records from transactions that did not publish LP.
    pub tmemory_undo_rollbacks: Vec<TransactionPersistenceRecoveredTMemoryUndoRollback>,
}

/// Creates a file-backed durable region image for restart smoke tests.
pub fn create_file_backed_region_image(path: &Path, num_blocks: u32) -> Result<()> {
    let _region = FileBackedMemoryBlockRegion::create_for_test(path, num_blocks)?;
    Ok(())
}

/// Publishes one committed tmemory update into a file-backed durable region image.
pub fn publish_committed_tmemory_update(
    path: &Path,
    stream_id: u32,
    logical_id: u64,
    version: u32,
    payload: &[u8],
) -> Result<()> {
    publish_committed_data_record(
        path,
        stream_id,
        logical_id,
        version,
        PackedGranuleDomain::TMemory as u16,
        0,
        payload,
    )
}

/// Publishes one committed tmemory undo record into a file-backed durable region image.
pub fn publish_committed_tmemory_undo_for_test(
    path: &Path,
    stream_id: u32,
    logical_id: u64,
    version: u32,
    payload: &[u8],
) -> Result<()> {
    let mut region = FileBackedMemoryBlockRegion::open_for_test(path)?;
    let stream = region.alloc_stream(stream_id)?;
    let record = TMemory::encode_granule_undo_data_record(
        logical_id,
        version,
        PackedGranuleDomain::TMemory as u16,
        0,
        payload,
    )?;
    let location = region.append_data_record(stream, &record)?;
    let log_block = region.alloc_log_block(stream_id, 0)?;
    let mut entry = TMemory::publication_log_entry(
        logical_id,
        version,
        stream_id << 1,
        location.data_block,
        location.data_offset,
        region.block_generation(location.data_block)?,
        true,
    )?;
    entry.set_role(TxLogEntryRole::TMemoryUndo);
    entry.seal_crc32();
    write_log_entries_to_file_backed_region(&mut region, log_block, &[entry])?;
    flush_chunk_for_recovery(&region, location.chunk_start_block)?;
    region.flush(region.block_offset(log_block)?, BLOCK_SIZE)?;
    region.fence()?;
    Ok(())
}

/// Publishes one committed persistent struct object into a file-backed region.
pub fn publish_committed_struct_object(
    path: &Path,
    stream_id: u32,
    object_id: u64,
    version: u32,
    type_info: u32,
    payload: &[u8],
) -> Result<()> {
    let fields = payload
        .iter()
        .map(|byte| ObjectValue::I32(i32::from(*byte)))
        .collect::<Vec<_>>();
    let mut region = FileBackedMemoryBlockRegion::open_for_test(path)?;
    region.append_type_layout_metadata(&scalar_struct_type_layout_for_test(
        type_info,
        fields.len(),
    )?)?;
    let object_record = encode_object_record_for_recovery(
        object_id,
        version,
        ObjectKind::Struct as u16,
        type_info,
        &ObjectPayload::Struct(fields),
    )?;
    publish_committed_data_record_into_region(
        &mut region,
        stream_id,
        pack_object_granule_id(PackedGranuleDomain::TStruct, object_id)?,
        version,
        PackedGranuleDomain::TStruct as u16,
        type_info,
        &object_record,
    )
}

/// Publishes one committed global root pointing at a persistent object.
pub fn publish_committed_global_object_root(
    path: &Path,
    stream_id: u32,
    object_id: u64,
) -> Result<()> {
    let mut payload = Vec::new();
    payload.extend_from_slice(
        &object_id
            .checked_add(1)
            .context("object root encoding overflow")?
            .to_le_bytes(),
    );
    publish_committed_data_record(
        path,
        stream_id,
        (PackedGranuleDomain::TGlobal as u64) << 60,
        1,
        PackedGranuleDomain::TGlobal as u16,
        0,
        &payload,
    )
}

fn publish_committed_data_record(
    path: &Path,
    stream_id: u32,
    logical_id: u64,
    version: u32,
    kind: u16,
    type_info: u32,
    payload: &[u8],
) -> Result<()> {
    let mut region = FileBackedMemoryBlockRegion::open_for_test(path)?;
    publish_committed_data_record_into_region(
        &mut region,
        stream_id,
        logical_id,
        version,
        kind,
        type_info,
        payload,
    )
}

fn publish_committed_data_record_into_region(
    region: &mut FileBackedMemoryBlockRegion,
    stream_id: u32,
    logical_id: u64,
    version: u32,
    kind: u16,
    type_info: u32,
    payload: &[u8],
) -> Result<()> {
    let stream = region.alloc_stream(stream_id)?;
    let record =
        TMemory::encode_publication_data_record(logical_id, version, kind, type_info, payload)?;
    let location = region.append_data_record(stream, &record)?;
    let log_block = region.alloc_log_block(stream_id, 0)?;
    let entry = TMemory::publication_log_entry(
        logical_id,
        version,
        stream_id << 1,
        location.data_block,
        location.data_offset,
        region.block_generation(location.data_block)?,
        true,
    )?;
    write_log_entries_to_file_backed_region(&mut *region, log_block, &[entry])?;
    flush_chunk_for_recovery(region, location.chunk_start_block)?;
    region.flush(region.block_offset(log_block)?, BLOCK_SIZE)?;
    region.fence()?;
    Ok(())
}

fn scalar_struct_type_layout_for_test(
    type_layout_id: u32,
    field_count: usize,
) -> Result<PersistentTypeLayout> {
    let mut fields = Vec::with_capacity(field_count);
    for field_index in 0..field_count {
        let field_index = u32::try_from(field_index).context("test struct field index overflow")?;
        fields.push(StructTraceField {
            field_index,
            field_offset: field_index
                .checked_mul(4)
                .context("test struct field offset overflow")?,
            value_size: 4,
            kind: TraceSlotKind::Scalar,
        });
    }
    Ok(PersistentTypeLayout::Struct {
        id: TypeLayoutId::new(type_layout_id)
            .context("test struct type layout id cannot be zero")?,
        fingerprint: 0x7478_5f73_7472_0000
            ^ u64::from(type_layout_id)
            ^ (u64::try_from(field_count).context("test struct field count overflow")? << 32),
        body_size: u32::try_from(field_count)
            .context("test struct field count exceeds u32")?
            .checked_mul(4)
            .context("test struct body size overflow")?,
        fields,
    })
}

/// Corrupts the first durable log-entry CRC in a file-backed region image.
pub fn corrupt_first_log_crc(path: &Path) -> Result<()> {
    let mut region = FileBackedMemoryBlockRegion::open_for_test(path)?;
    let mut block = 1usize;
    while block < region.num_blocks() {
        let start_block = u32::try_from(block).context("transactional block index overflow")?;
        match region.block_magic(start_block)? {
            LOG_BLOCK_MAGIC => {
                let entry_offset = region
                    .block_offset(start_block)?
                    .checked_add(size_of::<LogBlockHeader>())
                    .and_then(|offset| offset.checked_add(24))
                    .context("transactional log crc offset overflow")?;
                let mut crc_byte = region.read(entry_offset, 1)?;
                crc_byte[0] ^= 0x01;
                region.write(entry_offset, &crc_byte)?;
                region.flush(region.block_offset(start_block)?, BLOCK_SIZE)?;
                region.fence()?;
                return Ok(());
            }
            DATA_CHUNK_MAGIC => {
                let chunk_blocks =
                    usize::try_from(region.data_chunk_header(start_block)?.chunk_blocks)
                        .context("transactional data chunk block count overflow")?;
                ensure!(
                    chunk_blocks > 0,
                    "transactional data chunk at block {start_block} has zero blocks"
                );
                block += chunk_blocks;
            }
            _ => {
                block += 1;
            }
        }
    }

    bail!("transactional file-backed region image does not contain a log block")
}

/// Bumps the first object-data block generation in a file-backed region image.
pub fn bump_first_object_data_block_generation_for_test(path: &Path) -> Result<()> {
    let mut region = FileBackedMemoryBlockRegion::open_for_test(path)?;
    for block in 0..region.num_blocks() {
        let meta = region
            .block_meta(u32::try_from(block).context("transactional block index overflow")?)?;
        if !meta.is_active_or_sealed()? || meta.kind()? != BlockKind::ObjectData {
            continue;
        }
        let chunk_start =
            usize::try_from(meta.chunk_start).context("object-data chunk start block overflow")?;
        let chunk_blocks =
            usize::try_from(meta.chunk_blocks).context("object-data chunk block count overflow")?;
        let chunk_end = chunk_start
            .checked_add(chunk_blocks)
            .context("object-data chunk block range overflow")?;
        ensure!(
            chunk_end <= region.num_blocks(),
            "object-data chunk metadata range is out of bounds"
        );
        let next_generation = meta
            .generation
            .checked_add(1)
            .context("object-data block generation overflow")?;
        let updated = BlockMeta::active(
            BlockKind::ObjectData,
            next_generation,
            meta.owner_thread,
            meta.chunk_start,
            meta.chunk_blocks,
        );
        for chunk_block in chunk_start..chunk_end {
            region.write_block_meta(chunk_block, updated)?;
        }
        region.flush_block_meta_range(chunk_start, chunk_blocks)?;
        region.fence()?;
        return Ok(());
    }

    bail!("transactional file-backed region image does not contain an object-data block")
}

/// Reopens a file-backed durable region image and runs region recovery.
pub fn reopen_and_recover_file_backed_region(
    path: &Path,
) -> Result<TransactionPersistenceRecoveredRegion> {
    let recovered = reopen_and_recover_file_backed_region_for_runtime(path)?;
    Ok(TransactionPersistenceRecoveredRegion {
        winners: recovered
            .winners
            .into_iter()
            .map(|winner| TransactionPersistenceRecoveredWinner {
                logical_id: winner.logical_id,
                version: winner.version,
            })
            .collect(),
        object_winners: recovered
            .object_winners
            .into_iter()
            .map(|winner| TransactionPersistenceRecoveredObjectWinner {
                object_id: winner.object_id,
                version: winner.version,
                record_bytes: winner.record_bytes,
            })
            .collect(),
        root_object_ids: recovered.root_object_ids,
        tmemory_undo_rollbacks: recovered
            .tmemory_undo_rollbacks
            .into_iter()
            .map(
                |rollback| TransactionPersistenceRecoveredTMemoryUndoRollback {
                    logical_id: rollback.logical_id,
                    version: rollback.version,
                    old_granule_bytes: rollback.old_granule_bytes,
                },
            )
            .collect(),
    })
}

/// Retires committed linear undo chunks that are not still needed for loose-end rollback.
pub fn retire_completed_linear_undo_chunks_for_test(path: &Path) -> Result<Vec<u32>> {
    let mut region = FileBackedMemoryBlockRegion::open_for_test(path)?;
    let (committed_chunks, loose_end_chunks) = collect_linear_undo_chunk_starts(&region)?;
    region.retire_linear_undo_chunks(
        committed_chunks
            .difference(&loose_end_chunks)
            .copied()
            .collect::<Vec<_>>(),
    )
}

fn collect_linear_undo_chunk_starts(
    region: &FileBackedMemoryBlockRegion,
) -> Result<(BTreeSet<u32>, BTreeSet<u32>)> {
    let view = region.view();
    let mut log_blocks_by_stream = BTreeMap::<u32, Vec<(u32, u32)>>::new();
    let mut block = 1usize;

    while block < view.num_blocks() {
        let start_block = u32::try_from(block).context("transactional block index overflow")?;
        let meta = view.block_meta(start_block)?;
        if !meta.is_active_or_sealed()? {
            block += 1;
            continue;
        }

        match view.block_magic(start_block)? {
            LOG_BLOCK_MAGIC => {
                if meta.kind()? != BlockKind::Log {
                    block += 1;
                    continue;
                }
                let header = view.log_block_header(start_block)?;
                log_blocks_by_stream
                    .entry(header.stream_id)
                    .or_default()
                    .push((header.block_seq, start_block));
                block += 1;
            }
            DATA_CHUNK_MAGIC => {
                if !matches!(meta.kind()?, BlockKind::ObjectData | BlockKind::LinearUndo) {
                    block += 1;
                    continue;
                }
                let chunk_blocks =
                    usize::try_from(view.data_chunk_header(start_block)?.chunk_blocks)
                        .context("transactional data chunk block count overflow")?;
                ensure!(
                    chunk_blocks > 0,
                    "transactional data chunk at block {start_block} has zero blocks"
                );
                block += chunk_blocks;
            }
            _ => {
                block += 1;
            }
        }
    }

    let mut committed_chunks = BTreeSet::new();
    let mut loose_end_chunks = BTreeSet::new();
    for (stream_id, mut log_blocks) in log_blocks_by_stream {
        log_blocks.sort_unstable();
        let mut pending_txid = None;
        let mut pending_chunks = BTreeSet::new();

        for (_, log_block) in log_blocks {
            for entry in view.log_block_entries(log_block)? {
                ensure!(
                    entry.validate_crc32(),
                    "transactional recovery found corrupt log entry in stream {} block {}",
                    stream_id,
                    log_block
                );
                let txid = entry.tx_meta >> 1;
                if pending_txid.is_some_and(|current| current != txid) {
                    loose_end_chunks.extend(pending_chunks.iter().copied());
                    pending_chunks.clear();
                }
                pending_txid = Some(txid);

                if let Some(chunk_start_block) = linear_undo_chunk_start_for_entry(region, entry)? {
                    pending_chunks.insert(chunk_start_block);
                }

                if (entry.tx_meta & 1) != 0 {
                    committed_chunks.extend(pending_chunks.iter().copied());
                    pending_chunks.clear();
                    pending_txid = None;
                }
            }
        }

        if pending_txid.is_some() {
            loose_end_chunks.extend(pending_chunks);
        }
    }

    Ok((committed_chunks, loose_end_chunks))
}

fn linear_undo_chunk_start_for_entry(
    region: &FileBackedMemoryBlockRegion,
    entry: TxLogEntry,
) -> Result<Option<u32>> {
    if entry.role()? != TxLogEntryRole::TMemoryUndo {
        return Ok(None);
    }

    let meta = region.block_meta(entry.data_block)?;
    if !meta.is_active_or_sealed()? || meta.kind()? != BlockKind::LinearUndo {
        return Ok(None);
    }
    if meta.generation != entry.data_block_generation() {
        return Ok(None);
    }

    Ok(Some(meta.chunk_start))
}

pub(crate) fn reopen_and_recover_file_backed_region_for_runtime(
    path: &Path,
) -> Result<super::recovery::RecoveredRegion> {
    let region = FileBackedMemoryBlockRegion::open_for_test(path)?;
    super::recovery::recover_region(&region.view(), region.load_type_layout_metadata()?)
}

#[cfg(test)]
pub(crate) fn reopen_and_recover_file_backed_region_for_test(
    path: &Path,
) -> Result<super::recovery::RecoveredRegion> {
    reopen_and_recover_file_backed_region_for_runtime(path)
}

#[cfg(test)]
pub(crate) fn reopen_and_recover_file_backed_object_winners_for_test(
    path: &Path,
) -> Result<Vec<super::recovery::RecoveredObjectWinner>> {
    let recovered = reopen_and_recover_file_backed_region_for_test(path)?;
    recovered.committed_object_winners()
}

fn flush_chunk_for_recovery(
    region: &FileBackedMemoryBlockRegion,
    chunk_start_block: u32,
) -> Result<()> {
    let header = region.data_chunk_header(chunk_start_block)?;
    let chunk_blocks = usize::try_from(header.chunk_blocks)
        .context("transactional data chunk block count overflow")?;
    let flush_len = chunk_blocks
        .checked_mul(BLOCK_SIZE)
        .context("transactional data chunk flush length overflow")?;
    region.flush(region.block_offset(chunk_start_block)?, flush_len)
}

fn write_log_entries_to_file_backed_region(
    region: &mut FileBackedMemoryBlockRegion,
    start_block: u32,
    entries: &[TxLogEntry],
) -> Result<()> {
    let mut header = region.log_block_header(start_block)?;
    header.entry_count =
        u32::try_from(entries.len()).context("transactional log entry count overflow")?;
    let block_offset = region.block_offset(start_block)?;
    region.write(block_offset, &header.as_bytes())?;
    let mut offset = block_offset
        .checked_add(size_of::<LogBlockHeader>())
        .context("transactional log entry write offset overflow")?;
    for entry in entries {
        region.write(offset, &encode_tx_log_entry(*entry))?;
        offset = offset
            .checked_add(size_of::<TxLogEntry>())
            .context("transactional log entry write offset overflow")?;
    }
    Ok(())
}

fn encode_tx_log_entry(entry: TxLogEntry) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[0..8].copy_from_slice(&entry.logical_id.to_le_bytes());
    bytes[8..12].copy_from_slice(&entry.version.to_le_bytes());
    bytes[12..16].copy_from_slice(&entry.tx_meta.to_le_bytes());
    bytes[16..20].copy_from_slice(&entry.data_block.to_le_bytes());
    bytes[20..24].copy_from_slice(&entry.data_offset.to_le_bytes());
    bytes[24..28].copy_from_slice(&entry.crc32.to_le_bytes());
    bytes[28..32].copy_from_slice(&entry.entry_meta.to_le_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::transaction::type_layout::{
        PersistentTypeKind, PersistentTypeLayout, StructTraceField, TraceSlotKind, TypeLayoutId,
    };
    use crate::runtime::vm::memory::tmemory::DataChunkHeader;
    use crate::runtime::vm::memory::tmemory::durable_log::{BlockKind, BlockMeta};
    use crate::runtime::vm::memory::tmemory::metadata::{
        TYPE_LAYOUT_META_KIND, TYPE_LAYOUT_METADATA_HEADER_LEN, metadata_desc_start_block,
        reserved_metadata_blocks, type_layout_metadata_start_block,
    };
    use core::mem::size_of;
    use std::sync::{Mutex, OnceLock};

    const SMALL_DATA_RECORD_BYTES: usize = IMMIX_LINE_SIZE;
    const MEDIUM_DATA_RECORD_BYTES: usize = BLOCK_SIZE - size_of::<DataChunkHeader>();
    const LARGE_DATA_RECORD_BYTES: usize = MEDIUM_DATA_RECORD_BYTES + 1;

    fn file_backed_temp_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn sample_struct_layout(id: u32, fingerprint: u64) -> PersistentTypeLayout {
        PersistentTypeLayout::Struct {
            id: TypeLayoutId::new(id).unwrap(),
            fingerprint,
            body_size: 24,
            fields: vec![
                StructTraceField {
                    field_index: 0,
                    field_offset: 0,
                    value_size: 8,
                    kind: TraceSlotKind::ObjectRef,
                },
                StructTraceField {
                    field_index: 1,
                    field_offset: 8,
                    value_size: 4,
                    kind: TraceSlotKind::Scalar,
                },
            ],
        }
    }

    fn sample_scalar_layout(id: u32, fingerprint: u64) -> PersistentTypeLayout {
        PersistentTypeLayout::Scalar {
            id: TypeLayoutId::new(id).unwrap(),
            fingerprint,
            kind: PersistentTypeKind::Extern,
        }
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
    fn vmemory_block_region_grows_and_returns_new_chunk() {
        let mut region = VMemoryBlockRegion::new(1).unwrap();
        let first = region.alloc_chunk(1).unwrap();
        let first_range = first.byte_range();
        region.write(first_range.start + 16, &[1, 2, 3, 4]).unwrap();

        let grown = region.grow_to_blocks(3).unwrap().unwrap();

        assert_eq!(grown.start_block(), 1);
        assert_eq!(grown.block_count(), 2);
        assert_eq!(region.num_blocks(), 3);
        assert_eq!(
            region.read(first_range.start + 16, 4).unwrap(),
            vec![1, 2, 3, 4]
        );
        region
            .write(grown.byte_range().start, &[5, 6, 7, 8])
            .unwrap();
        assert_eq!(
            region.read(grown.byte_range().start, 4).unwrap(),
            vec![5, 6, 7, 8]
        );
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
            region
                .block_meta_for_test(usize::try_from(object_location.data_block).unwrap())
                .unwrap()
                .kind()
                .unwrap(),
            BlockKind::ObjectData
        );
        assert_eq!(
            region
                .block_meta_for_test(usize::try_from(log_block).unwrap())
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
            region
                .block_meta_for_test(usize::try_from(location.data_block).unwrap())
                .unwrap()
                .kind()
                .unwrap(),
            BlockKind::LinearUndo
        );
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
    fn file_backed_region_image_initializes_type_layout_metadata_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata-init.tmemory");
        let region = FileBackedMemoryBlockRegion::create_for_test(&path, 32).unwrap();
        let metadata_desc_start_block = metadata_desc_start_block(region.num_blocks()).unwrap();
        let type_layout_metadata_start_block =
            type_layout_metadata_start_block(region.num_blocks()).unwrap();
        let reserved_metadata_blocks = reserved_metadata_blocks(region.num_blocks()).unwrap();

        let header = region.region_header().unwrap();
        assert_eq!(header.metadata_descs_start_block, metadata_desc_start_block);
        assert_eq!(header.metadata_descs_block_count, 1);
        assert_eq!(header.num_descs, 1);

        let desc_offset = usize::try_from(metadata_desc_start_block).unwrap() * BLOCK_SIZE;
        let desc =
            MetaDataDesc::from_bytes(&region.read(desc_offset, META_DATA_DESC_SIZE).unwrap())
                .unwrap();
        assert_eq!(desc.kind, TYPE_LAYOUT_META_KIND);
        assert_eq!(
            desc.offset,
            u64::try_from(usize::try_from(type_layout_metadata_start_block).unwrap() * BLOCK_SIZE)
                .unwrap()
        );

        let metadata_offset =
            usize::try_from(type_layout_metadata_start_block).unwrap() * BLOCK_SIZE;
        let metadata = region
            .read(metadata_offset, TYPE_LAYOUT_METADATA_HEADER_LEN)
            .unwrap();
        assert_eq!(metadata, 0u32.to_le_bytes());

        let reserved = region
            .block_entries
            .iter()
            .take(reserved_metadata_blocks)
            .filter(|entry| entry.used == 1)
            .count();
        assert_eq!(reserved, reserved_metadata_blocks);
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_region_reopen_loads_empty_type_layout_registry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata-empty.tmemory");
        FileBackedMemoryBlockRegion::create_for_test(&path, 32).unwrap();

        let region = FileBackedMemoryBlockRegion::open_for_test(&path).unwrap();
        let registry = region.load_type_layout_metadata().unwrap();

        assert_eq!(registry.iter().count(), 0);
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[test]
    fn file_backed_region_grow_persists_new_free_block_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("block-meta-grow-region.bin");

        {
            let mut region = FileBackedMemoryBlockRegion::create_for_test(&path, 32).unwrap();
            let grown = region.grow_to_blocks(64).unwrap().unwrap();
            assert_eq!(grown.start_block(), 32);
            assert_eq!(grown.block_count(), 32);
        }

        let region = FileBackedMemoryBlockRegion::open_for_test(&path).unwrap();

        assert_eq!(region.num_blocks(), 64);
        assert_eq!(region.block_meta_for_test(63).unwrap(), BlockMeta::free());
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_region_reopen_repairs_orphaned_active_block_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orphan-block-meta-region.bin");
        let orphan_block;

        {
            let mut region = FileBackedMemoryBlockRegion::create_for_test(&path, 32).unwrap();
            orphan_block = reserved_metadata_blocks(region.num_blocks()).unwrap();
            region
                .write_block_meta(
                    orphan_block,
                    BlockMeta::active(
                        BlockKind::ObjectData,
                        7,
                        11,
                        u32::try_from(orphan_block).unwrap(),
                        1,
                    ),
                )
                .unwrap();
            region.flush_block_meta_range(orphan_block, 1).unwrap();
            region.fence().unwrap();
        }

        {
            let region = FileBackedMemoryBlockRegion::open_for_test(&path).unwrap();
            assert_eq!(
                region.block_meta_for_test(orphan_block).unwrap(),
                BlockMeta::free()
            );
            assert_eq!(region.block_entries[orphan_block].used, 0);
        }

        let recovered = reopen_and_recover_file_backed_region_for_test(&path).unwrap();
        assert!(recovered.streams.is_empty());
        assert!(recovered.winners.is_empty());

        let mut region = FileBackedMemoryBlockRegion::open_for_test(&path).unwrap();
        let stream = region.alloc_stream(5).unwrap();
        let record = TMemory::encode_publication_data_record(
            0x6000_0000_0000_0005,
            1,
            PackedGranuleDomain::TStruct as u16,
            17,
            &[1, 2, 3, 4],
        )
        .unwrap();
        let location = region.append_data_record(stream, &record).unwrap();

        assert_eq!(
            usize::try_from(location.chunk_start_block).unwrap(),
            orphan_block
        );
        assert_eq!(
            region
                .block_meta_for_test(orphan_block)
                .unwrap()
                .kind()
                .unwrap(),
            BlockKind::ObjectData
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_region_reuses_retired_object_chunk_with_incremented_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retired-object-reuse-region.bin");
        let retired_chunk;
        let old_generation;

        {
            let mut region = FileBackedMemoryBlockRegion::create_for_test(&path, 32).unwrap();
            let stream = region.alloc_stream(7).unwrap();
            let record = TMemory::encode_publication_data_record(
                pack_object_granule_id(PackedGranuleDomain::TStruct, 41).unwrap(),
                1,
                PackedGranuleDomain::TStruct as u16,
                19,
                &[1, 2, 3, 4],
            )
            .unwrap();
            let location = region.append_data_record(stream, &record).unwrap();
            retired_chunk = location.chunk_start_block;
            old_generation = region
                .block_meta_for_test(usize::try_from(location.data_block).unwrap())
                .unwrap()
                .generation;
            region
                .retire_whole_dead_object_chunks(
                    &[],
                    &[PersistentRecoveredRecordLocation {
                        object_id: crate::runtime::transaction::ObjectId { object_index: 41 },
                        version: 1,
                        data_block: location.data_block,
                        data_offset: location.data_offset,
                        record_len: u64::try_from(record.len()).unwrap(),
                    }],
                )
                .unwrap();
        }

        let mut region = FileBackedMemoryBlockRegion::open_for_test(&path).unwrap();
        let stream = region.alloc_stream(8).unwrap();
        let record = TMemory::encode_publication_data_record(
            pack_object_granule_id(PackedGranuleDomain::TStruct, 42).unwrap(),
            1,
            PackedGranuleDomain::TStruct as u16,
            19,
            &[5, 6, 7, 8],
        )
        .unwrap();
        let location = region.append_data_record(stream, &record).unwrap();

        assert_eq!(location.chunk_start_block, retired_chunk);
        assert_eq!(
            region
                .block_meta_for_test(usize::try_from(location.data_block).unwrap())
                .unwrap()
                .generation,
            old_generation + 1
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_region_reuses_retired_linear_undo_chunk_with_incremented_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retired-linear-undo-reuse-region.bin");
        let retired_chunk;
        let old_generation;

        {
            let mut region = FileBackedMemoryBlockRegion::create_for_test(&path, 32).unwrap();
            let stream = region.alloc_stream(7).unwrap();
            let record = TMemory::encode_granule_undo_data_record(
                0x1000_0000_0000_0007,
                1,
                PackedGranuleDomain::TMemory as u16,
                0,
                &[1, 2, 3, 4],
            )
            .unwrap();
            let location = region.append_data_record(stream, &record).unwrap();
            retired_chunk = location.chunk_start_block;
            old_generation = region
                .block_meta_for_test(usize::try_from(location.data_block).unwrap())
                .unwrap()
                .generation;
            region
                .retire_linear_undo_chunks([location.chunk_start_block])
                .unwrap();
        }

        let mut region = FileBackedMemoryBlockRegion::open_for_test(&path).unwrap();
        let stream = region.alloc_stream(8).unwrap();
        let record = TMemory::encode_granule_undo_data_record(
            0x1000_0000_0000_0008,
            1,
            PackedGranuleDomain::TMemory as u16,
            0,
            &[5, 6, 7, 8],
        )
        .unwrap();
        let location = region.append_data_record(stream, &record).unwrap();

        assert_eq!(location.chunk_start_block, retired_chunk);
        assert_eq!(
            region
                .block_meta_for_test(usize::try_from(location.data_block).unwrap())
                .unwrap()
                .generation,
            old_generation + 1
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_region_reopen_keeps_multiblock_data_chunk_covered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multiblock-coverage-region.bin");
        let chunk_start_block;
        let chunk_blocks;

        {
            let mut region = FileBackedMemoryBlockRegion::create_for_test(&path, 32).unwrap();
            let stream = region.alloc_stream(7).unwrap();
            let record = TMemory::encode_publication_data_record(
                0x6000_0000_0000_0007,
                1,
                PackedGranuleDomain::TStruct as u16,
                19,
                &vec![0x5a; LARGE_DATA_RECORD_BYTES],
            )
            .unwrap();
            let location = region.append_data_record(stream, &record).unwrap();
            chunk_start_block = location.chunk_start_block;
            chunk_blocks = region
                .data_chunk_header(chunk_start_block)
                .unwrap()
                .chunk_blocks;
            region.flush_data_chunk(chunk_start_block).unwrap();
            region.fence().unwrap();
        }

        let mut region = FileBackedMemoryBlockRegion::open_for_test(&path).unwrap();
        assert!(chunk_blocks > 1);
        for block in usize::try_from(chunk_start_block).unwrap()
            ..usize::try_from(chunk_start_block).unwrap() + usize::try_from(chunk_blocks).unwrap()
        {
            let meta = region.block_meta_for_test(block).unwrap();
            assert_eq!(meta.kind().unwrap(), BlockKind::ObjectData);
            assert_eq!(meta.state().unwrap(), BlockState::Active);
            assert_eq!(region.block_entries[block].used, 1);
        }

        let next_chunk = region.alloc_chunk(1).unwrap();
        assert_eq!(
            next_chunk.start_block(),
            usize::try_from(chunk_start_block).unwrap() + usize::try_from(chunk_blocks).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_region_rejects_malformed_metadata_descriptor_header_values() {
        let dir = tempfile::tempdir().unwrap();

        for (suffix, offset, bytes, expected) in [
            (
                "bad-start",
                20usize,
                0u32.to_le_bytes().to_vec(),
                "malformed metadata descriptor table header",
            ),
            (
                "bad-count",
                24usize,
                0u32.to_le_bytes().to_vec(),
                "malformed metadata descriptor table header",
            ),
            (
                "bad-num-descs",
                28usize,
                2u32.to_le_bytes().to_vec(),
                "malformed metadata descriptor table header",
            ),
        ] {
            let path = dir.path().join(format!("{suffix}.tmemory"));
            let mut region = FileBackedMemoryBlockRegion::create_for_test(&path, 32).unwrap();
            region.write(offset, &bytes).unwrap();
            region.flush(0, size_of::<RegionHeader>()).unwrap();
            region.fence().unwrap();
            drop(region);

            let err = FileBackedMemoryBlockRegion::open_for_test(&path)
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "{suffix}: {err}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_region_rejects_invalid_metadata_descriptor_kind_or_offset() {
        let dir = tempfile::tempdir().unwrap();
        let desc_offset =
            usize::try_from(metadata_desc_start_block(32).unwrap()).unwrap() * BLOCK_SIZE;

        for (suffix, field_offset, bytes, expected) in [
            (
                "bad-kind",
                0usize,
                0xffff_ffff_ffff_ffffu64.to_le_bytes().to_vec(),
                "unknown metadata descriptor kind",
            ),
            (
                "bad-offset",
                24usize,
                0u64.to_le_bytes().to_vec(),
                "overlaps region header block",
            ),
        ] {
            let path = dir.path().join(format!("{suffix}.tmemory"));
            let mut region = FileBackedMemoryBlockRegion::create_for_test(&path, 32).unwrap();
            region.write(desc_offset + field_offset, &bytes).unwrap();
            region.flush(desc_offset, META_DATA_DESC_SIZE).unwrap();
            region.fence().unwrap();
            drop(region);

            let err = FileBackedMemoryBlockRegion::open_for_test(&path)
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "{suffix}: {err}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_region_type_layout_metadata_round_trips_one_layout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata-one.tmemory");
        let layout = sample_struct_layout(11, 0x1111_2222_3333_4444);

        let mut region = FileBackedMemoryBlockRegion::create_for_test(&path, 32).unwrap();
        region.append_type_layout_metadata(&layout).unwrap();
        drop(region);

        let region = FileBackedMemoryBlockRegion::open_for_test(&path).unwrap();
        let registry = region.load_type_layout_metadata().unwrap();
        let layouts = registry.iter().cloned().collect::<Vec<_>>();

        assert_eq!(layouts, vec![layout]);
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_region_type_layout_metadata_round_trips_two_layouts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata-two.tmemory");
        let first = sample_struct_layout(21, 0xaaaa_bbbb_cccc_dddd);
        let second = sample_scalar_layout(22, 0xeeee_ffff_0000_1111);

        let mut region = FileBackedMemoryBlockRegion::create_for_test(&path, 32).unwrap();
        region.append_type_layout_metadata(&first).unwrap();
        region.append_type_layout_metadata(&second).unwrap();
        drop(region);

        let region = FileBackedMemoryBlockRegion::open_for_test(&path).unwrap();
        let registry = region.load_type_layout_metadata().unwrap();
        let layouts = registry.iter().cloned().collect::<Vec<_>>();

        assert_eq!(layouts, vec![first, second]);
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_region_rejects_corrupt_type_layout_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata-corrupt.tmemory");
        let layout = sample_struct_layout(31, 0x0123_4567_89ab_cdef);

        let mut region = FileBackedMemoryBlockRegion::create_for_test(&path, 32).unwrap();
        region.append_type_layout_metadata(&layout).unwrap();

        let corrupt_offset =
            usize::try_from(type_layout_metadata_start_block(32).unwrap()).unwrap() * BLOCK_SIZE
                + TYPE_LAYOUT_METADATA_HEADER_LEN
                + 12;
        region
            .write(corrupt_offset, &0xffff_u16.to_le_bytes())
            .unwrap();
        region.flush(corrupt_offset, 2).unwrap();
        region.fence().unwrap();
        drop(region);

        let region = FileBackedMemoryBlockRegion::open_for_test(&path).unwrap();
        let err = region.load_type_layout_metadata().unwrap_err().to_string();

        assert!(err.contains("unknown persistent type kind"));
    }

    #[test]
    fn vmemory_type_layout_metadata_round_trips_one_layout() {
        let layout = sample_struct_layout(41, 0x5566_7788_99aa_bbcc);
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();

        region.append_type_layout_metadata(&layout).unwrap();

        let registry = region.load_type_layout_metadata().unwrap();
        let layouts = registry.iter().cloned().collect::<Vec<_>>();

        assert_eq!(layouts, vec![layout]);
    }

    #[test]
    fn vmemory_rejects_corrupt_type_layout_metadata_descriptor() {
        let layout = sample_struct_layout(43, 0x7788_99aa_bbcc_ddee);
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();

        region.append_type_layout_metadata(&layout).unwrap();

        let corrupt_offset =
            usize::try_from(metadata_desc_start_block(32).unwrap()).unwrap() * BLOCK_SIZE;
        region
            .write(corrupt_offset, &0xffff_u64.to_le_bytes())
            .unwrap();

        let err = region.load_type_layout_metadata().unwrap_err().to_string();
        assert!(err.contains("unknown metadata descriptor kind"), "{err}");
    }

    #[test]
    fn vmemory_type_layout_metadata_requires_reserved_blocks_before_other_allocations() {
        let layout = sample_struct_layout(42, 0x6677_8899_aabb_ccdd);
        let mut region = VMemoryBlockRegion::new_for_test(4).unwrap();

        let _chunk = region.alloc_chunk(1).unwrap();

        let err = region
            .append_type_layout_metadata(&layout)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("used-block range overlaps existing chunk")
                || err.contains("blocks for metadata"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_backed_initialized_region_requires_reserved_metadata_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("too-small-metadata.tmemory");

        let err = FileBackedMemoryBlockRegion::create_for_test(&path, 2)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("at least 3 blocks") || err.contains("metadata"),
            "{err}"
        );
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
