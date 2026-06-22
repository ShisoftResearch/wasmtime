use crate::prelude::*;
#[cfg(test)]
use crate::runtime::transaction::PersistentObjectRefRaw;
use crate::runtime::transaction::PersistentRecoveredRecordLocation;
#[cfg(test)]
use crate::runtime::transaction::config::TMemoryRegionConfig;
use crate::runtime::transaction::type_layout::{
    PersistentTypeLayout, TypeLayoutId, TypeLayoutRegistry,
};
use crate::runtime::vm::block_region::{
    BlockRegionBackendView, DaxPmemBlockRegion, FileBackedMemoryBlockRegion, StreamCursor,
};
#[cfg(test)]
use crate::runtime::vm::unpack_object_granule_id;
use crate::runtime::vm::{
    PackedGranuleDomain, TMemory, TxLogEntry, TxLogEntryRole, pack_object_granule_id,
    pack_tmemory_size_logical_id,
};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[cfg(test)]
use super::persist_region_set::MultiRegionDurableLogBackend;

#[derive(Debug, Clone)]
pub(crate) struct PendingPublication {
    pub(crate) logical_id: u64,
    pub(crate) version: u32,
    pub(crate) kind: u16,
    pub(crate) type_layout_id: u32,
    pub(crate) payload: Vec<u8>,
}

pub(crate) fn encode_data_record(pub_: &PendingPublication) -> Result<Vec<u8>> {
    TMemory::encode_publication_data_record(
        pub_.logical_id,
        pub_.version,
        pub_.kind,
        pub_.type_layout_id,
        &pub_.payload,
    )
}

#[derive(Debug, Clone)]
pub(crate) struct PendingGranuleUndo {
    pub(crate) logical_id: u64,
    pub(crate) version: u32,
    pub(crate) kind: u16,
    pub(crate) type_info: u32,
    pub(crate) old_granule_bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct PendingCommitLogEntry {
    pub(crate) logical_id: u64,
    pub(crate) version: u32,
    pub(crate) chunk_start_block: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) data_block_generation: u32,
    pub(crate) role: TxLogEntryRole,
}

pub(crate) fn encode_undo_data_record(undo: &PendingGranuleUndo) -> Result<Vec<u8>> {
    TMemory::encode_granule_undo_data_record(
        undo.logical_id,
        undo.version,
        undo.kind,
        undo.type_info,
        &undo.old_granule_bytes,
    )
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum DurableDataStream {
    ObjectPublication,
    TMemoryUndo,
}

impl DurableDataStream {
    fn file_backed_stream_id(self, transaction_stream_id: u32) -> Result<u32> {
        const DATA_STREAM_ID_MASK: u32 = 0x3fff_ffff;
        const OBJECT_DATA_STREAM_TAG: u32 = 0x4000_0000;
        const TMEMORY_UNDO_DATA_STREAM_TAG: u32 = 0x8000_0000;

        ensure!(
            transaction_stream_id <= DATA_STREAM_ID_MASK,
            "transaction stream id is too large for file-backed role data stream"
        );

        Ok(match self {
            DurableDataStream::ObjectPublication => OBJECT_DATA_STREAM_TAG | transaction_stream_id,
            DurableDataStream::TMemoryUndo => TMEMORY_UNDO_DATA_STREAM_TAG | transaction_stream_id,
        })
    }
}

impl PendingPublication {
    pub(crate) fn tmemory_size(
        owner_instance: Option<u32>,
        memory_index: u32,
        version: u32,
        new_pages: u64,
    ) -> Result<Self> {
        Ok(Self {
            logical_id: pack_tmemory_size_logical_id(owner_instance, memory_index)?,
            version,
            kind: PackedGranuleDomain::TMemorySize as u16,
            type_layout_id: 0,
            payload: new_pages.to_le_bytes().to_vec(),
        })
    }

    fn persistent_root(
        domain: PackedGranuleDomain,
        root_index: u64,
        version: u32,
        roots: impl IntoIterator<Item = Option<u64>>,
    ) -> Result<Self> {
        ensure!(
            matches!(
                domain,
                PackedGranuleDomain::TGlobal | PackedGranuleDomain::TTable
            ),
            "persistent root publication domain must be TGlobal or TTable"
        );
        ensure!(
            root_index < (1u64 << 60),
            "persistent root index does not fit in packed granule id payload"
        );
        let logical_id = ((domain as u64) << 60) | root_index;
        let mut payload = Vec::new();
        for root in roots {
            let encoded = match root {
                Some(object_id) => object_id
                    .checked_add(1)
                    .context("persistent root object id encoding overflow")?,
                None => 0,
            };
            payload.extend_from_slice(&encoded.to_le_bytes());
        }
        Ok(Self {
            logical_id,
            version,
            kind: domain as u16,
            type_layout_id: 0,
            payload,
        })
    }

    pub(crate) fn persistent_global_root(
        global_index: u64,
        version: u32,
        roots: impl IntoIterator<Item = Option<u64>>,
    ) -> Result<Self> {
        Self::persistent_root(PackedGranuleDomain::TGlobal, global_index, version, roots)
    }

    pub(crate) fn persistent_table_root(
        table_index: u64,
        version: u32,
        roots: impl IntoIterator<Item = Option<u64>>,
    ) -> Result<Self> {
        Self::persistent_root(PackedGranuleDomain::TTable, table_index, version, roots)
    }

    pub(crate) fn persistent_object(
        domain: PackedGranuleDomain,
        object_id: u64,
        version: u32,
        type_layout_id: u32,
        payload: Vec<u8>,
    ) -> Result<Self> {
        ensure!(
            matches!(
                domain,
                PackedGranuleDomain::TStruct | PackedGranuleDomain::TArray
            ),
            "persistent object publication domain must be a durable object domain"
        );
        Ok(Self {
            logical_id: pack_object_granule_id(domain, object_id)?,
            version,
            kind: domain as u16,
            type_layout_id,
            payload,
        })
    }

    pub(crate) fn persistent_object_type_layout_id(&self) -> Result<Option<TypeLayoutId>> {
        if self.kind == PackedGranuleDomain::TStruct as u16
            || self.kind == PackedGranuleDomain::TArray as u16
        {
            return Ok(Some(TypeLayoutId::new(self.type_layout_id).context(
                "persistent object publication type layout id cannot be zero",
            )?));
        }
        Ok(None)
    }
}

impl PendingGranuleUndo {
    pub(crate) fn tmemory(logical_id: u64, version: u32, old_granule_bytes: Vec<u8>) -> Self {
        Self {
            logical_id,
            version,
            kind: PackedGranuleDomain::TMemory as u16,
            type_info: 0,
            old_granule_bytes,
        }
    }
}

#[cfg(test)]
impl PendingPublication {
    fn tmemory_for_test(logical_id: u64, version: u32, payload: &[u8]) -> Self {
        Self {
            logical_id,
            version,
            kind: 1,
            type_layout_id: 0,
            payload: payload.to_vec(),
        }
    }

    fn persistent_object_for_test(
        domain: PackedGranuleDomain,
        object_id: u64,
        version: u32,
        type_layout_id: u32,
        payload: &[u8],
    ) -> Self {
        Self::persistent_object(domain, object_id, version, type_layout_id, payload.to_vec())
            .unwrap()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DurableDataRecordPointer {
    pub(crate) chunk_start_block: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) data_block_generation: u32,
}

pub(crate) trait DurableSink {
    fn append_data_record(
        &mut self,
        record: &[u8],
        data_stream: DurableDataStream,
    ) -> Result<DurableDataRecordPointer>;
    fn append_log_entry(
        &mut self,
        logical_id: u64,
        version: u32,
        tx_meta: u32,
        data_block: u32,
        data_offset: u32,
        data_block_generation: u32,
        is_final: bool,
        role: TxLogEntryRole,
    ) -> Result<()>;
    fn mark_last_log_entry_committed(&mut self, marker: PendingCommitLogEntry) -> Result<()>;
    fn flush_data(&mut self) -> Result<()>;
    fn flush_log(&mut self) -> Result<()>;
    fn fence(&mut self) -> Result<()>;
    fn retire_committed_linear_undo_chunk(&mut self, _chunk_start_block: u32) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct TxDurableLog {
    storage: Box<dyn TxDurableLogBackend>,
}

/// Storage backend for the transaction-owned durable log.
///
/// `TxDurableLog` owns transaction ordering and commit protocol decisions; a
/// backend provides type-layout metadata persistence plus append, flush, and
/// fence primitives for log entries and role-specific data records. File-backed
/// storage is one implementation of this trait, not a separate commit path.
pub(crate) trait TxDurableLogBackend: core::fmt::Debug + Send + Sync {
    fn ensure_type_layout(&mut self, layout: &PersistentTypeLayout) -> Result<()>;
    fn has_type_layout(&self, id: TypeLayoutId) -> Result<bool>;
    fn append_data_record(
        &mut self,
        transaction_stream_id: u32,
        data_stream: DurableDataStream,
        record: &[u8],
    ) -> Result<DurableDataRecordPointer>;
    fn append_log_entry(&mut self, transaction_stream_id: u32, entry: TxLogEntry) -> Result<()>;
    fn mark_last_log_entry_committed(
        &mut self,
        transaction_stream_id: u32,
        marker: PendingCommitLogEntry,
    ) -> Result<()>;
    fn flush_data(&mut self) -> Result<()>;
    fn flush_log(&mut self) -> Result<()>;
    fn fence(&mut self) -> Result<()>;
    fn retire_committed_linear_undo_chunk(&mut self, _chunk_start_block: u32) -> Result<()> {
        Ok(())
    }
    fn with_recovered_region_snapshot(
        &self,
        f: &mut dyn FnMut(
            &crate::runtime::vm::RecoveredRegion,
            Option<Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>>,
        ) -> Result<()>,
    ) -> Result<bool> {
        let Some(recovered) = self.recover_region_snapshot()? else {
            return Ok(false);
        };
        f(&recovered, None)?;
        Ok(true)
    }
    // This returns recovered metadata and may carry a mapped source when the
    // backend supports one. Callers that need object record bytes must use a
    // mapped source from the returned region or `with_recovered_region_snapshot`.
    fn recover_region_snapshot(&self) -> Result<Option<crate::runtime::vm::RecoveredRegion>> {
        Ok(None)
    }
    fn object_data_chunk_start_for_block(&self, _data_block: u32) -> Result<Option<u32>> {
        Ok(None)
    }
    fn retire_whole_dead_object_chunks(
        &mut self,
        _reachable: &[PersistentRecoveredRecordLocation],
        _unreachable: &[PersistentRecoveredRecordLocation],
    ) -> Result<Vec<u32>> {
        Ok(Vec::new())
    }

    #[cfg(test)]
    fn log_entries_for_test(&self, stream_id: u32) -> Vec<TxLogEntry>;

    #[cfg(test)]
    fn latest_planned_recovery_worker_counts_for_test(&self) -> Vec<usize> {
        Vec::new()
    }

    #[cfg(test)]
    fn latest_multi_region_recovery_timing_for_test(
        &self,
    ) -> Option<MultiRegionRecoveryTimingForTest> {
        None
    }

    #[cfg(test)]
    fn region_count_for_test(&self) -> usize {
        1
    }

    #[cfg(test)]
    fn log_entries_for_region_for_test(
        &self,
        region_index: usize,
        stream_id: u32,
    ) -> Vec<TxLogEntry> {
        if region_index == 0 {
            return self.log_entries_for_test(stream_id);
        }
        Vec::new()
    }

    #[cfg(test)]
    fn has_type_layout_for_region_for_test(
        &self,
        region_index: usize,
        id: TypeLayoutId,
    ) -> Result<bool> {
        if region_index == 0 {
            return self.has_type_layout(id);
        }
        Ok(false)
    }

    #[cfg(test)]
    fn data_chunk_stream_id_for_data_block_for_test(&self, data_block: u32) -> Result<u32>;
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MultiRegionRecoveryTimingForTest {
    pub(crate) region_recovery_elapsed: Vec<std::time::Duration>,
    pub(crate) merge_elapsed: std::time::Duration,
    pub(crate) total_elapsed: std::time::Duration,
}

pub(super) trait DurableRegionStorage: core::fmt::Debug + Send + Sync {
    fn stream_cursor(&mut self, stream_id: u32) -> StreamCursor;
    fn refresh_from_image(&mut self) -> Result<()>;
    fn append_type_layout_metadata(&mut self, layout: &PersistentTypeLayout) -> Result<()>;
    fn load_type_layout_metadata(&self) -> Result<TypeLayoutRegistry>;
    fn append_data_record(
        &mut self,
        stream: StreamCursor,
        record: &[u8],
    ) -> Result<DurableDataRecordPointer>;
    fn append_data_record_requires_allocation(
        &self,
        stream: StreamCursor,
        record: &[u8],
    ) -> Result<bool>;
    fn append_log_entry(&mut self, stream: StreamCursor, entry: TxLogEntry) -> Result<u32>;
    fn append_log_entry_requires_allocation(&self, stream: StreamCursor) -> Result<bool>;
    fn mark_last_log_entry_committed(
        &mut self,
        stream: StreamCursor,
        marker: PendingCommitLogEntry,
    ) -> Result<u32>;
    fn flush_data_chunk(&self, chunk_start_block: u32) -> Result<()>;
    fn flush_log_block(&self, log_block: u32) -> Result<()>;
    fn fence(&self) -> Result<()>;
    fn retire_linear_undo_chunks<I>(&mut self, chunk_starts: I) -> Result<Vec<u32>>
    where
        I: IntoIterator<Item = u32>;
    fn recover_region_snapshot(&self) -> Result<crate::runtime::vm::RecoveredRegion>;
    fn recover_region_snapshot_with_options(
        &self,
        options: crate::runtime::vm::RecoveryOptions,
    ) -> Result<crate::runtime::vm::RecoveredRegion> {
        let _ = options;
        self.recover_region_snapshot()
    }
    fn object_data_chunk_start_for_block(&self, data_block: u32) -> Result<Option<u32>>;
    fn retire_whole_dead_object_chunks(
        &mut self,
        reachable: &[PersistentRecoveredRecordLocation],
        unreachable: &[PersistentRecoveredRecordLocation],
    ) -> Result<Vec<u32>>;
    fn view(&self) -> BlockRegionBackendView<'_>;
}

#[derive(Debug, Default)]
struct InMemoryTxDurableLog {
    streams: BTreeMap<u32, TxDurableStreamState>,
    type_layouts: TypeLayoutRegistry,
}

#[derive(Debug, Default)]
struct TxDurableStreamState {
    data_records: Vec<Vec<u8>>,
    log_entries: Vec<TxLogEntry>,
    next_data_block: u32,
}

#[derive(Debug)]
pub(super) struct DurableRegionLog<R> {
    region: R,
    streams: BTreeMap<u32, StreamCursor>,
    pending_data_chunks: BTreeSet<u32>,
    pending_log_blocks: BTreeSet<u32>,
    // Protects shared free-block allocation and region-metadata refresh across
    // independent file mappings. It is not a per-stream publication lock.
    shared_allocator_lock: Option<Arc<Mutex<()>>>,
}

pub(crate) struct TxDurableLogSink<'a> {
    log: &'a mut TxDurableLog,
    stream_id: u32,
}

impl Default for TxDurableLog {
    fn default() -> Self {
        Self::with_backend(InMemoryTxDurableLog::default())
    }
}

impl TxDurableLog {
    pub(crate) fn with_backend<B>(backend: B) -> Self
    where
        B: TxDurableLogBackend + 'static,
    {
        Self {
            storage: Box::new(backend),
        }
    }

    pub(crate) fn stream_sink(&mut self, stream_id: u32) -> TxDurableLogSink<'_> {
        TxDurableLogSink {
            log: self,
            stream_id,
        }
    }

    pub(crate) fn retire_committed_linear_undo_chunks<I>(&mut self, chunk_starts: I) -> Result<()>
    where
        I: IntoIterator<Item = u32>,
    {
        for chunk_start_block in chunk_starts {
            self.storage
                .retire_committed_linear_undo_chunk(chunk_start_block)?;
        }
        Ok(())
    }

    // This returns recovered metadata and may carry a mapped source when the
    // backend supports one. Callers that need object record bytes must use a
    // mapped source from the returned region or `with_recovered_region_snapshot`.
    pub(crate) fn recover_region_snapshot(
        &self,
    ) -> Result<Option<crate::runtime::vm::RecoveredRegion>> {
        self.storage.recover_region_snapshot()
    }

    pub(crate) fn with_recovered_region_snapshot(
        &self,
        f: &mut dyn FnMut(
            &crate::runtime::vm::RecoveredRegion,
            Option<Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>>,
        ) -> Result<()>,
    ) -> Result<bool> {
        self.storage.with_recovered_region_snapshot(f)
    }

    pub(crate) fn object_data_chunk_start_for_block(&self, data_block: u32) -> Result<Option<u32>> {
        self.storage.object_data_chunk_start_for_block(data_block)
    }

    pub(crate) fn retire_whole_dead_object_chunks(
        &mut self,
        reachable: &[PersistentRecoveredRecordLocation],
        unreachable: &[PersistentRecoveredRecordLocation],
    ) -> Result<Vec<u32>> {
        self.storage
            .retire_whole_dead_object_chunks(reachable, unreachable)
    }

    pub(crate) fn ensure_type_layout(&mut self, layout: &PersistentTypeLayout) -> Result<()> {
        self.storage.ensure_type_layout(layout)
    }

    pub(crate) fn has_type_layout(&self, id: TypeLayoutId) -> Result<bool> {
        self.storage.has_type_layout(id)
    }

    #[cfg(test)]
    pub(crate) fn log_entries_for_test(&self, stream_id: u32) -> Vec<TxLogEntry> {
        self.storage.log_entries_for_test(stream_id)
    }

    #[cfg(test)]
    pub(crate) fn latest_planned_recovery_worker_counts_for_test(&self) -> Vec<usize> {
        self.storage
            .latest_planned_recovery_worker_counts_for_test()
    }

    #[cfg(test)]
    pub(crate) fn latest_multi_region_recovery_timing_for_test(
        &self,
    ) -> Option<MultiRegionRecoveryTimingForTest> {
        self.storage
            .latest_multi_region_recovery_timing_for_test()
    }

    #[cfg(test)]
    pub(crate) fn region_count_for_test(&self) -> usize {
        self.storage.region_count_for_test()
    }

    #[cfg(test)]
    pub(crate) fn log_entries_for_region_for_test(
        &self,
        region_index: usize,
        stream_id: u32,
    ) -> Vec<TxLogEntry> {
        self.storage
            .log_entries_for_region_for_test(region_index, stream_id)
    }

    #[cfg(test)]
    pub(crate) fn has_type_layout_for_region_for_test(
        &self,
        region_index: usize,
        id: TypeLayoutId,
    ) -> Result<bool> {
        self.storage
            .has_type_layout_for_region_for_test(region_index, id)
    }

    #[cfg(test)]
    pub(crate) fn data_chunk_stream_id_for_data_block_for_test(
        &self,
        data_block: u32,
    ) -> Result<u32> {
        self.storage
            .data_chunk_stream_id_for_data_block_for_test(data_block)
    }

    pub(crate) fn create_file_backed(path: &Path, num_blocks: u32) -> Result<Self> {
        Self::create_file_backed_with_shared_allocator_lock(path, num_blocks, None)
    }

    pub(crate) fn create_file_backed_with_allocator_lock(
        path: &Path,
        num_blocks: u32,
        shared_allocator_lock: Arc<Mutex<()>>,
    ) -> Result<Self> {
        Self::create_file_backed_with_shared_allocator_lock(
            path,
            num_blocks,
            Some(shared_allocator_lock),
        )
    }

    fn create_file_backed_with_shared_allocator_lock(
        path: &Path,
        num_blocks: u32,
        shared_allocator_lock: Option<Arc<Mutex<()>>>,
    ) -> Result<Self> {
        let region = FileBackedMemoryBlockRegion::create_for_test(path, num_blocks)?;
        Ok(Self::from_file_backed_region(region, shared_allocator_lock))
    }

    pub(crate) fn open_file_backed(path: &Path) -> Result<Self> {
        Self::open_file_backed_with_shared_allocator_lock(path, None)
    }

    pub(crate) fn open_file_backed_with_allocator_lock(
        path: &Path,
        shared_allocator_lock: Arc<Mutex<()>>,
    ) -> Result<Self> {
        Self::open_file_backed_with_shared_allocator_lock(path, Some(shared_allocator_lock))
    }

    fn open_file_backed_with_shared_allocator_lock(
        path: &Path,
        shared_allocator_lock: Option<Arc<Mutex<()>>>,
    ) -> Result<Self> {
        if let Some(lock) = shared_allocator_lock.clone() {
            let _guard = lock
                .lock()
                .map_err(|_| crate::format_err!("shared durable log allocator lock poisoned"))?;
            let mut region = FileBackedMemoryBlockRegion::open_for_test(path)?;
            crate::runtime::vm::block_region::retire_completed_linear_undo_chunks(&mut region)?;
            return Ok(Self::from_file_backed_region(region, shared_allocator_lock));
        }
        let mut region = FileBackedMemoryBlockRegion::open_for_test(path)?;
        crate::runtime::vm::block_region::retire_completed_linear_undo_chunks(&mut region)?;
        Ok(Self::from_file_backed_region(region, shared_allocator_lock))
    }

    fn from_file_backed_region(
        region: FileBackedMemoryBlockRegion,
        shared_allocator_lock: Option<Arc<Mutex<()>>>,
    ) -> Self {
        Self::with_backend(DurableRegionLog::new(region, shared_allocator_lock))
    }

    #[cfg(test)]
    pub(crate) fn recover_file_backed_for_test(
        path: &Path,
    ) -> Result<crate::runtime::vm::block_region::TransactionPersistenceRecoveredRegion> {
        crate::runtime::vm::block_region::reopen_and_recover_file_backed_region(path)
    }

    #[cfg(test)]
    pub(crate) fn create_dax_pmem_research_for_test(payload_blocks: usize) -> Result<Self> {
        let region = DaxPmemBlockRegion::new_for_test(payload_blocks)?;
        Ok(Self::with_backend(DurableRegionLog::new(region, None)))
    }

    #[cfg(test)]
    pub(crate) fn create_dax_pmem_fsdax_for_test(
        path: &Path,
        payload_blocks: usize,
    ) -> Result<Self> {
        let region = DaxPmemBlockRegion::create_fsdax_path(path.to_path_buf(), payload_blocks)?;
        Ok(Self::with_backend(DurableRegionLog::new(region, None)))
    }

    #[cfg(test)]
    pub(crate) fn open_dax_pmem_fsdax_for_test(path: &Path, payload_blocks: usize) -> Result<Self> {
        let region = DaxPmemBlockRegion::open_fsdax_path(path.to_path_buf(), payload_blocks)?;
        Ok(Self::with_backend(DurableRegionLog::new(region, None)))
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn create_dax_pmem_fsdax_regions_for_test(
        regions: Vec<TMemoryRegionConfig>,
    ) -> Result<Self> {
        Ok(Self::with_backend(
            MultiRegionDurableLogBackend::create_fsdax_regions_for_test(regions)?,
        ))
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn open_dax_pmem_fsdax_regions_for_test(
        regions: Vec<TMemoryRegionConfig>,
    ) -> Result<Self> {
        Ok(Self::with_backend(
            MultiRegionDurableLogBackend::open_fsdax_regions_for_test(regions)?,
        ))
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn create_file_backed_regions_for_test(
        regions: Vec<TMemoryRegionConfig>,
    ) -> Result<Self> {
        Ok(Self::with_backend(
            MultiRegionDurableLogBackend::create_file_backed_regions_for_test(regions)?,
        ))
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn open_file_backed_regions_for_test(
        regions: Vec<TMemoryRegionConfig>,
    ) -> Result<Self> {
        Ok(Self::with_backend(
            MultiRegionDurableLogBackend::open_file_backed_regions_for_test(regions)?,
        ))
    }

    #[cfg(test)]
    pub(crate) fn recording_backend_for_test() -> (Self, Arc<Mutex<Vec<RecordingBackendEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self::with_backend(RecordingTxDurableLogBackend::new(events.clone())),
            events,
        )
    }

    #[cfg(test)]
    pub(crate) fn recording_backend_with_retire_failure_for_test()
    -> (Self, Arc<Mutex<Vec<RecordingBackendEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self::with_backend(RecordingTxDurableLogBackend::new_with_retire_failure(
                events.clone(),
            )),
            events,
        )
    }

    #[cfg(test)]
    pub(crate) fn recording_backend_with_retire_failures_for_test(
        failures_remaining: u32,
    ) -> (Self, Arc<Mutex<Vec<RecordingBackendEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self::with_backend(RecordingTxDurableLogBackend::new_with_retire_failures(
                events.clone(),
                failures_remaining,
            )),
            events,
        )
    }

    #[cfg(test)]
    pub(crate) fn recording_backend_with_flush_log_failure_for_test()
    -> (Self, Arc<Mutex<Vec<RecordingBackendEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self::with_backend(RecordingTxDurableLogBackend::new_with_flush_log_failure(
                events.clone(),
            )),
            events,
        )
    }
}

impl TxDurableLogBackend for InMemoryTxDurableLog {
    fn ensure_type_layout(&mut self, layout: &PersistentTypeLayout) -> Result<()> {
        self.type_layouts.insert(layout.clone())
    }

    fn has_type_layout(&self, id: TypeLayoutId) -> Result<bool> {
        Ok(self.type_layouts.contains(id))
    }

    fn append_data_record(
        &mut self,
        transaction_stream_id: u32,
        _data_stream: DurableDataStream,
        record: &[u8],
    ) -> Result<DurableDataRecordPointer> {
        let state = self.streams.entry(transaction_stream_id).or_default();
        state.data_records.push(record.to_vec());
        let data_block = state.next_data_block;
        state.next_data_block = state
            .next_data_block
            .checked_add(1)
            .context("transaction durable data block overflow")?;
        Ok(DurableDataRecordPointer {
            chunk_start_block: data_block,
            data_block,
            data_offset: 0,
            data_block_generation: 0,
        })
    }

    fn append_log_entry(&mut self, transaction_stream_id: u32, entry: TxLogEntry) -> Result<()> {
        let state = self.streams.entry(transaction_stream_id).or_default();
        state.log_entries.push(entry);
        Ok(())
    }

    fn mark_last_log_entry_committed(
        &mut self,
        transaction_stream_id: u32,
        marker: PendingCommitLogEntry,
    ) -> Result<()> {
        let state = self
            .streams
            .get_mut(&transaction_stream_id)
            .with_context(|| format!("transaction stream {transaction_stream_id} has no log"))?;
        let entry = state
            .log_entries
            .last_mut()
            .context("transaction commit LP requires a log entry")?;
        ensure!(
            entry.logical_id == marker.logical_id
                && entry.version == marker.version
                && entry.data_block == marker.data_block
                && entry.data_offset == marker.data_offset
                && entry.data_block_generation() == marker.data_block_generation
                && entry.role()? == marker.role,
            "transactional commit LP marker does not match the last log entry"
        );
        entry.tx_meta |= 1;
        Ok(())
    }

    fn flush_data(&mut self) -> Result<()> {
        Ok(())
    }

    fn flush_log(&mut self) -> Result<()> {
        Ok(())
    }

    fn fence(&mut self) -> Result<()> {
        Ok(())
    }

    #[cfg(test)]
    fn log_entries_for_test(&self, stream_id: u32) -> Vec<TxLogEntry> {
        self.streams
            .get(&stream_id)
            .map(|stream| stream.log_entries.clone())
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn data_chunk_stream_id_for_data_block_for_test(&self, _data_block: u32) -> Result<u32> {
        bail!("in-memory transaction log has no file-backed data chunk headers")
    }
}

impl DurableRegionStorage for FileBackedMemoryBlockRegion {
    fn stream_cursor(&mut self, stream_id: u32) -> StreamCursor {
        FileBackedMemoryBlockRegion::stream_cursor(self, stream_id)
    }

    fn refresh_from_image(&mut self) -> Result<()> {
        FileBackedMemoryBlockRegion::refresh_from_image(self)
    }

    fn append_type_layout_metadata(&mut self, layout: &PersistentTypeLayout) -> Result<()> {
        FileBackedMemoryBlockRegion::append_type_layout_metadata(self, layout)
    }

    fn load_type_layout_metadata(&self) -> Result<TypeLayoutRegistry> {
        FileBackedMemoryBlockRegion::load_type_layout_metadata(self)
    }

    fn append_data_record(
        &mut self,
        stream: StreamCursor,
        record: &[u8],
    ) -> Result<DurableDataRecordPointer> {
        let location = FileBackedMemoryBlockRegion::append_data_record(self, stream, record)?;
        Ok(DurableDataRecordPointer {
            chunk_start_block: location.chunk_start_block,
            data_block: location.data_block,
            data_offset: location.data_offset,
            data_block_generation: FileBackedMemoryBlockRegion::block_generation(
                self,
                location.data_block,
            )?,
        })
    }

    fn append_data_record_requires_allocation(
        &self,
        stream: StreamCursor,
        record: &[u8],
    ) -> Result<bool> {
        FileBackedMemoryBlockRegion::append_data_record_requires_allocation(self, stream, record)
    }

    fn append_log_entry(&mut self, stream: StreamCursor, entry: TxLogEntry) -> Result<u32> {
        FileBackedMemoryBlockRegion::append_log_entry(self, stream, entry)
    }

    fn append_log_entry_requires_allocation(&self, stream: StreamCursor) -> Result<bool> {
        FileBackedMemoryBlockRegion::append_log_entry_requires_allocation(self, stream)
    }

    fn mark_last_log_entry_committed(
        &mut self,
        stream: StreamCursor,
        marker: PendingCommitLogEntry,
    ) -> Result<u32> {
        FileBackedMemoryBlockRegion::mark_last_log_entry_committed(
            self,
            stream,
            marker.logical_id,
            marker.version,
            marker.data_block,
            marker.data_offset,
            marker.data_block_generation,
            marker.role,
        )
    }

    fn flush_data_chunk(&self, chunk_start_block: u32) -> Result<()> {
        FileBackedMemoryBlockRegion::flush_data_chunk(self, chunk_start_block)
    }

    fn flush_log_block(&self, log_block: u32) -> Result<()> {
        FileBackedMemoryBlockRegion::flush_log_block(self, log_block)
    }

    fn fence(&self) -> Result<()> {
        FileBackedMemoryBlockRegion::fence(self)
    }

    fn retire_linear_undo_chunks<I>(&mut self, chunk_starts: I) -> Result<Vec<u32>>
    where
        I: IntoIterator<Item = u32>,
    {
        FileBackedMemoryBlockRegion::retire_linear_undo_chunks(self, chunk_starts)
    }

    fn recover_region_snapshot(&self) -> Result<crate::runtime::vm::RecoveredRegion> {
        crate::runtime::vm::block_region::recover_file_backed_region_snapshot(self)
    }

    fn recover_region_snapshot_with_options(
        &self,
        options: crate::runtime::vm::RecoveryOptions,
    ) -> Result<crate::runtime::vm::RecoveredRegion> {
        crate::runtime::vm::block_region::recover_file_backed_region_snapshot_with_options(
            self, options,
        )
    }

    fn object_data_chunk_start_for_block(&self, data_block: u32) -> Result<Option<u32>> {
        FileBackedMemoryBlockRegion::object_data_chunk_start_for_block(self, data_block)
    }

    fn retire_whole_dead_object_chunks(
        &mut self,
        reachable: &[PersistentRecoveredRecordLocation],
        unreachable: &[PersistentRecoveredRecordLocation],
    ) -> Result<Vec<u32>> {
        FileBackedMemoryBlockRegion::retire_whole_dead_object_chunks(self, reachable, unreachable)
    }

    fn view(&self) -> BlockRegionBackendView<'_> {
        FileBackedMemoryBlockRegion::view(self)
    }
}

impl DurableRegionStorage for DaxPmemBlockRegion {
    fn stream_cursor(&mut self, stream_id: u32) -> StreamCursor {
        DaxPmemBlockRegion::stream_cursor(self, stream_id)
    }

    fn refresh_from_image(&mut self) -> Result<()> {
        DaxPmemBlockRegion::refresh_from_image(self)
    }

    fn append_type_layout_metadata(&mut self, layout: &PersistentTypeLayout) -> Result<()> {
        DaxPmemBlockRegion::append_type_layout_metadata(self, layout)
    }

    fn load_type_layout_metadata(&self) -> Result<TypeLayoutRegistry> {
        DaxPmemBlockRegion::load_type_layout_metadata(self)
    }

    fn append_data_record(
        &mut self,
        stream: StreamCursor,
        record: &[u8],
    ) -> Result<DurableDataRecordPointer> {
        let location = DaxPmemBlockRegion::append_data_record(self, stream, record)?;
        Ok(DurableDataRecordPointer {
            chunk_start_block: location.chunk_start_block,
            data_block: location.data_block,
            data_offset: location.data_offset,
            data_block_generation: DaxPmemBlockRegion::block_generation(self, location.data_block)?,
        })
    }

    fn append_data_record_requires_allocation(
        &self,
        stream: StreamCursor,
        record: &[u8],
    ) -> Result<bool> {
        DaxPmemBlockRegion::append_data_record_requires_allocation(self, stream, record)
    }

    fn append_log_entry(&mut self, stream: StreamCursor, entry: TxLogEntry) -> Result<u32> {
        DaxPmemBlockRegion::append_log_entry(self, stream, entry)
    }

    fn append_log_entry_requires_allocation(&self, stream: StreamCursor) -> Result<bool> {
        DaxPmemBlockRegion::append_log_entry_requires_allocation(self, stream)
    }

    fn mark_last_log_entry_committed(
        &mut self,
        stream: StreamCursor,
        marker: PendingCommitLogEntry,
    ) -> Result<u32> {
        DaxPmemBlockRegion::mark_last_log_entry_committed(
            self,
            stream,
            marker.logical_id,
            marker.version,
            marker.data_block,
            marker.data_offset,
            marker.data_block_generation,
            marker.role,
        )
    }

    fn flush_data_chunk(&self, chunk_start_block: u32) -> Result<()> {
        DaxPmemBlockRegion::flush_data_chunk(self, chunk_start_block)
    }

    fn flush_log_block(&self, log_block: u32) -> Result<()> {
        DaxPmemBlockRegion::flush_log_block(self, log_block)
    }

    fn fence(&self) -> Result<()> {
        DaxPmemBlockRegion::fence(self)
    }

    fn retire_linear_undo_chunks<I>(&mut self, chunk_starts: I) -> Result<Vec<u32>>
    where
        I: IntoIterator<Item = u32>,
    {
        DaxPmemBlockRegion::retire_linear_undo_chunks(self, chunk_starts)
    }

    fn recover_region_snapshot(&self) -> Result<crate::runtime::vm::RecoveredRegion> {
        crate::runtime::vm::block_region::recover_dax_pmem_region_snapshot(self)
    }

    fn recover_region_snapshot_with_options(
        &self,
        options: crate::runtime::vm::RecoveryOptions,
    ) -> Result<crate::runtime::vm::RecoveredRegion> {
        crate::runtime::vm::block_region::recover_dax_pmem_region_snapshot_with_options(
            self, options,
        )
    }

    fn object_data_chunk_start_for_block(&self, data_block: u32) -> Result<Option<u32>> {
        DaxPmemBlockRegion::object_data_chunk_start_for_block(self, data_block)
    }

    fn retire_whole_dead_object_chunks(
        &mut self,
        reachable: &[PersistentRecoveredRecordLocation],
        unreachable: &[PersistentRecoveredRecordLocation],
    ) -> Result<Vec<u32>> {
        DaxPmemBlockRegion::retire_whole_dead_object_chunks(self, reachable, unreachable)
    }

    fn view(&self) -> BlockRegionBackendView<'_> {
        DaxPmemBlockRegion::view(self)
    }
}

impl<R> DurableRegionLog<R>
where
    R: DurableRegionStorage,
{
    pub(super) fn new(region: R, shared_allocator_lock: Option<Arc<Mutex<()>>>) -> Self {
        Self {
            region,
            streams: BTreeMap::new(),
            pending_data_chunks: BTreeSet::new(),
            pending_log_blocks: BTreeSet::new(),
            shared_allocator_lock,
        }
    }

    fn stream_cursor(&mut self, stream_id: u32) -> Result<StreamCursor> {
        let stream = self.region.stream_cursor(stream_id);
        self.streams.insert(stream_id, stream);
        Ok(stream)
    }

    fn with_shared_allocator_lock<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let shared_allocator_lock = self.shared_allocator_lock.clone();
        if let Some(lock) = shared_allocator_lock {
            let _guard = lock
                .lock()
                .map_err(|_| crate::format_err!("shared durable log allocator lock poisoned"))?;
            self.region.refresh_from_image()?;
            return f(self);
        }
        f(self)
    }

    pub(super) fn mapped_len_bytes(&self) -> usize {
        self.region.view().bytes_len()
    }

    pub(super) fn mapped_block_len(&self) -> Result<u32> {
        let blocks = self.mapped_len_bytes() / crate::runtime::vm::block_region::BLOCK_SIZE;
        u32::try_from(blocks).context("durable region block count overflow")
    }

    pub(super) fn recovered_region_input(
        &self,
    ) -> Result<crate::runtime::vm::RecoveredRegionInput> {
        self.recovered_region_input_with_options(crate::runtime::vm::RecoveryOptions::default())
    }

    pub(super) fn recovered_region_input_with_options(
        &self,
        options: crate::runtime::vm::RecoveryOptions,
    ) -> Result<crate::runtime::vm::RecoveredRegionInput> {
        let recovered = self.region.recover_region_snapshot_with_options(options)?;
        let view = self.region.view();
        let mapped_source = view
            .mapped_region_source()
            .context("durable region recovery requires mapped source")?;
        Ok(crate::runtime::vm::RecoveredRegionInput {
            recovered,
            mapped_source,
            mapped_len: view.bytes_len(),
        })
    }
}

impl<R> TxDurableLogBackend for DurableRegionLog<R>
where
    R: DurableRegionStorage,
{
    fn ensure_type_layout(&mut self, layout: &PersistentTypeLayout) -> Result<()> {
        self.with_shared_allocator_lock(|this| this.region.append_type_layout_metadata(layout))
    }

    fn has_type_layout(&self, id: TypeLayoutId) -> Result<bool> {
        Ok(self.region.load_type_layout_metadata()?.contains(id))
    }

    fn append_data_record(
        &mut self,
        transaction_stream_id: u32,
        data_stream: DurableDataStream,
        record: &[u8],
    ) -> Result<DurableDataRecordPointer> {
        let data_stream_id = data_stream.file_backed_stream_id(transaction_stream_id)?;
        let stream = self.stream_cursor(data_stream_id)?;
        let append = |this: &mut Self| {
            let stream = this.stream_cursor(data_stream_id)?;
            let pointer = this.region.append_data_record(stream, record)?;
            this.pending_data_chunks.insert(pointer.chunk_start_block);
            Ok(pointer)
        };
        if self
            .region
            .append_data_record_requires_allocation(stream, record)?
        {
            return self.with_shared_allocator_lock(append);
        }
        append(self)
    }

    fn append_log_entry(&mut self, transaction_stream_id: u32, entry: TxLogEntry) -> Result<()> {
        let stream = self.stream_cursor(transaction_stream_id)?;
        let append = |this: &mut Self| {
            let stream = this.stream_cursor(transaction_stream_id)?;
            let log_block = this.region.append_log_entry(stream, entry)?;
            this.pending_log_blocks.insert(log_block);
            Ok(())
        };
        if self.region.append_log_entry_requires_allocation(stream)? {
            return self.with_shared_allocator_lock(append);
        }
        append(self)
    }

    fn mark_last_log_entry_committed(
        &mut self,
        transaction_stream_id: u32,
        marker: PendingCommitLogEntry,
    ) -> Result<()> {
        let stream = self.stream_cursor(transaction_stream_id)?;
        let log_block = self.region.mark_last_log_entry_committed(stream, marker)?;
        self.pending_log_blocks.insert(log_block);
        Ok(())
    }

    fn flush_data(&mut self) -> Result<()> {
        for chunk in core::mem::take(&mut self.pending_data_chunks) {
            self.region.flush_data_chunk(chunk)?;
        }
        Ok(())
    }

    fn flush_log(&mut self) -> Result<()> {
        for block in core::mem::take(&mut self.pending_log_blocks) {
            self.region.flush_log_block(block)?;
        }
        Ok(())
    }

    fn fence(&mut self) -> Result<()> {
        self.region.fence()
    }

    fn retire_committed_linear_undo_chunk(&mut self, chunk_start_block: u32) -> Result<()> {
        self.region
            .retire_linear_undo_chunks(core::iter::once(chunk_start_block))?;
        Ok(())
    }

    fn with_recovered_region_snapshot(
        &self,
        f: &mut dyn FnMut(
            &crate::runtime::vm::RecoveredRegion,
            Option<Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>>,
        ) -> Result<()>,
    ) -> Result<bool> {
        let input = self.recovered_region_input()?;
        f(&input.recovered, Some(input.mapped_source))?;
        Ok(true)
    }

    fn recover_region_snapshot(&self) -> Result<Option<crate::runtime::vm::RecoveredRegion>> {
        Ok(Some(self.region.recover_region_snapshot()?))
    }

    fn object_data_chunk_start_for_block(&self, data_block: u32) -> Result<Option<u32>> {
        self.region.object_data_chunk_start_for_block(data_block)
    }

    fn retire_whole_dead_object_chunks(
        &mut self,
        reachable: &[PersistentRecoveredRecordLocation],
        unreachable: &[PersistentRecoveredRecordLocation],
    ) -> Result<Vec<u32>> {
        self.region
            .retire_whole_dead_object_chunks(reachable, unreachable)
    }

    #[cfg(test)]
    fn log_entries_for_test(&self, stream_id: u32) -> Vec<TxLogEntry> {
        let mut entries = Vec::new();
        let view = self.region.view();
        for block in 1..view.num_blocks() {
            let Ok(block) = u32::try_from(block) else {
                break;
            };
            let Ok(header) = view.log_block_header(block) else {
                continue;
            };
            if header.stream_id == stream_id
                && let Ok(block_entries) = view.log_block_entries(block)
            {
                entries.extend(block_entries);
            }
        }
        entries
    }

    #[cfg(test)]
    fn data_chunk_stream_id_for_data_block_for_test(&self, data_block: u32) -> Result<u32> {
        let view = self.region.view();
        for block in 1..view.num_blocks() {
            let chunk_start =
                u32::try_from(block).context("transaction data block index overflow")?;
            let Ok(Some(header)) = view.valid_data_chunk_header(chunk_start) else {
                continue;
            };
            let chunk_blocks = header.chunk_blocks;
            let chunk_end = chunk_start
                .checked_add(chunk_blocks)
                .context("transaction data chunk range overflow")?;
            if chunk_start <= data_block && data_block < chunk_end {
                return Ok(header.stream_id);
            }
        }
        bail!("transaction data block {data_block} is not inside a data chunk")
    }
}

impl DurableSink for TxDurableLogSink<'_> {
    fn append_data_record(
        &mut self,
        record: &[u8],
        data_stream: DurableDataStream,
    ) -> Result<DurableDataRecordPointer> {
        self.log
            .storage
            .append_data_record(self.stream_id, data_stream, record)
    }

    fn append_log_entry(
        &mut self,
        logical_id: u64,
        version: u32,
        tx_meta: u32,
        data_block: u32,
        data_offset: u32,
        data_block_generation: u32,
        is_final: bool,
        role: TxLogEntryRole,
    ) -> Result<()> {
        let mut entry = TMemory::publication_log_entry(
            logical_id,
            version,
            tx_meta,
            data_block,
            data_offset,
            data_block_generation,
            is_final,
        )?;
        entry.set_role(role);
        entry.seal_crc32();
        self.log.storage.append_log_entry(self.stream_id, entry)
    }

    fn mark_last_log_entry_committed(&mut self, marker: PendingCommitLogEntry) -> Result<()> {
        self.log
            .storage
            .mark_last_log_entry_committed(self.stream_id, marker)
    }

    fn flush_data(&mut self) -> Result<()> {
        self.log.storage.flush_data()
    }

    fn flush_log(&mut self) -> Result<()> {
        self.log.storage.flush_log()
    }

    fn fence(&mut self) -> Result<()> {
        self.log.storage.fence()
    }

    fn retire_committed_linear_undo_chunk(&mut self, chunk_start_block: u32) -> Result<()> {
        self.log
            .storage
            .retire_committed_linear_undo_chunk(chunk_start_block)
    }
}

pub(crate) struct StreamPublisher<'a, S> {
    sink: &'a mut S,
    stream_id: u32,
    txid: u32,
}

impl<'a, S> StreamPublisher<'a, S>
where
    S: DurableSink,
{
    pub(crate) fn new(sink: &'a mut S, stream_id: u32, txid: u32) -> Self {
        Self {
            sink,
            stream_id,
            txid,
        }
    }

    pub(crate) fn publish_tmemory_undo_before_in_place_write(
        &mut self,
        undo: &PendingGranuleUndo,
    ) -> Result<PendingCommitLogEntry> {
        let _ = self.stream_id;

        let record = encode_undo_data_record(undo)?;
        let pointer = self
            .sink
            .append_data_record(&record, DurableDataStream::TMemoryUndo)?;

        self.sink.flush_data()?;
        self.sink.fence()?;

        self.sink.append_log_entry(
            undo.logical_id,
            undo.version,
            self.txid << 1,
            pointer.data_block,
            pointer.data_offset,
            pointer.data_block_generation,
            false,
            TxLogEntryRole::TMemoryUndo,
        )?;
        self.sink.flush_log()?;
        self.sink.fence()?;

        Ok(PendingCommitLogEntry {
            logical_id: undo.logical_id,
            version: undo.version,
            chunk_start_block: pointer.chunk_start_block,
            data_block: pointer.data_block,
            data_offset: pointer.data_offset,
            data_block_generation: pointer.data_block_generation,
            role: TxLogEntryRole::TMemoryUndo,
        })
    }

    pub(crate) fn publish_object_publication_before_commit(
        &mut self,
        pub_: &PendingPublication,
    ) -> Result<PendingCommitLogEntry> {
        let _ = self.stream_id;

        let record = encode_data_record(pub_)?;
        let pointer = self
            .sink
            .append_data_record(&record, DurableDataStream::ObjectPublication)?;

        self.sink.flush_data()?;
        self.sink.fence()?;

        self.sink.append_log_entry(
            pub_.logical_id,
            pub_.version,
            self.txid << 1,
            pointer.data_block,
            pointer.data_offset,
            pointer.data_block_generation,
            false,
            TxLogEntryRole::TObjectPub,
        )?;
        self.sink.flush_log()?;
        self.sink.fence()?;

        Ok(PendingCommitLogEntry {
            logical_id: pub_.logical_id,
            version: pub_.version,
            chunk_start_block: pointer.chunk_start_block,
            data_block: pointer.data_block,
            data_offset: pointer.data_offset,
            data_block_generation: pointer.data_block_generation,
            role: TxLogEntryRole::TObjectPub,
        })
    }

    pub(crate) fn publish_object_publications_before_commit(
        &mut self,
        publications: &[PendingPublication],
    ) -> Result<Option<PendingCommitLogEntry>> {
        let _ = self.stream_id;
        if publications.is_empty() {
            return Ok(None);
        }

        let mut records = Vec::with_capacity(publications.len());
        for publication in publications {
            let record = encode_data_record(publication)?;
            let pointer = self
                .sink
                .append_data_record(&record, DurableDataStream::ObjectPublication)?;
            records.push((publication, pointer));
        }

        self.sink.flush_data()?;
        self.sink.fence()?;

        let mut final_marker = None;
        for (publication, pointer) in records {
            self.sink.append_log_entry(
                publication.logical_id,
                publication.version,
                self.txid << 1,
                pointer.data_block,
                pointer.data_offset,
                pointer.data_block_generation,
                false,
                TxLogEntryRole::TObjectPub,
            )?;
            final_marker = Some(PendingCommitLogEntry {
                logical_id: publication.logical_id,
                version: publication.version,
                chunk_start_block: pointer.chunk_start_block,
                data_block: pointer.data_block,
                data_offset: pointer.data_offset,
                data_block_generation: pointer.data_block_generation,
                role: TxLogEntryRole::TObjectPub,
            });
        }
        self.sink.flush_log()?;

        Ok(final_marker)
    }

    pub(crate) fn publish_commit_lp(&mut self, marker: PendingCommitLogEntry) -> Result<()> {
        self.sink.mark_last_log_entry_committed(marker)?;
        self.sink.flush_log()?;
        self.sink.fence()?;
        if marker.role == TxLogEntryRole::TMemoryUndo {
            let _ = self
                .sink
                .retire_committed_linear_undo_chunk(marker.chunk_start_block);
        }

        Ok(())
    }

    pub(crate) fn abort_transaction(&mut self, _pubs: &[PendingPublication]) -> Result<()> {
        let _ = self.stream_id;
        Ok(())
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecordingBackendEvent {
    EnsureTypeLayout(u32),
    AppendDataRecord(DurableDataStream),
    AppendLogEntry(TxLogEntryRole),
    MarkLogEntryCommitted(TxLogEntryRole),
    FlushData,
    FlushLog,
    Fence,
    RetireCommittedLinearUndoChunk(u32),
    RetireCommittedLinearUndoChunkFailed(u32),
}

#[cfg(test)]
#[derive(Debug)]
struct RecordingTxDurableLogBackend {
    events: Arc<Mutex<Vec<RecordingBackendEvent>>>,
    next_data_block: u32,
    type_layouts: TypeLayoutRegistry,
    retire_committed_linear_undo_failures_remaining: u32,
    fail_flush_log_once: bool,
}

#[cfg(test)]
impl RecordingTxDurableLogBackend {
    fn new(events: Arc<Mutex<Vec<RecordingBackendEvent>>>) -> Self {
        Self {
            events,
            next_data_block: 0,
            type_layouts: TypeLayoutRegistry::default(),
            retire_committed_linear_undo_failures_remaining: 0,
            fail_flush_log_once: false,
        }
    }

    fn new_with_retire_failure(events: Arc<Mutex<Vec<RecordingBackendEvent>>>) -> Self {
        Self::new_with_retire_failures(events, u32::MAX)
    }

    fn new_with_retire_failures(
        events: Arc<Mutex<Vec<RecordingBackendEvent>>>,
        failures_remaining: u32,
    ) -> Self {
        Self {
            retire_committed_linear_undo_failures_remaining: failures_remaining,
            ..Self::new(events)
        }
    }

    fn new_with_flush_log_failure(events: Arc<Mutex<Vec<RecordingBackendEvent>>>) -> Self {
        Self {
            fail_flush_log_once: true,
            ..Self::new(events)
        }
    }

    fn push_event(&self, event: RecordingBackendEvent) {
        self.events.lock().unwrap().push(event);
    }
}

#[cfg(test)]
impl TxDurableLogBackend for RecordingTxDurableLogBackend {
    fn ensure_type_layout(&mut self, layout: &PersistentTypeLayout) -> Result<()> {
        self.type_layouts.insert(layout.clone())?;
        self.push_event(RecordingBackendEvent::EnsureTypeLayout(layout.id().get()));
        Ok(())
    }

    fn has_type_layout(&self, id: TypeLayoutId) -> Result<bool> {
        Ok(self.type_layouts.contains(id))
    }

    fn append_data_record(
        &mut self,
        _transaction_stream_id: u32,
        data_stream: DurableDataStream,
        _record: &[u8],
    ) -> Result<DurableDataRecordPointer> {
        self.push_event(RecordingBackendEvent::AppendDataRecord(data_stream));
        let data_block = self.next_data_block;
        self.next_data_block = self
            .next_data_block
            .checked_add(1)
            .context("recording backend data block overflow")?;
        Ok(DurableDataRecordPointer {
            chunk_start_block: data_block,
            data_block,
            data_offset: 0,
            data_block_generation: 0,
        })
    }

    fn append_log_entry(&mut self, _transaction_stream_id: u32, entry: TxLogEntry) -> Result<()> {
        self.push_event(RecordingBackendEvent::AppendLogEntry(entry.role()?));
        Ok(())
    }

    fn mark_last_log_entry_committed(
        &mut self,
        _transaction_stream_id: u32,
        marker: PendingCommitLogEntry,
    ) -> Result<()> {
        self.push_event(RecordingBackendEvent::MarkLogEntryCommitted(marker.role));
        Ok(())
    }

    fn flush_data(&mut self) -> Result<()> {
        self.push_event(RecordingBackendEvent::FlushData);
        Ok(())
    }

    fn flush_log(&mut self) -> Result<()> {
        self.push_event(RecordingBackendEvent::FlushLog);
        if core::mem::take(&mut self.fail_flush_log_once) {
            bail!("recording backend flush log failure");
        }
        Ok(())
    }

    fn fence(&mut self) -> Result<()> {
        self.push_event(RecordingBackendEvent::Fence);
        Ok(())
    }

    fn retire_committed_linear_undo_chunk(&mut self, chunk_start_block: u32) -> Result<()> {
        self.push_event(RecordingBackendEvent::RetireCommittedLinearUndoChunk(
            chunk_start_block,
        ));
        if self.retire_committed_linear_undo_failures_remaining > 0 {
            self.retire_committed_linear_undo_failures_remaining -= 1;
            self.push_event(RecordingBackendEvent::RetireCommittedLinearUndoChunkFailed(
                chunk_start_block,
            ));
            bail!("recording backend post-LP cleanup failure")
        }
        Ok(())
    }

    fn log_entries_for_test(&self, _stream_id: u32) -> Vec<TxLogEntry> {
        Vec::new()
    }

    fn data_chunk_stream_id_for_data_block_for_test(&self, _data_block: u32) -> Result<u32> {
        bail!("recording transaction log backend has no file-backed data chunks")
    }
}

#[cfg(test)]
impl<'a, S> StreamPublisher<'a, S>
where
    S: DurableSink,
{
    fn new_for_test(sink: &'a mut S, stream_id: u32, txid: u32) -> Self {
        Self::new(sink, stream_id, txid)
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DurabilityEvent {
    DataWrite,
    DataFlush,
    LogWrite,
    LogFlush,
    Fence,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct RecordingDurability {
    events: Vec<DurabilityEvent>,
    tx_meta: Vec<u32>,
    roles: Vec<TxLogEntryRole>,
    next_data_block: u32,
}

#[cfg(test)]
impl RecordingDurability {
    fn final_lp_count(&self) -> usize {
        self.tx_meta
            .iter()
            .filter(|entry| (**entry & 1) != 0)
            .count()
    }

    fn non_final_log_entries_before_final_lp(&self) -> bool {
        let Some(final_index) = self.tx_meta.iter().position(|entry| (*entry & 1) != 0) else {
            return false;
        };
        self.tx_meta[..final_index]
            .iter()
            .all(|entry| (*entry & 1) == 0)
    }

    fn events(&self) -> &[DurabilityEvent] {
        &self.events
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
struct RetirementFailingDurability {
    inner: RecordingDurability,
    retired_chunks: Vec<u32>,
}

#[cfg(test)]
impl DurableSink for RetirementFailingDurability {
    fn append_data_record(
        &mut self,
        record: &[u8],
        data_stream: DurableDataStream,
    ) -> Result<DurableDataRecordPointer> {
        self.inner.append_data_record(record, data_stream)
    }

    fn append_log_entry(
        &mut self,
        logical_id: u64,
        version: u32,
        tx_meta: u32,
        data_block: u32,
        data_offset: u32,
        data_block_generation: u32,
        is_final: bool,
        role: TxLogEntryRole,
    ) -> Result<()> {
        self.inner.append_log_entry(
            logical_id,
            version,
            tx_meta,
            data_block,
            data_offset,
            data_block_generation,
            is_final,
            role,
        )
    }

    fn mark_last_log_entry_committed(&mut self, marker: PendingCommitLogEntry) -> Result<()> {
        self.inner.mark_last_log_entry_committed(marker)
    }

    fn flush_data(&mut self) -> Result<()> {
        self.inner.flush_data()
    }

    fn flush_log(&mut self) -> Result<()> {
        self.inner.flush_log()
    }

    fn fence(&mut self) -> Result<()> {
        self.inner.fence()
    }

    fn retire_committed_linear_undo_chunk(&mut self, chunk_start_block: u32) -> Result<()> {
        self.retired_chunks.push(chunk_start_block);
        bail!("post-LP cleanup failure")
    }
}

#[cfg(test)]
impl DurableSink for RecordingDurability {
    fn append_data_record(
        &mut self,
        _record: &[u8],
        _data_stream: DurableDataStream,
    ) -> Result<DurableDataRecordPointer> {
        self.events.push(DurabilityEvent::DataWrite);
        let data_block = self.next_data_block;
        self.next_data_block = self
            .next_data_block
            .checked_add(1)
            .context("recording durability data block overflow")?;
        Ok(DurableDataRecordPointer {
            chunk_start_block: data_block,
            data_block,
            data_offset: 0,
            data_block_generation: 0,
        })
    }

    fn append_log_entry(
        &mut self,
        _logical_id: u64,
        _version: u32,
        tx_meta: u32,
        _data_block: u32,
        _data_offset: u32,
        _data_block_generation: u32,
        is_final: bool,
        role: TxLogEntryRole,
    ) -> Result<()> {
        self.events.push(DurabilityEvent::LogWrite);
        let stored_tx_meta = if is_final { tx_meta | 1 } else { tx_meta & !1 };
        self.tx_meta.push(stored_tx_meta);
        self.roles.push(role);
        Ok(())
    }

    fn mark_last_log_entry_committed(&mut self, _marker: PendingCommitLogEntry) -> Result<()> {
        let entry = self
            .tx_meta
            .last_mut()
            .context("recording durability commit LP requires a log entry")?;
        *entry |= 1;
        Ok(())
    }

    fn flush_data(&mut self) -> Result<()> {
        self.events.push(DurabilityEvent::DataFlush);
        Ok(())
    }

    fn flush_log(&mut self) -> Result<()> {
        self.events.push(DurabilityEvent::LogFlush);
        Ok(())
    }

    fn fence(&mut self) -> Result<()> {
        self.events.push(DurabilityEvent::Fence);
        Ok(())
    }
}

#[cfg(test)]
fn sample_publications() -> Vec<PendingPublication> {
    vec![
        PendingPublication {
            logical_id: 0x1000,
            version: 1,
            kind: 7,
            type_layout_id: 0x10,
            payload: vec![1, 2, 3, 4],
        },
        PendingPublication {
            logical_id: 0x1001,
            version: 2,
            kind: 9,
            type_layout_id: 0x20,
            payload: vec![5, 6, 7, 8, 9],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::transaction::{
        ObjectKind, ObjectPayload, ObjectValue, ObjectValueAbi,
        config::TMemoryRegionConfig,
        object_heap::encode_object_record_for_test,
        object_heap::{TxArrayHeader, TxObjectHeader},
        type_layout::{PersistentTypeLayout, StructTraceField, TraceSlotKind, TypeLayoutId},
    };
    use crate::runtime::vm::RecoveredObjectWinner;

    const TEST_STRUCT_TYPE_LAYOUT_ID: u32 = 12;

    fn first_active_linear_undo_chunk_for_test(path: &Path) -> (u32, u32) {
        let region =
            crate::runtime::vm::block_region::FileBackedMemoryBlockRegion::open_for_test(path)
                .unwrap();
        for block in 0..region.view().num_blocks() {
            let block = u32::try_from(block).unwrap();
            let Ok(entries) = region.view().log_block_entries(block) else {
                continue;
            };
            for entry in entries {
                if entry.role().unwrap() != TxLogEntryRole::TMemoryUndo {
                    continue;
                }
                let meta = region.block_meta(entry.data_block).unwrap();
                if meta.is_active_or_sealed().unwrap() {
                    return (meta.chunk_start, meta.generation);
                }
            }
        }
        panic!("expected an active linear undo chunk");
    }

    fn sample_struct_type_layout(id: u32, fingerprint: u64) -> PersistentTypeLayout {
        PersistentTypeLayout::Struct {
            id: TypeLayoutId::new(id).unwrap(),
            fingerprint,
            body_size: 16,
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

    fn scalar_struct_type_layout_for_test(id: u32, body_size: u32) -> PersistentTypeLayout {
        PersistentTypeLayout::Struct {
            id: TypeLayoutId::new(id).unwrap(),
            fingerprint: (u64::from(id) << 32) | u64::from(body_size),
            body_size,
            fields: vec![],
        }
    }

    fn test_struct_type_layout() -> PersistentTypeLayout {
        PersistentTypeLayout::Struct {
            id: TypeLayoutId::new(TEST_STRUCT_TYPE_LAYOUT_ID).unwrap(),
            fingerprint: 0x7478_5f74_6573_740c,
            body_size: 4,
            fields: vec![StructTraceField {
                field_index: 0,
                field_offset: 0,
                value_size: 4,
                kind: TraceSlotKind::Scalar,
            }],
        }
    }

    fn test_struct_publication(object_index: u64, version: u32, value: i32) -> PendingPublication {
        PendingPublication::persistent_object_for_test(
            PackedGranuleDomain::TStruct,
            object_index,
            version,
            TEST_STRUCT_TYPE_LAYOUT_ID,
            &encode_object_record_for_test(
                object_index,
                version,
                TEST_STRUCT_TYPE_LAYOUT_ID,
                &ObjectPayload::Struct(vec![ObjectValue::I32(value)]),
            )
            .unwrap(),
        )
    }

    fn publish_sample_publications_with_lp<S>(publisher: &mut StreamPublisher<'_, S>) -> Result<()>
    where
        S: DurableSink,
    {
        let publications = sample_publications();
        let marker = publisher.publish_object_publications_before_commit(&publications)?;
        if let Some(marker) = marker {
            publisher.publish_commit_lp(marker)?;
        }
        Ok(())
    }

    fn publish_single_object_publication_for_test(
        log: &mut TxDurableLog,
        txid: u32,
        publication: PendingPublication,
    ) -> Result<()> {
        let marker = {
            let mut sink = log.stream_sink(1);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 1, txid);
            publisher.publish_object_publication_before_commit(&publication)?
        };
        let mut sink = log.stream_sink(1);
        let mut publisher = StreamPublisher::new_for_test(&mut sink, 1, txid);
        publisher.publish_commit_lp(marker)?;
        Ok(())
    }

    #[test]
    fn commit_publishes_only_the_last_entry_with_lp() {
        let mut recorder = RecordingDurability::default();
        let mut publisher = StreamPublisher::new_for_test(&mut recorder, 4, 21);
        publish_sample_publications_with_lp(&mut publisher).unwrap();

        assert_eq!(recorder.tx_meta.len(), sample_publications().len());
        assert_eq!(recorder.final_lp_count(), 1);
        assert!(recorder.non_final_log_entries_before_final_lp());
    }

    #[test]
    fn commit_orders_data_before_final_lp() {
        let mut recorder = RecordingDurability::default();
        let mut publisher = StreamPublisher::new_for_test(&mut recorder, 4, 99);
        publish_sample_publications_with_lp(&mut publisher).unwrap();

        let data_write_count = sample_publications().len();
        assert!(
            recorder.events()[..data_write_count]
                .iter()
                .all(|event| *event == DurabilityEvent::DataWrite)
        );
        assert_eq!(
            &recorder.events()[data_write_count..data_write_count + 2],
            &[DurabilityEvent::DataFlush, DurabilityEvent::Fence]
        );
        assert_eq!(recorder.events().last(), Some(&DurabilityEvent::Fence));
    }

    #[test]
    fn abort_does_not_publish_lp() {
        let mut recorder = RecordingDurability::default();
        let mut publisher = StreamPublisher::new_for_test(&mut recorder, 5, 12);
        publisher.abort_transaction(&sample_publications()).unwrap();
        assert_eq!(recorder.final_lp_count(), 0);
    }

    #[test]
    fn tmemory_undo_is_durable_before_in_place_write() {
        let mut recorder = RecordingDurability::default();
        let mut publisher = StreamPublisher::new_for_test(&mut recorder, 5, 12);
        let undo = PendingGranuleUndo::tmemory(0x1000_0000_0000_002a, 13, vec![9, 8, 7, 6]);

        publisher
            .publish_tmemory_undo_before_in_place_write(&undo)
            .unwrap();

        assert_eq!(
            recorder.events(),
            &[
                DurabilityEvent::DataWrite,
                DurabilityEvent::DataFlush,
                DurabilityEvent::Fence,
                DurabilityEvent::LogWrite,
                DurabilityEvent::LogFlush,
                DurabilityEvent::Fence,
            ]
        );
        assert_eq!(recorder.final_lp_count(), 0);
        assert_eq!(recorder.roles, vec![TxLogEntryRole::TMemoryUndo]);
    }

    #[test]
    fn tmemory_undo_only_commit_publishes_final_lp_without_new_data() {
        let mut recorder = RecordingDurability::default();
        let mut publisher = StreamPublisher::new_for_test(&mut recorder, 5, 12);
        let undo = PendingGranuleUndo::tmemory(0x1000_0000_0000_002a, 13, vec![9, 8, 7, 6]);

        let marker = publisher
            .publish_tmemory_undo_before_in_place_write(&undo)
            .unwrap();
        publisher.publish_commit_lp(marker).unwrap();

        assert_eq!(
            recorder.events(),
            &[
                DurabilityEvent::DataWrite,
                DurabilityEvent::DataFlush,
                DurabilityEvent::Fence,
                DurabilityEvent::LogWrite,
                DurabilityEvent::LogFlush,
                DurabilityEvent::Fence,
                DurabilityEvent::LogFlush,
                DurabilityEvent::Fence,
            ]
        );
        assert_eq!(recorder.final_lp_count(), 1);
        assert_eq!(recorder.roles, vec![TxLogEntryRole::TMemoryUndo]);
    }

    #[test]
    fn publish_commit_lp_succeeds_when_post_lp_linear_undo_retirement_fails() {
        let mut durability = RetirementFailingDurability::default();
        let mut publisher = StreamPublisher::new_for_test(&mut durability, 5, 12);
        let undo = PendingGranuleUndo::tmemory(0x1000_0000_0000_002a, 13, vec![9, 8, 7, 6]);

        let marker = publisher
            .publish_tmemory_undo_before_in_place_write(&undo)
            .unwrap();
        assert!(publisher.publish_commit_lp(marker).is_ok());
        assert_eq!(durability.retired_chunks, vec![marker.chunk_start_block]);
        assert_eq!(durability.inner.final_lp_count(), 1);
    }

    #[test]
    fn object_publication_before_commit_does_not_publish_lp() {
        let mut recorder = RecordingDurability::default();
        let mut publisher = StreamPublisher::new_for_test(&mut recorder, 5, 12);
        let publication = PendingPublication::persistent_object_for_test(
            PackedGranuleDomain::TStruct,
            41,
            7,
            12,
            &encode_object_record_for_test(
                41,
                7,
                12,
                &ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
            )
            .unwrap(),
        );

        let marker = publisher
            .publish_object_publication_before_commit(&publication)
            .unwrap();

        assert_eq!(marker.logical_id, publication.logical_id);
        assert_eq!(marker.version, publication.version);
        assert_eq!(marker.role, TxLogEntryRole::TObjectPub);
        assert_eq!(recorder.final_lp_count(), 0);
        assert_eq!(recorder.roles, vec![TxLogEntryRole::TObjectPub]);
    }

    #[test]
    fn tx_durable_log_sink_preserves_non_zero_data_block_generation() {
        let mut log = TxDurableLog::default();

        {
            let mut sink = log.stream_sink(7);
            sink.append_log_entry(
                0x1000_0000_0000_002a,
                3,
                11 << 1,
                9,
                64,
                17,
                true,
                TxLogEntryRole::TMemoryUndo,
            )
            .unwrap();
        }

        let entries = log.log_entries_for_test(7);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data_block_generation(), 17);
        assert_eq!(entries[0].role().unwrap(), TxLogEntryRole::TMemoryUndo);
        assert!(entries[0].validate_crc32());
    }

    #[test]
    fn file_backed_object_publication_without_lp_is_not_committed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx-log.bin");
        let mut log = TxDurableLog::create_file_backed(&path, 32).unwrap();
        let publication = PendingPublication::persistent_object_for_test(
            PackedGranuleDomain::TStruct,
            41,
            7,
            12,
            &encode_object_record_for_test(
                41,
                7,
                12,
                &ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
            )
            .unwrap(),
        );

        {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            publisher
                .publish_object_publication_before_commit(&publication)
                .unwrap();
        }
        drop(log);

        let recovered = TxDurableLog::recover_file_backed_for_test(&path).unwrap();
        assert!(recovered.object_winners.is_empty());
    }

    #[test]
    fn file_backed_publisher_stamps_data_block_generation_on_log_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("generation-log.bin");
        let mut log = TxDurableLog::create_file_backed(&path, 64).unwrap();
        let mut sink = log.stream_sink(7);
        let mut publisher = StreamPublisher::new(&mut sink, 7, 11);

        let publication = PendingPublication::persistent_object_for_test(
            PackedGranuleDomain::TStruct,
            41,
            1,
            12,
            &encode_object_record_for_test(
                41,
                1,
                12,
                &ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
            )
            .unwrap(),
        );
        let marker = publisher
            .publish_object_publication_before_commit(&publication)
            .unwrap();
        publisher.publish_commit_lp(marker).unwrap();

        let entries = log.log_entries_for_test(7);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data_block_generation(), 0);
        assert_eq!(entries[0].tx_meta & 1, 1);
        assert_eq!(entries[0].role().unwrap(), TxLogEntryRole::TObjectPub);
    }

    #[test]
    fn file_backed_durable_log_persists_type_layout_metadata_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx-log.bin");
        let layout = sample_struct_type_layout(101, 0x0101_0202_0303_0404);
        let mut log = TxDurableLog::create_file_backed(&path, 32).unwrap();

        assert!(!log.has_type_layout(layout.id()).unwrap());

        log.ensure_type_layout(&layout).unwrap();
        log.ensure_type_layout(&layout).unwrap();

        assert!(log.has_type_layout(layout.id()).unwrap());
        drop(log);

        let region =
            crate::runtime::vm::block_region::FileBackedMemoryBlockRegion::open_for_test(&path)
                .unwrap();
        let registry = region.load_type_layout_metadata().unwrap();
        let layouts = registry.iter().cloned().collect::<Vec<_>>();

        assert_eq!(layouts, vec![layout]);
    }

    #[test]
    fn file_backed_durable_log_rejects_conflicting_type_layout_for_existing_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx-log.bin");
        let original = sample_struct_type_layout(102, 0x1111_2222_3333_4444);
        let conflicting = sample_struct_type_layout(102, 0x5555_6666_7777_8888);
        let mut log = TxDurableLog::create_file_backed(&path, 32).unwrap();

        log.ensure_type_layout(&original).unwrap();
        let err = log
            .ensure_type_layout(&conflicting)
            .unwrap_err()
            .to_string();

        assert!(err.contains("conflicting persistent type layout for id 102"));
    }

    #[test]
    fn transaction_durable_log_can_publish_multiple_undo_records_with_one_lp() {
        let mut log = TxDurableLog::default();
        let first = PendingGranuleUndo::tmemory(0x1000_0000_0000_002a, 13, vec![1, 2, 3, 4]);
        let second = PendingGranuleUndo::tmemory(0x1000_0000_1000_002a, 8, vec![5, 6, 7, 8]);

        let first_marker = {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            publisher
                .publish_tmemory_undo_before_in_place_write(&first)
                .unwrap()
        };
        let second_marker = {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            publisher
                .publish_tmemory_undo_before_in_place_write(&second)
                .unwrap()
        };
        {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            publisher.publish_commit_lp(second_marker).unwrap();
        }

        let entries = log.log_entries_for_test(12);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].logical_id, first_marker.logical_id);
        assert_eq!(entries[1].logical_id, second_marker.logical_id);
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.tx_meta & 1 != 0)
                .count(),
            1
        );
        assert!(entries[1].tx_meta & 1 != 0);
        assert!(entries.iter().all(TxLogEntry::validate_crc32));
    }

    #[test]
    fn file_backed_transaction_log_recovers_committed_tmemory_undo_as_no_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx-log.bin");
        let mut log = TxDurableLog::create_file_backed(&path, 32).unwrap();
        let first = PendingGranuleUndo::tmemory(0x1000_0000_0000_002a, 13, vec![1, 2, 3, 4]);
        let second = PendingGranuleUndo::tmemory(0x1000_0000_1000_002a, 8, vec![5, 6, 7, 8]);

        let final_marker = {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            publisher
                .publish_tmemory_undo_before_in_place_write(&first)
                .unwrap();
            publisher
                .publish_tmemory_undo_before_in_place_write(&second)
                .unwrap()
        };
        {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            publisher.publish_commit_lp(final_marker).unwrap();
        }
        drop(log);

        let recovered = TxDurableLog::recover_file_backed_for_test(&path).unwrap();
        assert!(recovered.tmemory_undo_rollbacks.is_empty());
    }

    #[test]
    fn file_backed_transaction_log_recovers_loose_tmemory_undo_as_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx-log.bin");
        let mut log = TxDurableLog::create_file_backed(&path, 32).unwrap();
        let undo = PendingGranuleUndo::tmemory(0x1000_0000_0000_002a, 13, vec![1, 2, 3, 4]);

        {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            publisher
                .publish_tmemory_undo_before_in_place_write(&undo)
                .unwrap();
        }
        drop(log);

        let recovered = TxDurableLog::recover_file_backed_for_test(&path).unwrap();
        assert_eq!(recovered.tmemory_undo_rollbacks.len(), 1);
        assert_eq!(
            recovered.tmemory_undo_rollbacks[0].old_granule_bytes,
            vec![1, 2, 3, 4]
        );
    }

    #[test]
    fn file_backed_transaction_log_commits_object_pub_and_tmemory_undo_with_one_lp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx-log.bin");
        let mut log = TxDurableLog::create_file_backed(&path, 32).unwrap();
        log.ensure_type_layout(&test_struct_type_layout()).unwrap();
        let undo = PendingGranuleUndo::tmemory(0x1000_0000_0000_002a, 13, vec![1, 2, 3, 4]);
        let publication = PendingPublication::persistent_object_for_test(
            PackedGranuleDomain::TStruct,
            41,
            7,
            TEST_STRUCT_TYPE_LAYOUT_ID,
            &encode_object_record_for_test(
                41,
                7,
                TEST_STRUCT_TYPE_LAYOUT_ID,
                &ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
            )
            .unwrap(),
        );

        let object_marker = {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            publisher
                .publish_tmemory_undo_before_in_place_write(&undo)
                .unwrap();
            publisher
                .publish_object_publication_before_commit(&publication)
                .unwrap()
        };
        {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            publisher.publish_commit_lp(object_marker).unwrap();
        }
        assert_eq!(
            {
                let entries = log.log_entries_for_test(12);
                assert_eq!(entries.len(), 2);
                entries
                    .iter()
                    .filter(|entry| entry.tx_meta & 1 != 0)
                    .count()
            },
            1
        );
        drop(log);

        let recovered = TxDurableLog::recover_file_backed_for_test(&path).unwrap();
        assert_eq!(recovered.object_winners.len(), 1);
        assert_eq!(recovered.object_winners[0].object_id, 41);
        assert!(recovered.tmemory_undo_rollbacks.is_empty());
    }

    #[test]
    fn dax_pmem_durable_region_log_recovers_object_publication() {
        let mut log = TxDurableLog::create_dax_pmem_research_for_test(32).unwrap();
        log.ensure_type_layout(&test_struct_type_layout()).unwrap();
        let publication = PendingPublication::persistent_object_for_test(
            PackedGranuleDomain::TStruct,
            41,
            7,
            TEST_STRUCT_TYPE_LAYOUT_ID,
            &encode_object_record_for_test(
                41,
                7,
                TEST_STRUCT_TYPE_LAYOUT_ID,
                &ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
            )
            .unwrap(),
        );

        let marker = {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            publisher
                .publish_object_publication_before_commit(&publication)
                .unwrap()
        };
        {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            publisher.publish_commit_lp(marker).unwrap();
        }

        let recovered = log.recover_region_snapshot().unwrap().unwrap();
        let object_winners = recovered.committed_object_winners().unwrap();
        assert_eq!(object_winners.len(), 1);
        assert_eq!(object_winners[0].object_id, 41);
        assert!(
            recovered
                .type_layouts
                .contains(test_struct_type_layout().id())
        );
    }

    #[test]
    fn in_memory_durable_log_snapshot_callback_reports_absent_recovery() {
        let log = TxDurableLog::default();
        let mut called = false;

        let consumed = log
            .with_recovered_region_snapshot(&mut |_, _| {
                called = true;
                Ok(())
            })
            .unwrap();

        assert!(!consumed);
        assert!(!called);
    }

    #[test]
    fn dax_pmem_durable_region_log_snapshot_callback_exposes_mapped_source() {
        let mut log = TxDurableLog::create_dax_pmem_research_for_test(32).unwrap();
        log.ensure_type_layout(&test_struct_type_layout()).unwrap();
        let expected_record = encode_object_record_for_test(
            41,
            7,
            TEST_STRUCT_TYPE_LAYOUT_ID,
            &ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
        )
        .unwrap();
        let publication = PendingPublication::persistent_object_for_test(
            PackedGranuleDomain::TStruct,
            41,
            7,
            TEST_STRUCT_TYPE_LAYOUT_ID,
            &expected_record,
        );

        let marker = {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            publisher
                .publish_object_publication_before_commit(&publication)
                .unwrap()
        };
        {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            publisher.publish_commit_lp(marker).unwrap();
        }

        let mut consumed = false;
        let mut recovered_record = None;
        log.with_recovered_region_snapshot(&mut |recovered, mapped_source| {
            let mapped_source = mapped_source.expect("mapped durable log backend");
            let winner = recovered
                .committed_object_winners()?
                .into_iter()
                .find(|winner| winner.object_id == 41)
                .expect("recovered object winner");
            winner.with_source_record_bytes(mapped_source.as_ref(), &mut |bytes| {
                recovered_record = Some(bytes.to_vec());
                Ok(())
            })?;
            consumed = true;
            Ok(())
        })
        .unwrap();

        assert!(consumed);
        assert_eq!(recovered_record, Some(expected_record));
    }

    #[test]
    fn dax_pmem_durable_log_retires_dead_object_chunk_and_reuses_generation() {
        let mut log = TxDurableLog::create_dax_pmem_research_for_test(32).unwrap();
        log.ensure_type_layout(&test_struct_type_layout()).unwrap();
        let first_publication = PendingPublication::persistent_object_for_test(
            PackedGranuleDomain::TStruct,
            41,
            7,
            TEST_STRUCT_TYPE_LAYOUT_ID,
            &encode_object_record_for_test(
                41,
                7,
                TEST_STRUCT_TYPE_LAYOUT_ID,
                &ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
            )
            .unwrap(),
        );

        let first_marker = {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            let marker = publisher
                .publish_object_publication_before_commit(&first_publication)
                .unwrap();
            publisher.publish_commit_lp(marker).unwrap();
            marker
        };

        let recovered = log.recover_region_snapshot().unwrap().unwrap();
        let first_winner = recovered
            .committed_object_winners()
            .unwrap()
            .into_iter()
            .find(|winner| winner.object_id == 41)
            .unwrap();
        let retired = log
            .retire_whole_dead_object_chunks(
                &[],
                &[PersistentRecoveredRecordLocation {
                    object_id: crate::runtime::transaction::ObjectId { object_index: 41 },
                    version: first_winner.version,
                    data_block: first_winner.data_block,
                    data_offset: first_winner.data_offset,
                    record_len: first_winner.record_len,
                }],
            )
            .unwrap();

        assert_eq!(retired, vec![first_marker.chunk_start_block]);

        let second_publication = PendingPublication::persistent_object_for_test(
            PackedGranuleDomain::TStruct,
            42,
            1,
            TEST_STRUCT_TYPE_LAYOUT_ID,
            &encode_object_record_for_test(
                42,
                1,
                TEST_STRUCT_TYPE_LAYOUT_ID,
                &ObjectPayload::Struct(vec![ObjectValue::I32(11)]),
            )
            .unwrap(),
        );
        let second_marker = {
            let mut sink = log.stream_sink(13);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 13, 13);
            publisher
                .publish_object_publication_before_commit(&second_publication)
                .unwrap()
        };

        assert_eq!(
            second_marker.chunk_start_block,
            first_marker.chunk_start_block
        );
        assert_eq!(
            second_marker.data_block_generation,
            first_marker.data_block_generation + 1
        );
    }

    #[cfg(target_os = "linux")]
    fn real_pmem_durable_log_test_path(file_name: &str) -> Option<std::path::PathBuf> {
        if std::env::var_os("WASMTIME_TEST_REAL_PMEM").is_none() {
            return None;
        }
        let root = std::env::var_os("WASMTIME_TEST_DAX_PMEM_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/pmem0/wasmtime-dcpmm"));
        Some(root.join(file_name))
    }

    #[cfg(target_os = "linux")]
    const STRESS_ARRAY_TYPE_LAYOUT_ID: u32 = 13;
    #[cfg(target_os = "linux")]
    const STRESS_OBJECT_ENCODED_BATCH_BYTES: u64 = 4 * 1024 * 1024;
    #[cfg(target_os = "linux")]
    const STRESS_OBJECT_BATCH_PUBLICATIONS: usize = 256;
    #[cfg(target_os = "linux")]
    const STRESS_LINEAR_CHUNK_BYTES: usize = 1024 * 1024;
    #[cfg(target_os = "linux")]
    const STRESS_LINEAR_SEGMENT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
    #[cfg(target_os = "linux")]
    const STRESS_LINEAR_SAMPLE_BYTES: usize = 256;
    #[cfg(target_os = "linux")]
    const STRESS_LINEAR_SAMPLE_STRIDE_BYTES: u64 = 64 * 1024 * 1024;
    #[cfg(target_os = "linux")]
    const STRESS_OBJECT_SAMPLE_STRIDE_BYTES: u64 = 64 * 1024 * 1024;
    #[cfg(target_os = "linux")]
    const STRESS_OBJECT_SAMPLE_FIRST_COUNT: usize = 4;
    #[cfg(target_os = "linux")]
    const STRESS_OBJECT_SAMPLE_LAST_COUNT: usize = 4;
    #[cfg(target_os = "linux")]
    const STRESS_OBJECT_SAMPLE_STRIDE_COUNT: usize = 16;
    #[cfg(target_os = "linux")]
    const STRESS_DEFAULT_SEED: u64 = 0x5eed_dacc_2026_0621;
    const STRESS_OBJECT_ID_ROOT_SHIFT: u32 = 48;
    #[cfg(target_os = "linux")]
    const STRESS_OBJECT_ID_LOCAL_MASK: u64 = (1u64 << STRESS_OBJECT_ID_ROOT_SHIFT) - 1;
    #[cfg(target_os = "linux")]
    const STRESS_MULTI_REGION_WINDOW_BASE_U64: u64 = 0x4000_0000_0000;
    #[cfg(target_os = "linux")]
    const STRESS_MULTI_REGION_WINDOW_STRIDE_U64: u64 = 1u64 << 40;

    #[cfg(target_os = "linux")]
    #[derive(Clone, Debug)]
    struct DaxPmemStressConfig {
        bytes_per_root: u64,
        roots: Vec<std::path::PathBuf>,
        seed: u64,
        keep_files: bool,
    }

    #[cfg(target_os = "linux")]
    #[derive(Debug)]
    struct ObjectStressStats {
        path: std::path::PathBuf,
        payload_blocks: usize,
        object_count: usize,
        sampled_object_count: usize,
        bucket_counts: [u64; 5],
        requested_logical_bytes: u64,
        encoded_bytes: u64,
        populate_elapsed: std::time::Duration,
        recovery_elapsed: std::time::Duration,
        recovery_samples: Vec<ObjectRecoverySample>,
        recovery_workers: usize,
        recovered_object_count: usize,
    }

    #[cfg(target_os = "linux")]
    #[derive(Debug)]
    struct LinearStressStats {
        paths: Vec<std::path::PathBuf>,
        committed_bytes: u64,
        pages: u64,
        sample_count: usize,
        populate_elapsed: std::time::Duration,
        reopen_validate_elapsed: std::time::Duration,
    }

    #[cfg(target_os = "linux")]
    #[derive(Clone, Copy, Debug)]
    struct RegionRecoveryStatsForTest {
        root_index: usize,
        numa_node: Option<i32>,
        workers: usize,
        recovered_bytes: u64,
        elapsed: std::time::Duration,
    }

    #[cfg(target_os = "linux")]
    #[derive(Clone, Copy, Debug)]
    struct StressRng {
        state: u64,
    }

    #[cfg(target_os = "linux")]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct ObjectRecoverySample {
        object_id: u64,
        requested_logical_bytes: u64,
    }

    #[cfg(target_os = "linux")]
    #[derive(Debug)]
    struct ObjectRecoverySampleSet {
        first: Vec<ObjectRecoverySample>,
        stride: Vec<ObjectRecoverySample>,
        last: Vec<ObjectRecoverySample>,
        next_stride_threshold: u64,
    }

    #[cfg(target_os = "linux")]
    #[derive(Debug)]
    struct MultiRegionDaxObjectLogForTest {
        root_index: usize,
        path: std::path::PathBuf,
        numa_node: Option<i32>,
        object_count: usize,
        encoded_bytes: u64,
        seed: u64,
        recovery_samples: Vec<ObjectRecoverySample>,
    }

    #[cfg(target_os = "linux")]
    impl StressRng {
        fn new(seed: u64) -> Self {
            Self {
                state: if seed == 0 {
                    0x9e37_79b9_7f4a_7c15
                } else {
                    seed
                },
            }
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.state;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.state = x;
            x.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, upper_exclusive: u64) -> u64 {
            debug_assert!(upper_exclusive > 0);
            self.next_u64() % upper_exclusive
        }

        fn range_inclusive(&mut self, start: u64, end: u64) -> u64 {
            debug_assert!(start <= end);
            start + self.below(end - start + 1)
        }
    }

    #[cfg(target_os = "linux")]
    impl ObjectRecoverySampleSet {
        fn new() -> Self {
            Self {
                first: Vec::new(),
                stride: Vec::new(),
                last: Vec::new(),
                next_stride_threshold: STRESS_OBJECT_SAMPLE_STRIDE_BYTES,
            }
        }

        fn observe(&mut self, object_id: u64, requested_logical_bytes: u64, encoded_bytes: u64) {
            let sample = ObjectRecoverySample {
                object_id,
                requested_logical_bytes,
            };
            if self.first.len() < STRESS_OBJECT_SAMPLE_FIRST_COUNT {
                self.first.push(sample);
            }
            self.last.push(sample);
            if self.last.len() > STRESS_OBJECT_SAMPLE_LAST_COUNT {
                self.last.remove(0);
            }
            while self.next_stride_threshold <= encoded_bytes {
                if self.stride.len() < STRESS_OBJECT_SAMPLE_STRIDE_COUNT {
                    self.stride.push(sample);
                }
                self.next_stride_threshold = self
                    .next_stride_threshold
                    .saturating_add(STRESS_OBJECT_SAMPLE_STRIDE_BYTES);
                if self.next_stride_threshold == 0 {
                    break;
                }
            }
        }

        fn into_vec(self) -> Vec<ObjectRecoverySample> {
            let mut samples = Vec::new();
            let mut seen = BTreeSet::new();
            for sample in self.first.into_iter().chain(self.stride).chain(self.last) {
                if seen.insert(sample.object_id) {
                    samples.push(sample);
                }
            }
            samples
        }
    }

    #[cfg(target_os = "linux")]
    fn dax_stress_object_id(root_index: usize, local_ordinal: u64) -> Result<u64> {
        ensure!(
            local_ordinal > 0,
            "DAX stress object id local ordinal must be nonzero"
        );
        ensure!(
            local_ordinal <= STRESS_OBJECT_ID_LOCAL_MASK,
            "DAX stress object id local ordinal {local_ordinal} exceeds {}",
            STRESS_OBJECT_ID_LOCAL_MASK
        );
        let root_slot = u64::try_from(root_index).context("stress root index exceeds u64")?;
        let root_base = root_slot
            .checked_shl(STRESS_OBJECT_ID_ROOT_SHIFT)
            .context("stress object id root partition shift overflow")?;
        let object_id = root_base
            .checked_add(local_ordinal)
            .context("stress object id overflow")?;
        ensure!(
            object_id < (1u64 << 60),
            "DAX stress object id {object_id} exceeds packed object id limit"
        );
        Ok(object_id)
    }

    #[cfg(target_os = "linux")]
    fn dax_stress_existing_file_len(path: &Path) -> Result<usize> {
        let len = std::fs::metadata(path)
            .with_context(|| format!("reading metadata for {}", path.display()))?
            .len();
        ensure!(len > 0, "DAX stress log {} is empty", path.display());
        usize::try_from(len).with_context(|| {
            format!(
                "DAX stress log {} length {len} exceeds addressable usize",
                path.display()
            )
        })
    }

    #[cfg(target_os = "linux")]
    fn build_multi_region_dax_object_log_configs_for_test(
        regions: &[MultiRegionDaxObjectLogForTest],
    ) -> Result<Vec<TMemoryRegionConfig>> {
        ensure!(
            !regions.is_empty(),
            "multi-region DAX object recovery requires at least one region"
        );

        let mut configs = Vec::with_capacity(regions.len());
        for (slot, region) in regions.iter().enumerate() {
            let reserved_len = dax_stress_existing_file_len(&region.path)?;
            ensure!(
                reserved_len % crate::runtime::vm::block_region::BLOCK_SIZE == 0,
                "multi-region DAX object log {} length {} is not block-aligned",
                region.path.display(),
                reserved_len
            );
            ensure!(
                u64::try_from(reserved_len).unwrap() <= STRESS_MULTI_REGION_WINDOW_STRIDE_U64,
                "multi-region DAX object log {} length {} exceeds fixed window stride {}",
                region.path.display(),
                reserved_len,
                STRESS_MULTI_REGION_WINDOW_STRIDE_U64
            );
            let slot_u64 = u64::try_from(slot).context("multi-region stress region slot overflow")?;
            let address_base_u64 = STRESS_MULTI_REGION_WINDOW_BASE_U64
                .checked_add(
                    slot_u64
                        .checked_mul(STRESS_MULTI_REGION_WINDOW_STRIDE_U64)
                        .context("multi-region stress fixed-window offset overflow")?,
                )
                .context("multi-region stress fixed-window base overflow")?;
            let address_base = usize::try_from(address_base_u64).with_context(|| {
                format!(
                    "multi-region stress fixed-window base {address_base_u64:#x} exceeds usize"
                )
            })?;
            configs.push(TMemoryRegionConfig {
                path: region.path.clone(),
                address_base,
                reserved_len,
                numa_node: region.numa_node,
            });
        }

        Ok(configs)
    }

    #[cfg(target_os = "linux")]
    fn run_multi_region_dax_object_recovery_for_test(
        regions: &[MultiRegionDaxObjectLogForTest],
    ) -> Result<(
        Vec<RegionRecoveryStatsForTest>,
        std::time::Duration,
        std::time::Duration,
    )> {
        ensure!(
            regions.len() >= 2,
            "multi-region DAX object recovery requires at least two regions"
        );

        let configs = build_multi_region_dax_object_log_configs_for_test(regions)?;
        let log = TxDurableLog::open_dax_pmem_fsdax_regions_for_test(configs)?;
        let recovered = log
            .recover_region_snapshot()?
            .context("expected multi-region DAX PMEM durable log recovery snapshot")?;
        let timing = log
            .latest_multi_region_recovery_timing_for_test()
            .context("multi-region DAX object recovery did not report phase timing")?;
        let worker_counts = log.latest_planned_recovery_worker_counts_for_test();
        ensure!(
            worker_counts.len() == regions.len(),
            "multi-region DAX object recovery reported {} region worker counts for {} regions",
            worker_counts.len(),
            regions.len()
        );
        ensure!(
            timing.region_recovery_elapsed.len() == regions.len(),
            "multi-region DAX object recovery reported {} region timings for {} regions",
            timing.region_recovery_elapsed.len(),
            regions.len()
        );
        let recovered_winners = recovered.committed_object_winners()?;
        let expected_object_count = regions.iter().map(|region| region.object_count).sum::<usize>();
        ensure!(
            recovered_winners.len() == expected_object_count,
            "multi-region recovered object count mismatch: published {expected_object_count}, recovered {}",
            recovered_winners.len()
        );
        let winners_by_id = recovered_winners
            .iter()
            .map(|winner| (winner.object_id, winner))
            .collect::<BTreeMap<u64, &RecoveredObjectWinner>>();
        let mapped_source = recovered
            .mapped_region_source()
            .context("multi-region recovered DAX PMEM snapshot does not expose a mapped region source")?;
        for region in regions {
            validate_recovered_object_samples(
                region.seed,
                &region.recovery_samples,
                &winners_by_id,
                mapped_source,
            )
            .with_context(|| {
                format!(
                    "validating multi-region DAX object recovery samples for root {}",
                    region.root_index
                )
            })?;
        }

        let region_stats = regions
            .iter()
            .zip(worker_counts)
            .zip(timing.region_recovery_elapsed.iter().copied())
            .map(|((region, workers), elapsed)| RegionRecoveryStatsForTest {
                root_index: region.root_index,
                numa_node: region.numa_node,
                workers,
                recovered_bytes: region.encoded_bytes,
                elapsed,
            })
            .collect::<Vec<_>>();
        Ok((region_stats, timing.merge_elapsed, timing.total_elapsed))
    }

    #[cfg(target_os = "linux")]
    fn dax_pmem_fsdax_stress_impl(config: &DaxPmemStressConfig) -> Result<()> {
        let object_target_bytes = config
            .bytes_per_root
            .checked_mul(45)
            .context("per-root stress byte budget overflow")?
            / 100;
        let linear_target_bytes = config
            .bytes_per_root
            .checked_mul(45)
            .context("per-root stress byte budget overflow")?
            / 100;
        let headroom_bytes = config
            .bytes_per_root
            .checked_sub(object_target_bytes + linear_target_bytes)
            .context("per-root stress split overflow")?;
        let mut multi_region_object_logs = Vec::with_capacity(config.roots.len());
        let mut cleanup_paths = Vec::new();

        for (root_index, root) in config.roots.iter().enumerate() {
            std::fs::create_dir_all(root)
                .with_context(|| format!("creating DAX stress root {}", root.display()))?;

            let root_seed = config.seed.wrapping_add(u64::try_from(root_index).unwrap())
                ^ u64::from(std::process::id()).wrapping_mul(0x9e37_79b9);
            let object_seed = root_seed ^ 0x0b1e_c710_6a2d_5f51;
            let object = run_dax_object_stress(
                root,
                root_index,
                object_target_bytes,
                object_seed,
            )?;
            let linear = run_dax_linear_stress(
                root,
                root_index,
                linear_target_bytes,
                root_seed ^ 0x1ea4_5eed_2048_abcd,
            )?;

            eprintln!(
                "{}",
                format_dax_stress_line(
                    root_index,
                    config.bytes_per_root,
                    object_target_bytes,
                    linear_target_bytes,
                    headroom_bytes,
                    &object,
                    &linear,
                )
            );
            let numa_node = infer_dax_stress_numa_node(root);
            multi_region_object_logs.push(MultiRegionDaxObjectLogForTest {
                root_index,
                path: object.path.clone(),
                numa_node,
                object_count: object.object_count,
                encoded_bytes: object.encoded_bytes,
                seed: object_seed,
                recovery_samples: object.recovery_samples.clone(),
            });
            cleanup_paths.push(object.path.clone());
            cleanup_paths.extend(linear.paths.iter().cloned());
        }

        if multi_region_object_logs.len() >= 2 {
            let (region_recovery, merge_elapsed, total_elapsed) =
                run_multi_region_dax_object_recovery_for_test(&multi_region_object_logs)?;
            eprintln!(
                "{}",
                format_multi_region_dax_recovery_line_for_test(
                    &region_recovery,
                    merge_elapsed,
                    total_elapsed,
                )
            );
        }

        if !config.keep_files {
            for path in cleanup_paths {
                remove_file_if_exists(&path)?;
            }
        }

        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn dax_pmem_stress_config_from_env() -> Result<DaxPmemStressConfig> {
        let bytes_per_root = parse_scaled_env_u64(
            "WASMTIME_DAX_STRESS_BYTES",
            &std::env::var("WASMTIME_DAX_STRESS_BYTES")
                .context("WASMTIME_DAX_STRESS_BYTES must be valid UTF-8")?,
        )?;
        let roots = if let Ok(value) = std::env::var("WASMTIME_DAX_STRESS_DIRS") {
            parse_root_dirs(&value)
        } else if let Some(root) = std::env::var_os("WASMTIME_TEST_DAX_PMEM_DIR") {
            vec![std::path::PathBuf::from(root)]
        } else {
            vec![std::path::PathBuf::from("/pmem0/wasmtime-dcpmm")]
        };
        let seed = match std::env::var("WASMTIME_DAX_STRESS_SEED") {
            Ok(value) => parse_seed_u64(&value)?,
            Err(std::env::VarError::NotPresent) => STRESS_DEFAULT_SEED,
            Err(std::env::VarError::NotUnicode(_)) => {
                bail!("WASMTIME_DAX_STRESS_SEED must be valid UTF-8")
            }
        };
        let keep_files = std::env::var_os("WASMTIME_DAX_STRESS_KEEP_FILES").as_deref()
            == Some(std::ffi::OsStr::new("1"));
        ensure!(
            !roots.is_empty(),
            "DAX PMEM stress test requires at least one root directory"
        );

        Ok(DaxPmemStressConfig {
            bytes_per_root,
            roots,
            seed,
            keep_files,
        })
    }

    #[cfg(target_os = "linux")]
    fn parse_root_dirs(value: &str) -> Vec<std::path::PathBuf> {
        let roots = value
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(std::path::PathBuf::from)
            .collect::<Vec<_>>();
        if roots.is_empty() {
            if let Some(root) = std::env::var_os("WASMTIME_TEST_DAX_PMEM_DIR") {
                return vec![std::path::PathBuf::from(root)];
            }
            return vec![std::path::PathBuf::from("/pmem0/wasmtime-dcpmm")];
        }
        roots
    }

    #[cfg(target_os = "linux")]
    fn parse_seed_u64(value: &str) -> Result<u64> {
        let trimmed = value.trim();
        ensure!(
            !trimmed.is_empty(),
            "WASMTIME_DAX_STRESS_SEED cannot be empty"
        );
        if let Some(hex) = trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
        {
            return u64::from_str_radix(hex, 16)
                .with_context(|| format!("invalid hex seed: {trimmed}"));
        }
        trimmed
            .parse::<u64>()
            .with_context(|| format!("invalid decimal seed: {trimmed}"))
    }

    #[cfg(target_os = "linux")]
    fn parse_scaled_env_u64(name: &str, value: &str) -> Result<u64> {
        let trimmed = value.trim();
        ensure!(!trimmed.is_empty(), "{name} cannot be empty");
        let suffix_start = trimmed
            .find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(trimmed.len());
        ensure!(suffix_start > 0, "{name} must start with decimal digits");
        let number = trimmed[..suffix_start]
            .parse::<u64>()
            .with_context(|| format!("{name} has an invalid integer value"))?;
        let suffix = trimmed[suffix_start..].trim().to_ascii_uppercase();
        let scale = match suffix.as_str() {
            "" => 1,
            "K" | "KIB" => 1024,
            "M" | "MIB" => 1024_u64.pow(2),
            "G" | "GIB" => 1024_u64.pow(3),
            "T" | "TIB" => 1024_u64.pow(4),
            _ => bail!("{name} has an unsupported suffix: {suffix}"),
        };
        number
            .checked_mul(scale)
            .with_context(|| format!("{name} overflows u64"))
    }

    #[cfg(target_os = "linux")]
    fn run_dax_object_stress(
        root: &Path,
        root_index: usize,
        target_encoded_bytes: u64,
        seed: u64,
    ) -> Result<ObjectStressStats> {
        let path = root.join(format!(
            "dax-pmem-stress-object-{}-{}.log",
            std::process::id(),
            root_index
        ));
        remove_file_if_exists(&path)?;

        let payload_blocks = estimate_object_payload_blocks(target_encoded_bytes)?;
        let layout = stress_array_type_layout();
        let mut log = TxDurableLog::create_dax_pmem_fsdax_for_test(&path, payload_blocks)
            .with_context(|| format!("creating object stress log {}", path.display()))?;
        log.ensure_type_layout(&layout)?;

        let mut rng = StressRng::new(seed);
        let mut local_object_ordinal = 1u64;
        let mut txid = 1u32;
        let mut object_count = 0usize;
        let mut bucket_counts = [0u64; 5];
        let mut requested_logical_bytes = 0u64;
        let mut encoded_bytes = 0u64;
        let mut batch_encoded_bytes = 0u64;
        let mut recovery_samples = ObjectRecoverySampleSet::new();
        let mut batch = Vec::new();
        let populate_start = std::time::Instant::now();

        while encoded_bytes < target_encoded_bytes {
            let (requested_bytes, bucket_index) = sample_object_logical_size(&mut rng);
            let object_id = dax_stress_object_id(root_index, local_object_ordinal)?;
            let payload = deterministic_array_payload(seed, object_id, requested_bytes)?;
            let record =
                encode_object_record_for_test(object_id, 1, STRESS_ARRAY_TYPE_LAYOUT_ID, &payload)?;
            let record_len =
                u64::try_from(record.len()).context("object record length exceeds u64")?;
            batch.push(PendingPublication::persistent_object_for_test(
                PackedGranuleDomain::TArray,
                object_id,
                1,
                STRESS_ARRAY_TYPE_LAYOUT_ID,
                &record,
            ));
            bucket_counts[bucket_index] += 1;
            requested_logical_bytes = requested_logical_bytes
                .checked_add(requested_bytes)
                .context("object logical byte counter overflow")?;
            encoded_bytes = encoded_bytes
                .checked_add(record_len)
                .context("object encoded byte counter overflow")?;
            batch_encoded_bytes = batch_encoded_bytes
                .checked_add(record_len)
                .context("object batch byte counter overflow")?;
            recovery_samples.observe(object_id, requested_bytes, encoded_bytes);
            object_count += 1;
            local_object_ordinal = local_object_ordinal
                .checked_add(1)
                .context("stress object ordinal overflow during stress test")?;

            if batch_encoded_bytes >= STRESS_OBJECT_ENCODED_BATCH_BYTES
                || batch.len() >= STRESS_OBJECT_BATCH_PUBLICATIONS
            {
                publish_object_batch(&mut log, root_index, txid, &batch)?;
                batch.clear();
                batch_encoded_bytes = 0;
                txid = txid.checked_add(1).context("stress txid overflow")?;
            }
        }

        if !batch.is_empty() {
            publish_object_batch(&mut log, root_index, txid, &batch)?;
        }

        let populate_elapsed = populate_start.elapsed();
        let recovery_start = std::time::Instant::now();
        let recovered = log
            .recover_region_snapshot()?
            .context("expected DAX PMEM durable log recovery snapshot")?;
        let recovered_winners = recovered.committed_object_winners()?;
        let recovered_object_count = recovered_winners.len();
        let recovery_workers = crate::runtime::vm::recovery_worker_count_for_test(
            crate::runtime::vm::RecoveryOptions::default(),
            recovered_winners.len(),
        );
        let recovery_elapsed = recovery_start.elapsed();
        ensure!(
            recovered_object_count == object_count,
            "recovered object count mismatch: published {object_count}, recovered {recovered_object_count}"
        );
        let recovery_samples = recovery_samples.into_vec();
        let winners_by_id = recovered_winners
            .iter()
            .map(|winner| (winner.object_id, winner))
            .collect::<BTreeMap<u64, &RecoveredObjectWinner>>();
        let mapped_source = recovered
            .mapped_region_source()
            .context("recovered DAX PMEM snapshot does not expose a mapped region source")?;
        validate_recovered_object_samples(seed, &recovery_samples, &winners_by_id, mapped_source)?;

        Ok(ObjectStressStats {
            path,
            payload_blocks,
            object_count,
            sampled_object_count: recovery_samples.len(),
            bucket_counts,
            requested_logical_bytes,
            encoded_bytes,
            populate_elapsed,
            recovery_elapsed,
            recovery_samples,
            recovery_workers,
            recovered_object_count,
        })
    }

    #[cfg(target_os = "linux")]
    fn format_dax_stress_line(
        root_index: usize,
        bytes_per_root: u64,
        object_target_bytes: u64,
        linear_target_bytes: u64,
        headroom_bytes: u64,
        object: &ObjectStressStats,
        linear: &LinearStressStats,
    ) -> String {
        format!(
            "dax-stress root[{root_index}] target={}MiB split(obj={}MiB lin={}MiB head={}MiB) \
obj={:.1}MiB/s rec={:.3}s rec_workers={} lin={:.1}MiB/s reopen={:.3}s objects={} recovered={} \
buckets={:?} logical={}MiB encoded={}MiB linear_samples={} object_samples={} obj_blocks={} \
pages={} linear_segments={} obj_path={} tmemory_path={}",
            bytes_to_mib(bytes_per_root),
            bytes_to_mib(object_target_bytes),
            bytes_to_mib(linear_target_bytes),
            bytes_to_mib(headroom_bytes),
            mib_per_sec(object.encoded_bytes, object.populate_elapsed),
            object.recovery_elapsed.as_secs_f64(),
            object.recovery_workers,
            mib_per_sec(linear.committed_bytes, linear.populate_elapsed),
            linear.reopen_validate_elapsed.as_secs_f64(),
            object.object_count,
            object.recovered_object_count,
            object.bucket_counts,
            bytes_to_mib(object.requested_logical_bytes),
            bytes_to_mib(object.encoded_bytes),
            linear.sample_count,
            object.sampled_object_count,
            object.payload_blocks,
            linear.pages,
            linear.paths.len(),
            object.path.display(),
            linear
                .paths
                .first()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<none>".to_string()),
        )
    }

    #[cfg(target_os = "linux")]
    fn format_multi_region_dax_recovery_line_for_test(
        regions: &[RegionRecoveryStatsForTest],
        merge_elapsed: std::time::Duration,
        total_elapsed: std::time::Duration,
    ) -> String {
        let roots = regions
            .iter()
            .map(|region| region.root_index.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let region_workers = regions
            .iter()
            .map(|region| region.workers.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let numa = regions
            .iter()
            .map(|region| match region.numa_node {
                Some(node) => node.to_string(),
                None => "?".to_string(),
            })
            .collect::<Vec<_>>()
            .join(",");
        let region_recovery_mibps = regions
            .iter()
            .map(|region| format!("{:.1}", mib_per_sec(region.recovered_bytes, region.elapsed)))
            .collect::<Vec<_>>()
            .join(",");
        let region_recovery_elapsed = regions
            .iter()
            .map(|region| format!("{:.3}", region.elapsed.as_secs_f64()))
            .collect::<Vec<_>>()
            .join(",");
        let region_recovery_max = regions
            .iter()
            .map(|region| region.elapsed)
            .max()
            .unwrap_or_default();
        let total_workers = regions.iter().map(|region| region.workers).sum::<usize>();
        let total_recovered_bytes = regions
            .iter()
            .map(|region| region.recovered_bytes)
            .sum::<u64>();

        format!(
            "dax-stress multi-root roots=[{roots}] region_workers=[{region_workers}] \
total_workers={total_workers} worker_budget=planned numa=[{numa}] region_recovery_mibps=[{region_recovery_mibps}] \
region_recovery_elapsed=[{region_recovery_elapsed}]s region_recovery_max={:.3}s merge_elapsed={:.3}s \
total_recovery_mibps={:.1} total_recovered={}MiB total_recovery_elapsed={:.3}s",
            region_recovery_max.as_secs_f64(),
            merge_elapsed.as_secs_f64(),
            mib_per_sec(total_recovered_bytes, total_elapsed),
            bytes_to_mib(total_recovered_bytes),
            total_elapsed.as_secs_f64(),
        )
    }

    #[cfg(target_os = "linux")]
    fn infer_dax_stress_numa_node(root: &Path) -> Option<i32> {
        root.components().find_map(|component| {
            let std::path::Component::Normal(name) = component else {
                return None;
            };
            let name = name.to_str()?;
            parse_dax_stress_numa_component(name)
        })
    }

    #[cfg(target_os = "linux")]
    fn parse_dax_stress_numa_component(component: &str) -> Option<i32> {
        let suffix = component.strip_prefix("pmem")?;
        if suffix.is_empty() || !suffix.chars().all(|ch| ch.is_ascii_digit()) {
            return None;
        }
        suffix.parse::<i32>().ok()
    }

    #[cfg(target_os = "linux")]
    fn publish_object_batch(
        log: &mut TxDurableLog,
        root_index: usize,
        txid: u32,
        publications: &[PendingPublication],
    ) -> Result<()> {
        let stream_id = u32::try_from(root_index)
            .context("stress root index exceeds u32")?
            .checked_add(1)
            .context("stress stream id overflow")?;
        let marker = {
            let mut sink = log.stream_sink(stream_id);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, stream_id, txid);
            publisher.publish_object_publications_before_commit(publications)?
        };
        if let Some(marker) = marker {
            let mut sink = log.stream_sink(stream_id);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, stream_id, txid);
            publisher.publish_commit_lp(marker)?;
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn estimate_object_payload_blocks(target_encoded_bytes: u64) -> Result<usize> {
        let block_size = u64::try_from(crate::runtime::vm::block_region::BLOCK_SIZE).unwrap();
        // `target_encoded_bytes` only counts encoded object records. The stress
        // harness wraps each record in a publication data record and also needs
        // room for log blocks, data-chunk headers, and persisted type-layout
        // metadata. Reserve a bounded but conservative region size so large
        // runs do not exhaust the object log before the `encoded_bytes` loop
        // target is reached.
        let estimated_bytes = target_encoded_bytes
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(4 * block_size))
            .context("object stress region size overflow")?;
        let blocks = estimated_bytes.div_ceil(block_size).max(8);
        usize::try_from(blocks).context("object stress payload block count exceeds usize")
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dax_pmem_stress_object_capacity_estimate_reserves_durable_overhead() {
        let block_size = u64::try_from(crate::runtime::vm::block_region::BLOCK_SIZE).unwrap();
        let target_encoded_bytes = 64 * 1024 * 1024;
        let blocks = estimate_object_payload_blocks(target_encoded_bytes).unwrap();
        let estimated_bytes = u64::try_from(blocks).unwrap() * block_size;

        assert!(
            estimated_bytes >= (2 * target_encoded_bytes) + (4 * block_size),
            "estimated_bytes={estimated_bytes} target_encoded_bytes={target_encoded_bytes} block_size={block_size}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dax_pmem_stress_linear_sample_len_rejects_offset_past_end() {
        let err = linear_sample_len(128, 129).unwrap_err().to_string();
        assert!(err.contains("linear sample offset 129 exceeds total bytes 128"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dax_pmem_stress_object_sample_validator_streams_expected_record() {
        let seed = 0x5150_aa55_0123_4567;
        let sample = ObjectRecoverySample {
            object_id: 17,
            requested_logical_bytes: 257,
        };
        let payload =
            deterministic_array_payload(seed, sample.object_id, sample.requested_logical_bytes)
                .unwrap();
        let mut record = encode_object_record_for_test(
            sample.object_id,
            1,
            STRESS_ARRAY_TYPE_LAYOUT_ID,
            &payload,
        )
        .unwrap();

        validate_recovered_stress_array_record(seed, &sample, &record).unwrap();

        let corrupt_offset = record.len() - 3;
        record[corrupt_offset] ^= 0x5a;
        let err = validate_recovered_stress_array_record(seed, &sample, &record)
            .unwrap_err()
            .to_string();
        assert!(err.contains("first_mismatch_offset="));
        assert!(err.contains(&format!("first_mismatch_offset={corrupt_offset}")));
    }

    #[cfg(target_os = "linux")]
    fn sample_object_logical_size(rng: &mut StressRng) -> (u64, usize) {
        let bucket = rng.below(100);
        match bucket {
            0..=54 => (rng.range_inclusive(32, 256), 0),
            55..=79 => (rng.range_inclusive(256, 2 * 1024), 1),
            80..=94 => (rng.range_inclusive(2 * 1024, 32 * 1024), 2),
            95..=98 => (rng.range_inclusive(32 * 1024, 512 * 1024), 3),
            _ => (rng.range_inclusive(512 * 1024, 4 * 1024 * 1024), 4),
        }
    }

    #[cfg(target_os = "linux")]
    fn deterministic_array_payload(
        seed: u64,
        object_id: u64,
        requested_logical_bytes: u64,
    ) -> Result<ObjectPayload> {
        let element_count = requested_logical_bytes.div_ceil(8).max(1);
        let element_count =
            usize::try_from(element_count).context("array element count exceeds usize")?;
        let mut values = Vec::with_capacity(element_count);
        let mut state = mix64(seed ^ object_id.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        for index in 0..element_count {
            state = mix64(
                state
                    ^ u64::try_from(index)
                        .unwrap()
                        .wrapping_mul(0xd1b5_4a32_d192_ed03),
            );
            values.push(ObjectValue::I64(state as i64));
        }
        Ok(ObjectPayload::Array(values))
    }

    #[cfg(target_os = "linux")]
    fn deterministic_array_element_count(requested_logical_bytes: u64) -> Result<usize> {
        let element_count = requested_logical_bytes.div_ceil(8).max(1);
        usize::try_from(element_count).context("array element count exceeds usize")
    }

    #[cfg(target_os = "linux")]
    fn deterministic_array_element_value(seed: u64, object_id: u64, element_index: usize) -> i64 {
        let mut state = mix64(seed ^ object_id.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        for index in 0..=element_index {
            state = mix64(
                state
                    ^ u64::try_from(index)
                        .unwrap()
                        .wrapping_mul(0xd1b5_4a32_d192_ed03),
            );
        }
        state as i64
    }

    #[cfg(target_os = "linux")]
    fn deterministic_array_element_bytes(
        seed: u64,
        object_id: u64,
        element_index: usize,
    ) -> Result<[u8; 20]> {
        object_value_i64_bytes(deterministic_array_element_value(
            seed,
            object_id,
            element_index,
        ))
    }

    #[cfg(target_os = "linux")]
    fn object_value_i64_bytes(value: i64) -> Result<[u8; 20]> {
        let abi = ObjectValueAbi::from_object_value(&ObjectValue::I64(value))?;
        let (tag, low, high) = abi.as_parts();
        let mut bytes = [0u8; 20];
        bytes[0..4].copy_from_slice(&tag.to_le_bytes());
        bytes[4..12].copy_from_slice(&low.to_le_bytes());
        bytes[12..20].copy_from_slice(&high.to_le_bytes());
        Ok(bytes)
    }

    #[cfg(target_os = "linux")]
    fn deterministic_array_record_len(element_count: usize) -> Result<usize> {
        element_count
            .checked_mul(20)
            .and_then(|payload_len| payload_len.checked_add(core::mem::size_of::<TxArrayHeader>()))
            .context("deterministic array record length overflow")
    }

    #[cfg(target_os = "linux")]
    fn validate_recovered_object_samples(
        seed: u64,
        samples: &[ObjectRecoverySample],
        winners_by_id: &BTreeMap<u64, &RecoveredObjectWinner>,
        source: &dyn crate::runtime::vm::block_region::MappedRegionSource,
    ) -> Result<()> {
        for sample in samples {
            let winner = winners_by_id.get(&sample.object_id).with_context(|| {
                format!(
                    "missing recovered winner for sampled object {}",
                    sample.object_id
                )
            })?;
            let mut matched = false;
            winner.with_source_record_bytes(source, &mut |bytes| {
                validate_recovered_stress_array_record(seed, sample, bytes)?;
                matched = true;
                Ok(())
            })?;
            ensure!(
                matched,
                "sampled object {} did not map recovered record bytes",
                sample.object_id,
            );
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn validate_recovered_stress_array_record(
        seed: u64,
        sample: &ObjectRecoverySample,
        actual: &[u8],
    ) -> Result<()> {
        let element_count = deterministic_array_element_count(sample.requested_logical_bytes)?;
        let expected_len = deterministic_array_record_len(element_count)?;
        if actual.len() != expected_len {
            bail!(
                "recovered record mismatch for sampled object {} requested_bytes={} recovered_len={} expected_len={}",
                sample.object_id,
                sample.requested_logical_bytes,
                actual.len(),
                expected_len
            );
        }

        let header = TxObjectHeader {
            record_len: u64::try_from(expected_len).context("record length exceeds u64")?,
            object_id: sample.object_id,
            version: 1,
            kind: ObjectKind::Array as u16,
            flags: 0,
            type_layout_id: STRESS_ARRAY_TYPE_LAYOUT_ID,
        };
        if let Some(mismatch_offset) =
            first_mismatch_in_expected_slice(actual, 0, &header.as_bytes())?
        {
            bail_stress_array_record_mismatch(seed, sample, actual, expected_len, mismatch_offset)?;
        }

        let object_header_len = core::mem::size_of::<TxObjectHeader>();
        let array_len = u32::try_from(element_count).context("array length exceeds u32")?;
        if let Some(mismatch_offset) =
            first_mismatch_in_expected_slice(actual, object_header_len, &array_len.to_le_bytes())?
        {
            bail_stress_array_record_mismatch(seed, sample, actual, expected_len, mismatch_offset)?;
        }

        let payload_start = core::mem::size_of::<TxArrayHeader>();
        for offset in object_header_len + core::mem::size_of::<u32>()..payload_start {
            if actual[offset] != 0 {
                bail_stress_array_record_mismatch(seed, sample, actual, expected_len, offset)?;
            }
        }

        let mut state = mix64(seed ^ sample.object_id.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        for element_index in 0..element_count {
            state = mix64(
                state
                    ^ u64::try_from(element_index)
                        .unwrap()
                        .wrapping_mul(0xd1b5_4a32_d192_ed03),
            );
            let element_offset = payload_start
                .checked_add(
                    element_index
                        .checked_mul(20)
                        .context("deterministic array element offset overflow")?,
                )
                .context("deterministic array payload offset overflow")?;
            let expected = object_value_i64_bytes(state as i64)?;
            if let Some(mismatch_offset) =
                first_mismatch_in_expected_slice(actual, element_offset, &expected)?
            {
                bail_stress_array_record_mismatch(
                    seed,
                    sample,
                    actual,
                    expected_len,
                    mismatch_offset,
                )?;
            }
        }

        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn first_mismatch_in_expected_slice(
        actual: &[u8],
        start: usize,
        expected: &[u8],
    ) -> Result<Option<usize>> {
        let end = start
            .checked_add(expected.len())
            .context("expected comparison range overflow")?;
        let actual = actual
            .get(start..end)
            .context("expected comparison range exceeds actual record length")?;
        Ok(actual
            .iter()
            .zip(expected.iter())
            .position(|(actual, expected)| actual != expected)
            .map(|offset| start + offset))
    }

    #[cfg(target_os = "linux")]
    fn bail_stress_array_record_mismatch(
        seed: u64,
        sample: &ObjectRecoverySample,
        actual: &[u8],
        expected_len: usize,
        mismatch_offset: usize,
    ) -> Result<()> {
        bail!(
            "recovered record mismatch for sampled object {} requested_bytes={} {}",
            sample.object_id,
            sample.requested_logical_bytes,
            describe_stress_array_record_mismatch(
                seed,
                sample.object_id,
                actual,
                expected_len,
                mismatch_offset,
            )?
        )
    }

    #[cfg(target_os = "linux")]
    fn describe_stress_array_record_mismatch(
        seed: u64,
        object_id: u64,
        actual: &[u8],
        expected_len: usize,
        mismatch_offset: usize,
    ) -> Result<String> {
        let window_start = mismatch_offset.saturating_sub(4);
        let window_end = (mismatch_offset + 5).min(expected_len).min(actual.len());
        let mut expected_window = Vec::with_capacity(window_end.saturating_sub(window_start));
        for offset in window_start..window_end {
            expected_window.push(expected_stress_array_record_byte(
                seed,
                object_id,
                expected_len,
                offset,
            )?);
        }
        Ok(format!(
            "len={} first_mismatch_offset={} expected_window[{}..{}]={} actual_window[{}..{}]={}",
            expected_len,
            mismatch_offset,
            window_start,
            window_end,
            format_byte_window(&expected_window),
            window_start,
            window_end,
            format_byte_window(&actual[window_start..window_end]),
        ))
    }

    #[cfg(target_os = "linux")]
    fn expected_stress_array_record_byte(
        seed: u64,
        object_id: u64,
        expected_len: usize,
        offset: usize,
    ) -> Result<u8> {
        let element_count = (expected_len
            .checked_sub(core::mem::size_of::<TxArrayHeader>())
            .context("deterministic array record is shorter than array header")?)
            / 20;
        let header = TxObjectHeader {
            record_len: u64::try_from(expected_len).context("record length exceeds u64")?,
            object_id,
            version: 1,
            kind: ObjectKind::Array as u16,
            flags: 0,
            type_layout_id: STRESS_ARRAY_TYPE_LAYOUT_ID,
        };
        let object_header_len = core::mem::size_of::<TxObjectHeader>();
        if offset < object_header_len {
            return Ok(header.as_bytes()[offset]);
        }
        let array_length_end = object_header_len + core::mem::size_of::<u32>();
        if offset < array_length_end {
            let array_len = u32::try_from(element_count).context("array length exceeds u32")?;
            return Ok(array_len.to_le_bytes()[offset - object_header_len]);
        }
        let payload_start = core::mem::size_of::<TxArrayHeader>();
        if offset < payload_start {
            return Ok(0);
        }
        let payload_offset = offset - payload_start;
        let element_index = payload_offset / 20;
        let element_byte = payload_offset % 20;
        Ok(deterministic_array_element_bytes(seed, object_id, element_index)?[element_byte])
    }

    fn describe_record_byte_mismatch(expected: &[u8], actual: &[u8]) -> String {
        if expected.len() != actual.len() {
            return format!(
                "recovered_len={} expected_len={}",
                actual.len(),
                expected.len()
            );
        }

        let mismatch_offset = expected
            .iter()
            .zip(actual.iter())
            .position(|(expected, actual)| expected != actual)
            .unwrap_or(expected.len());
        let window_start = mismatch_offset.saturating_sub(4);
        let window_end = (mismatch_offset + 5).min(expected.len());
        format!(
            "len={} first_mismatch_offset={} expected_window[{}..{}]={} actual_window[{}..{}]={}",
            expected.len(),
            mismatch_offset,
            window_start,
            window_end,
            format_byte_window(&expected[window_start..window_end]),
            window_start,
            window_end,
            format_byte_window(&actual[window_start..window_end]),
        )
    }

    fn format_byte_window(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[cfg(target_os = "linux")]
    fn stress_array_type_layout() -> PersistentTypeLayout {
        PersistentTypeLayout::Array {
            id: TypeLayoutId::new(STRESS_ARRAY_TYPE_LAYOUT_ID).unwrap(),
            fingerprint: 0xda7a_6600_0000_0020,
            element_size: 20,
            element_kind: TraceSlotKind::Scalar,
        }
    }

    #[cfg(target_os = "linux")]
    fn run_dax_linear_stress(
        root: &Path,
        root_index: usize,
        target_bytes: u64,
        seed: u64,
    ) -> Result<LinearStressStats> {
        let wasm_page_size = 64 * 1024;
        let populate_start = std::time::Instant::now();
        let mut chunk = vec![0u8; STRESS_LINEAR_CHUNK_BYTES];
        let mut paths = Vec::new();
        let mut committed_bytes = 0u64;
        let mut pages = 0u64;
        let mut segment_index = 0usize;
        while committed_bytes < target_bytes {
            let segment_bytes = (target_bytes - committed_bytes).min(STRESS_LINEAR_SEGMENT_BYTES);
            let path = root.join(format!(
                "dax-pmem-stress-linear-{}-{}-{}.tmemory",
                std::process::id(),
                root_index,
                segment_index
            ));
            remove_file_if_exists(&path)?;

            let config = crate::runtime::transaction::TransactionConfig::with_dax_pmem_fsdax_path(
                path.clone(),
            )?;
            let mut tmemory = TMemory::new(config, 0, None)?;
            let segment_pages = segment_bytes.div_ceil(wasm_page_size);
            if segment_pages > 0 {
                tmemory.grow_to_pages(segment_pages)?;
            }

            let mut segment_written = 0u64;
            while segment_written < segment_bytes {
                let remaining = segment_bytes - segment_written;
                let chunk_len = usize::try_from(remaining.min(STRESS_LINEAR_CHUNK_BYTES as u64))
                    .context("linear chunk length exceeds usize")?;
                let global_offset = committed_bytes
                    .checked_add(segment_written)
                    .context("linear global write offset overflow")?;
                fill_deterministic_linear_bytes(
                    &mut chunk[..chunk_len],
                    seed,
                    u64::try_from(root_index).unwrap(),
                    global_offset,
                );
                tmemory.commit_range(
                    usize::try_from(segment_written)
                        .context("linear write offset exceeds usize")?,
                    &chunk[..chunk_len],
                )?;
                segment_written += u64::try_from(chunk_len).unwrap();
            }
            drop(tmemory);

            pages = pages
                .checked_add(segment_pages)
                .context("linear page counter overflow")?;
            committed_bytes = committed_bytes
                .checked_add(segment_bytes)
                .context("linear committed byte counter overflow")?;
            paths.push(path);
            segment_index += 1;
        }
        let populate_elapsed = populate_start.elapsed();

        let reopen_start = std::time::Instant::now();
        let mut sample_count = 0usize;
        let mut segment_base = 0u64;
        for path in &paths {
            let segment_bytes = (target_bytes - segment_base).min(STRESS_LINEAR_SEGMENT_BYTES);
            let segment_pages = segment_bytes.div_ceil(wasm_page_size);
            let config =
                crate::runtime::transaction::TransactionConfig::with_dax_pmem_existing_fsdax_path(
                    path.clone(),
                )?;
            let reopened = if segment_pages == 0 {
                TMemory::new(config, 0, None)?
            } else {
                TMemory::new(config, segment_pages, Some(segment_pages))?
            };
            let sample_offsets = linear_sample_offsets(segment_bytes);
            sample_count = sample_count
                .checked_add(sample_offsets.len())
                .context("linear sample count overflow")?;
            for offset in &sample_offsets {
                let len = linear_sample_len(segment_bytes, *offset)?;
                if len == 0 {
                    continue;
                }
                let range_start =
                    usize::try_from(*offset).context("linear sample offset exceeds usize")?;
                let range_end = range_start
                    .checked_add(len)
                    .context("linear sample range end overflow")?;
                let bytes = reopened.read_committed(range_start..range_end)?;
                let mut expected = vec![0u8; len];
                let global_offset = segment_base
                    .checked_add(*offset)
                    .context("linear global sample offset overflow")?;
                fill_deterministic_linear_bytes(
                    &mut expected,
                    seed,
                    u64::try_from(root_index).unwrap(),
                    global_offset,
                );
                ensure!(
                    bytes == expected,
                    "linear DAX PMEM sample mismatch at offset {global_offset}"
                );
            }
            segment_base = segment_base
                .checked_add(segment_bytes)
                .context("linear segment base overflow")?;
        }
        let reopen_validate_elapsed = reopen_start.elapsed();

        Ok(LinearStressStats {
            paths,
            committed_bytes,
            pages,
            sample_count,
            populate_elapsed,
            reopen_validate_elapsed,
        })
    }

    #[cfg(target_os = "linux")]
    fn linear_sample_offsets(total_bytes: u64) -> Vec<u64> {
        if total_bytes == 0 {
            return Vec::new();
        }
        let mut offsets = Vec::new();
        offsets.push(0);
        offsets.push(total_bytes / 2);
        offsets.push(total_bytes.saturating_sub(STRESS_LINEAR_SAMPLE_BYTES as u64));
        if total_bytes > STRESS_LINEAR_SAMPLE_STRIDE_BYTES {
            let mut offset = 0u64;
            while offset < total_bytes {
                offsets.push(offset);
                offset = offset.saturating_add(STRESS_LINEAR_SAMPLE_STRIDE_BYTES);
            }
        }
        offsets.sort_unstable();
        offsets.dedup();
        offsets
            .into_iter()
            .map(|offset| offset.min(total_bytes.saturating_sub(1)))
            .collect()
    }

    #[cfg(target_os = "linux")]
    fn linear_sample_len(total_bytes: u64, offset: u64) -> Result<usize> {
        ensure!(
            offset <= total_bytes,
            "linear sample offset {offset} exceeds total bytes {total_bytes}"
        );
        usize::try_from((total_bytes - offset).min(STRESS_LINEAR_SAMPLE_BYTES as u64))
            .context("linear sample length exceeds usize")
    }

    #[cfg(target_os = "linux")]
    fn fill_deterministic_linear_bytes(bytes: &mut [u8], seed: u64, root_index: u64, offset: u64) {
        for (index, byte) in bytes.iter_mut().enumerate() {
            let absolute_offset = offset + u64::try_from(index).unwrap();
            *byte = linear_pattern_byte(seed, root_index, absolute_offset);
        }
    }

    #[cfg(target_os = "linux")]
    fn linear_pattern_byte(seed: u64, root_index: u64, absolute_offset: u64) -> u8 {
        let mixed = mix64(seed ^ root_index.wrapping_mul(0x517c_c1b7_2722_0a95) ^ absolute_offset);
        mixed as u8
    }

    #[cfg(target_os = "linux")]
    fn mix64(mut value: u64) -> u64 {
        value ^= value >> 30;
        value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value ^= value >> 27;
        value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    #[cfg(target_os = "linux")]
    fn remove_file_if_exists(path: &Path) -> Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
        }
    }

    #[cfg(target_os = "linux")]
    fn bytes_to_mib(bytes: u64) -> u64 {
        bytes / (1024 * 1024)
    }

    #[cfg(target_os = "linux")]
    fn mib_per_sec(bytes: u64, elapsed: std::time::Duration) -> f64 {
        if bytes == 0 {
            return 0.0;
        }
        (bytes as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64().max(1e-9)
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires real fsdax PMEM and WASMTIME_TEST_REAL_PMEM=1"]
    fn dax_pmem_fsdax_durable_log_reuses_retired_object_chunk_after_reopen() {
        let file_name = format!("dax-pmem-object-gc-{}.log", std::process::id());
        let Some(path) = real_pmem_durable_log_test_path(&file_name) else {
            eprintln!("set WASMTIME_TEST_REAL_PMEM=1 to run this test");
            return;
        };
        let _ = std::fs::remove_file(&path);
        let first_marker;

        {
            let mut log = TxDurableLog::create_dax_pmem_fsdax_for_test(&path, 32).unwrap();
            log.ensure_type_layout(&test_struct_type_layout()).unwrap();
            first_marker = {
                let mut sink = log.stream_sink(12);
                let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
                let marker = publisher
                    .publish_object_publication_before_commit(&test_struct_publication(41, 7, 9))
                    .unwrap();
                publisher.publish_commit_lp(marker).unwrap();
                marker
            };

            let recovered = log.recover_region_snapshot().unwrap().unwrap();
            let first_winner = recovered
                .committed_object_winners()
                .unwrap()
                .into_iter()
                .find(|winner| winner.object_id == 41)
                .unwrap();
            let retired = log
                .retire_whole_dead_object_chunks(
                    &[],
                    &[PersistentRecoveredRecordLocation {
                        object_id: crate::runtime::transaction::ObjectId { object_index: 41 },
                        version: first_winner.version,
                        data_block: first_winner.data_block,
                        data_offset: first_winner.data_offset,
                        record_len: first_winner.record_len,
                    }],
                )
                .unwrap();
            assert_eq!(retired, vec![first_marker.chunk_start_block]);
        }

        {
            let mut log = TxDurableLog::open_dax_pmem_fsdax_for_test(&path, 32).unwrap();
            let second_marker = {
                let mut sink = log.stream_sink(13);
                let mut publisher = StreamPublisher::new_for_test(&mut sink, 13, 13);
                let marker = publisher
                    .publish_object_publication_before_commit(&test_struct_publication(42, 1, 11))
                    .unwrap();
                publisher.publish_commit_lp(marker).unwrap();
                marker
            };

            assert_eq!(
                second_marker.chunk_start_block,
                first_marker.chunk_start_block
            );
            assert_eq!(
                second_marker.data_block_generation,
                first_marker.data_block_generation + 1
            );

            let winners = log
                .recover_region_snapshot()
                .unwrap()
                .unwrap()
                .committed_object_winners()
                .unwrap();
            assert!(winners.iter().any(|winner| winner.object_id == 42));
            assert!(!winners.iter().any(|winner| winner.object_id == 41));
        }

        let _ = std::fs::remove_file(path);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires real fsdax PMEM, WASMTIME_TEST_REAL_PMEM=1, and WASMTIME_DAX_STRESS_BYTES"]
    fn dax_pmem_fsdax_persistent_data_stress() {
        if std::env::var("WASMTIME_TEST_REAL_PMEM").ok().as_deref() != Some("1") {
            eprintln!("set WASMTIME_TEST_REAL_PMEM=1 to run this test");
            return;
        }
        if std::env::var_os("WASMTIME_DAX_STRESS_BYTES").is_none() {
            eprintln!("set WASMTIME_DAX_STRESS_BYTES to the per-root byte budget");
            return;
        }

        let config = match dax_pmem_stress_config_from_env() {
            Ok(config) => config,
            Err(err) => {
                eprintln!("{err}");
                return;
            }
        };

        dax_pmem_fsdax_stress_impl(&config).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dax_stress_report_includes_recovery_workers() {
        let object = ObjectStressStats {
            path: std::path::PathBuf::from("/tmp/object.log"),
            payload_blocks: 17,
            object_count: 23,
            sampled_object_count: 5,
            bucket_counts: [1, 2, 3, 4, 5],
            requested_logical_bytes: 256 * 1024 * 1024,
            encoded_bytes: 128 * 1024 * 1024,
            populate_elapsed: std::time::Duration::from_secs(2),
            recovery_elapsed: std::time::Duration::from_millis(250),
            recovery_samples: Vec::new(),
            recovery_workers: 7,
            recovered_object_count: 23,
        };
        let linear = LinearStressStats {
            paths: vec![std::path::PathBuf::from("/tmp/linear.log")],
            committed_bytes: 64 * 1024 * 1024,
            pages: 42,
            sample_count: 9,
            populate_elapsed: std::time::Duration::from_secs(4),
            reopen_validate_elapsed: std::time::Duration::from_millis(500),
        };

        let line = format_dax_stress_line(
            3,
            1024 * 1024 * 1024,
            400 * 1024 * 1024,
            500 * 1024 * 1024,
            124 * 1024 * 1024,
            &object,
            &linear,
        );

        assert!(line.contains("rec=0.250s rec_workers=7 lin=16.0MiB/s"));
        assert!(line.contains("objects=23 recovered=23"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dax_stress_line_reports_multi_region_recovery_workers() {
        let regions = vec![
            RegionRecoveryStatsForTest {
                root_index: 0,
                numa_node: Some(0),
                workers: 16,
                recovered_bytes: 512 * 1024 * 1024,
                elapsed: std::time::Duration::from_secs(1),
            },
            RegionRecoveryStatsForTest {
                root_index: 1,
                numa_node: Some(1),
                workers: 16,
                recovered_bytes: 512 * 1024 * 1024,
                elapsed: std::time::Duration::from_secs(1),
            },
        ];

        let line = format_multi_region_dax_recovery_line_for_test(
            &regions,
            std::time::Duration::from_millis(250),
            std::time::Duration::from_millis(1250),
        );

        assert!(line.contains("region_workers=[16,16]"));
        assert!(line.contains("total_workers=32"));
        assert!(line.contains("worker_budget=planned"));
        assert!(line.contains("numa=[0,1]"));
        assert!(line.contains("region_recovery_elapsed=["));
        assert!(line.contains("region_recovery_max=1.000s"));
        assert!(line.contains("merge_elapsed=0.250s"));
        assert!(line.contains("total_recovery_elapsed=1.250s"));
        assert!(line.contains("total_recovery_mibps=819.2"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dax_stress_multi_region_object_log_configs_use_existing_file_lengths() {
        let temp = tempfile::tempdir().unwrap();
        let pmem0 = temp.path().join("pmem0");
        let pmem1 = temp.path().join("pmem1");
        std::fs::create_dir_all(&pmem0).unwrap();
        std::fs::create_dir_all(&pmem1).unwrap();

        let path0 = pmem0.join("object-0.log");
        let path1 = pmem1.join("object-1.log");
        std::fs::File::create(&path0)
            .unwrap()
            .set_len(16 * 1024 * 1024)
            .unwrap();
        std::fs::File::create(&path1)
            .unwrap()
            .set_len(32 * 1024 * 1024)
            .unwrap();

        let regions = vec![
            MultiRegionDaxObjectLogForTest {
                root_index: 0,
                path: path0.clone(),
                numa_node: infer_dax_stress_numa_node(&pmem0),
                object_count: 0,
                encoded_bytes: 0,
                seed: 0,
                recovery_samples: Vec::new(),
            },
            MultiRegionDaxObjectLogForTest {
                root_index: 1,
                path: path1.clone(),
                numa_node: infer_dax_stress_numa_node(&pmem1),
                object_count: 0,
                encoded_bytes: 0,
                seed: 0,
                recovery_samples: Vec::new(),
            },
        ];

        let configs = build_multi_region_dax_object_log_configs_for_test(&regions).unwrap();

        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].path, path0);
        assert_eq!(configs[0].reserved_len, 16 * 1024 * 1024);
        assert_eq!(configs[0].numa_node, Some(0));
        assert_eq!(configs[1].path, path1);
        assert_eq!(configs[1].reserved_len, 32 * 1024 * 1024);
        assert_eq!(configs[1].numa_node, Some(1));
        assert!(configs[0].address_base < configs[1].address_base);
        assert!(configs[0].address_base + configs[0].reserved_len <= configs[1].address_base);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dax_stress_object_ids_are_root_unique() {
        assert_eq!(dax_stress_object_id(0, 1).unwrap(), 1);
        let second_root_first = dax_stress_object_id(1, 1).unwrap();
        let second_root_second = dax_stress_object_id(1, 2).unwrap();
        assert!(second_root_first > dax_stress_object_id(0, 2).unwrap());
        assert_eq!(second_root_second, second_root_first + 1);
    }

    #[test]
    fn file_backed_recovery_handles_many_large_object_records_without_materializing_bytes()
    -> Result<()> {
        let temp = tempfile::Builder::new()
            .prefix("wasmtime-zero-copy-recovery")
            .tempdir()?;
        let path = temp.path().join("region.bin");
        let mut log = TxDurableLog::create_file_backed(&path, 256)?;
        let layout = scalar_struct_type_layout_for_test(9001, 8192);
        log.ensure_type_layout(&layout)?;
        let mut expected_record_lens = Vec::new();
        let mut sampled_expected_records = BTreeMap::new();
        let sampled_object_ids = [0u64, 17, 255, 511];

        for object_id in 0..512u64 {
            let object_record = large_recovery_record_for_test(object_id, layout.id().get())?;
            expected_record_lens.push(u64::try_from(object_record.len())?);
            if sampled_object_ids.contains(&object_id) {
                sampled_expected_records.insert(object_id, object_record.clone());
            }
            let publication = PendingPublication::persistent_object(
                PackedGranuleDomain::TStruct,
                object_id,
                1,
                layout.id().get(),
                object_record,
            )?;
            publish_single_object_publication_for_test(
                &mut log,
                u32::try_from(object_id)? + 1,
                publication,
            )?;
        }

        let recovered = log.recover_region_snapshot()?.unwrap();
        let winners = recovered.committed_object_winners()?;
        assert_eq!(winners.len(), 512);
        let recovered_object_ids = winners
            .iter()
            .map(|winner| winner.object_id)
            .collect::<Vec<_>>();
        assert_eq!(recovered_object_ids, (0..512).collect::<Vec<_>>());
        assert_eq!(
            winners
                .iter()
                .map(|winner| winner.record_len)
                .collect::<Vec<_>>(),
            expected_record_lens
        );
        let mapped_source = recovered
            .mapped_region_source()
            .context("file-backed recovery should expose a mapped region source")?;
        let winners_by_id = winners
            .iter()
            .map(|winner| (winner.object_id, winner))
            .collect::<BTreeMap<_, _>>();
        for (object_id, expected_record) in &sampled_expected_records {
            let winner = winners_by_id.get(object_id).with_context(|| {
                format!("missing recovered winner for sampled object {object_id}")
            })?;
            assert_recovered_winner_matches_expected_record(
                winner,
                mapped_source,
                expected_record,
            )?;
        }
        Ok(())
    }

    fn large_recovery_record_for_test(object_id: u64, type_layout_id: u32) -> Result<Vec<u8>> {
        let word_count = 512usize
            + usize::try_from((object_id % 11) * 37).context("word count conversion overflow")?;
        let payload = ObjectPayload::Struct(
            (0..word_count)
                .map(|index| {
                    let pattern = object_id
                        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                        .rotate_left(17)
                        ^ u64::try_from(index)
                            .unwrap()
                            .wrapping_mul(0xd1b5_4a32_d192_ed03)
                            .rotate_right(9);
                    ObjectValue::I32(pattern as i32)
                })
                .collect(),
        );
        encode_object_record_for_test(object_id, 1, type_layout_id, &payload)
    }

    fn assert_recovered_winner_matches_expected_record(
        winner: &RecoveredObjectWinner,
        source: &dyn crate::runtime::vm::block_region::MappedRegionSource,
        expected_record: &[u8],
    ) -> Result<()> {
        let mut matched = false;
        winner.with_source_record_bytes(source, &mut |bytes| {
            if bytes != expected_record {
                bail!(
                    "recovered record mismatch for sampled object {} {}",
                    winner.object_id,
                    describe_record_byte_mismatch(expected_record, bytes),
                );
            }
            matched = true;
            Ok(())
        })?;
        ensure!(
            matched,
            "sampled object {} did not map recovered record bytes",
            winner.object_id,
        );
        Ok(())
    }

    #[test]
    fn file_backed_transaction_log_separates_object_and_tmemory_data_streams() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx-log.bin");
        let mut log = TxDurableLog::create_file_backed(&path, 32).unwrap();
        log.ensure_type_layout(&test_struct_type_layout()).unwrap();
        let undo = PendingGranuleUndo::tmemory(0x1000_0000_0000_002a, 13, vec![1, 2, 3, 4]);
        let publication = PendingPublication::persistent_object_for_test(
            PackedGranuleDomain::TStruct,
            41,
            7,
            TEST_STRUCT_TYPE_LAYOUT_ID,
            &encode_object_record_for_test(
                41,
                7,
                TEST_STRUCT_TYPE_LAYOUT_ID,
                &ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
            )
            .unwrap(),
        );

        {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            publisher
                .publish_tmemory_undo_before_in_place_write(&undo)
                .unwrap();
            publisher
                .publish_object_publication_before_commit(&publication)
                .unwrap();
        }

        let entries = log.log_entries_for_test(12);
        assert_eq!(entries[0].role().unwrap(), TxLogEntryRole::TMemoryUndo);
        assert_eq!(entries[1].role().unwrap(), TxLogEntryRole::TObjectPub);
        let tmemory_data_stream = log
            .data_chunk_stream_id_for_data_block_for_test(entries[0].data_block)
            .unwrap();
        let object_data_stream = log
            .data_chunk_stream_id_for_data_block_for_test(entries[1].data_block)
            .unwrap();

        assert_ne!(tmemory_data_stream, object_data_stream);
    }

    #[test]
    fn file_backed_transaction_log_reopens_and_appends_to_existing_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx-log.bin");
        let first = PendingGranuleUndo::tmemory(0x1000_0000_0000_002a, 13, vec![1, 2, 3, 4]);
        {
            let mut log = TxDurableLog::create_file_backed(&path, 32).unwrap();
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            publisher
                .publish_tmemory_undo_before_in_place_write(&first)
                .unwrap();
        }

        let second = PendingGranuleUndo::tmemory(0x1000_0000_1000_002a, 8, vec![5, 6, 7, 8]);
        {
            let mut log = TxDurableLog::open_file_backed(&path).unwrap();
            let final_marker = {
                let mut sink = log.stream_sink(13);
                let mut publisher = StreamPublisher::new_for_test(&mut sink, 13, 13);
                publisher
                    .publish_tmemory_undo_before_in_place_write(&second)
                    .unwrap()
            };
            let mut sink = log.stream_sink(13);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 13, 13);
            publisher.publish_commit_lp(final_marker).unwrap();
        }

        let recovered = TxDurableLog::recover_file_backed_for_test(&path).unwrap();
        assert_eq!(recovered.tmemory_undo_rollbacks.len(), 1);
        assert_eq!(
            recovered.tmemory_undo_rollbacks[0].old_granule_bytes,
            vec![1, 2, 3, 4]
        );
    }

    #[test]
    fn file_backed_logs_sharing_allocator_lock_allocate_distinct_undo_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx-log.bin");
        let allocator_lock = Arc::new(Mutex::new(()));

        let first = PendingGranuleUndo::tmemory(0x1000_0000_0000_002a, 13, vec![1, 2, 3, 4]);
        {
            let mut log = TxDurableLog::create_file_backed_with_allocator_lock(
                &path,
                64,
                allocator_lock.clone(),
            )
            .unwrap();
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 12, 12);
            publisher
                .publish_tmemory_undo_before_in_place_write(&first)
                .unwrap();
        }

        let second = PendingGranuleUndo::tmemory(0x1000_0000_1000_002a, 8, vec![5, 6, 7, 8]);
        {
            let mut log =
                TxDurableLog::open_file_backed_with_allocator_lock(&path, allocator_lock).unwrap();
            let mut sink = log.stream_sink(13);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 13, 13);
            publisher
                .publish_tmemory_undo_before_in_place_write(&second)
                .unwrap();
        }

        let reopened = TxDurableLog::open_file_backed(&path).unwrap();
        let first_entries = reopened.log_entries_for_test(12);
        let second_entries = reopened.log_entries_for_test(13);

        assert_eq!(first_entries.len(), 1);
        assert_eq!(second_entries.len(), 1);
        assert_ne!(first_entries[0].data_block, second_entries[0].data_block);

        let recovered = TxDurableLog::recover_file_backed_for_test(&path).unwrap();
        assert_eq!(recovered.tmemory_undo_rollbacks.len(), 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn file_backed_multi_region_durable_log_test_constructors_expose_region_count() {
        let dir = tempfile::tempdir().unwrap();
        let reserved_len = 32 * crate::runtime::vm::block_region::BLOCK_SIZE;
        let regions = vec![
            TMemoryRegionConfig::new_for_test(
                dir.path().join("tx-log-a.bin"),
                0x6000_0000_0000,
                reserved_len,
            ),
            TMemoryRegionConfig::new_for_test(
                dir.path().join("tx-log-b.bin"),
                0x6000_0200_0000,
                reserved_len,
            ),
        ];

        let log = TxDurableLog::create_file_backed_regions_for_test(regions.clone()).unwrap();
        assert_eq!(log.region_count_for_test(), 2);
        drop(log);

        let reopened = TxDurableLog::open_file_backed_regions_for_test(regions).unwrap();
        assert_eq!(reopened.region_count_for_test(), 2);
    }

    #[test]
    fn file_backed_open_retires_committed_linear_undo_chunks_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx-log.bin");
        crate::runtime::vm::block_region::create_file_backed_region_image(&path, 64).unwrap();
        crate::runtime::vm::block_region::publish_committed_tmemory_undo_for_test(
            &path,
            12,
            0x1000_0000_0000_002a,
            13,
            &[1, 2, 3, 4],
        )
        .unwrap();

        let (old_chunk_start, old_generation) = {
            let recovered = TxDurableLog::recover_file_backed_for_test(&path).unwrap();
            assert!(recovered.tmemory_undo_rollbacks.is_empty());
            first_active_linear_undo_chunk_for_test(&path)
        };

        let mut log = TxDurableLog::open_file_backed(&path).unwrap();
        let undo = PendingGranuleUndo::tmemory(0x1000_0000_0000_002b, 14, vec![5, 6, 7, 8]);
        let marker = {
            let mut sink = log.stream_sink(13);
            let mut publisher = StreamPublisher::new_for_test(&mut sink, 13, 13);
            publisher
                .publish_tmemory_undo_before_in_place_write(&undo)
                .unwrap()
        };

        assert_eq!(marker.chunk_start_block, old_chunk_start);
        assert_eq!(marker.data_block_generation, old_generation + 1);
    }

    #[test]
    fn encodes_tmemory_granule_record() {
        let publication =
            PendingPublication::tmemory_for_test(0x1000_0000_0000_0001, 5, &[1, 2, 3, 4]);
        let bytes = encode_data_record(&publication).unwrap();
        assert_eq!(u16::from_le_bytes(bytes[12..14].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 5);
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 4);
    }

    #[test]
    fn tmemory_undo_record_encodes_old_granule_bytes() {
        let undo = PendingGranuleUndo::tmemory(0x1000_0000_0000_002a, 13, vec![9, 8, 7, 6]);
        let bytes = encode_undo_data_record(&undo).unwrap();

        assert_eq!(
            u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            0x1000_0000_0000_002a
        );
        assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 13);
        assert_eq!(
            u16::from_le_bytes(bytes[12..14].try_into().unwrap()),
            PackedGranuleDomain::TMemory as u16
        );
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 4);
        assert_eq!(&bytes[24..], &[9, 8, 7, 6]);
    }

    #[test]
    fn tmemory_undo_record_is_not_a_publication() {
        let undo = PendingGranuleUndo::tmemory(0x1000_0000_0000_002a, 13, vec![1, 2, 3, 4]);
        let publication = PendingPublication::tmemory_for_test(
            undo.logical_id,
            undo.version,
            &undo.old_granule_bytes,
        );

        assert_ne!(
            encode_undo_data_record(&undo).unwrap(),
            encode_data_record(&publication).unwrap()
        );
    }

    #[test]
    fn encodes_object_record_with_tx_object_header() {
        let bytes = encode_object_record_for_test(
            41,
            7,
            12,
            &ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
        )
        .unwrap();
        let header = TxObjectHeader::read_from_prefix(&bytes).unwrap();
        assert_eq!(header.object_id, 41);
        assert_eq!(header.version, 7);
        assert_eq!(header.type_layout_id, 12);
    }

    #[test]
    fn object_publication_builds_object_logical_id() {
        let pub_ = PendingPublication::persistent_object_for_test(
            PackedGranuleDomain::TStruct,
            41,
            7,
            12,
            &[1, 2, 3],
        );
        let (domain, object_id) = unpack_object_granule_id(pub_.logical_id).unwrap();
        assert_eq!(domain, PackedGranuleDomain::TStruct);
        assert_eq!(object_id, 41);
        assert_eq!(pub_.version, 7);
        assert_eq!(pub_.type_layout_id, 12);
    }

    #[test]
    fn persistent_root_publication_encodes_global_object_id() {
        let publication = PendingPublication::persistent_global_root(0, 7, [Some(41_u64)]).unwrap();
        assert_eq!(
            publication.logical_id,
            (PackedGranuleDomain::TGlobal as u64) << 60
        );
        assert_eq!(publication.version, 7);
        assert_eq!(publication.kind, PackedGranuleDomain::TGlobal as u16);
        assert_eq!(publication.type_layout_id, 0);
        assert_eq!(publication.payload, (42_u64).to_le_bytes());
    }

    #[test]
    fn persistent_root_publication_encodes_table_object_ids() {
        let publication =
            PendingPublication::persistent_table_root(3, 9, [Some(41_u64), None, Some(99_u64)])
                .unwrap();
        let logical_id = ((PackedGranuleDomain::TTable as u64) << 60) | 3;
        assert_eq!(publication.logical_id, logical_id);
        assert_eq!(publication.version, 9);
        assert_eq!(publication.kind, PackedGranuleDomain::TTable as u16);
        assert_eq!(publication.type_layout_id, 0);
        let mut expected = Vec::new();
        expected.extend_from_slice(&42_u64.to_le_bytes());
        expected.extend_from_slice(&0_u64.to_le_bytes());
        expected.extend_from_slice(&100_u64.to_le_bytes());
        assert_eq!(publication.payload, expected);
    }

    #[test]
    fn persistent_root_publication_rejects_oversized_root_index() {
        let err =
            PendingPublication::persistent_global_root(1u64 << 60, 7, [Some(41_u64)]).unwrap_err();

        assert!(
            err.to_string()
                .contains("persistent root index does not fit in packed granule id payload"),
            "{err:?}"
        );
    }

    mod model_recovery {
        use super::*;
        use proptest::prelude::*;
        use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

        #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
        enum ModelRole {
            TObjectPub,
            TMemoryUndo,
        }

        #[derive(Clone, Debug, Eq, PartialEq)]
        struct ModelRecord {
            tx: u32,
            logical_id: u64,
            version: u32,
            role: ModelRole,
            payload_byte: u8,
            has_lp: bool,
        }

        #[derive(Clone, Debug, Default, Eq, PartialEq)]
        struct ExpectedRecovery {
            object_winners: BTreeSet<(u64, u32)>,
            tmemory_undo_rollbacks: BTreeSet<(u64, u32, Vec<u8>)>,
            tmemory_undo_rollback_count: usize,
        }

        fn expected_recovery(records: &[ModelRecord]) -> ExpectedRecovery {
            let mut records_by_tx = BTreeMap::<u32, Vec<&ModelRecord>>::new();
            for record in records {
                records_by_tx.entry(record.tx).or_default().push(record);
            }

            let mut object_versions = BTreeMap::<u64, u32>::new();
            let mut tmemory_undo_rollbacks = BTreeSet::new();
            let mut tmemory_undo_rollback_count = 0usize;

            for tx_records in records_by_tx.values() {
                let committed = tx_records.iter().any(|record| record.has_lp);
                if committed {
                    for record in tx_records {
                        if record.role != ModelRole::TObjectPub {
                            continue;
                        }
                        object_versions
                            .entry(record.logical_id)
                            .and_modify(|current| *current = (*current).max(record.version))
                            .or_insert(record.version);
                    }
                    continue;
                }

                for record in tx_records {
                    if record.role != ModelRole::TMemoryUndo {
                        continue;
                    }
                    tmemory_undo_rollbacks.insert((
                        record.logical_id,
                        record.version,
                        repeated_payload(record.payload_byte),
                    ));
                    tmemory_undo_rollback_count += 1;
                }
            }

            ExpectedRecovery {
                object_winners: object_versions.into_iter().collect(),
                tmemory_undo_rollbacks,
                tmemory_undo_rollback_count,
            }
        }

        fn object_logical_id(object_id: u64) -> u64 {
            pack_object_granule_id(PackedGranuleDomain::TStruct, object_id).unwrap()
        }

        fn tmemory_logical_id(granule_id: u64) -> u64 {
            0x1000_0000_0000_0000 | (granule_id << 12) | 0x2a
        }

        fn repeated_payload(byte: u8) -> Vec<u8> {
            vec![byte; 4]
        }

        fn build_model_records(
            tx_count: u32,
            object_id_count: u64,
            tmemory_id_count: u64,
            raw_records: Vec<(u32, bool, u64, u64, u32, u8)>,
            committed_flags: Vec<bool>,
        ) -> Vec<ModelRecord> {
            let mut records = raw_records
                .into_iter()
                .map(
                    |(
                        tx_zero_based,
                        is_object,
                        object_slot,
                        tmemory_slot,
                        version,
                        payload_byte,
                    )| {
                        let tx = tx_zero_based
                            .min(tx_count.saturating_sub(1))
                            .checked_add(1)
                            .unwrap();
                        let (logical_id, role) = if is_object {
                            let object_id = (object_slot % object_id_count).checked_add(1).unwrap();
                            (object_logical_id(object_id), ModelRole::TObjectPub)
                        } else {
                            let granule_id =
                                (tmemory_slot % tmemory_id_count).checked_add(1).unwrap();
                            (tmemory_logical_id(granule_id), ModelRole::TMemoryUndo)
                        };
                        ModelRecord {
                            tx,
                            logical_id,
                            version,
                            role,
                            payload_byte,
                            has_lp: false,
                        }
                    },
                )
                .collect::<Vec<_>>();

            records.sort_by_key(|record| record.tx);

            for tx in 1..=tx_count {
                if !committed_flags
                    .get(tx.checked_sub(1).unwrap() as usize)
                    .copied()
                    .unwrap_or(false)
                {
                    continue;
                }
                if let Some(last_record) = records.iter_mut().rev().find(|record| record.tx == tx) {
                    last_record.has_lp = true;
                }
            }

            records
        }

        fn model_history_is_supported(records: &[ModelRecord]) -> bool {
            let mut committed_object_versions = BTreeSet::new();
            let mut records_by_tx = BTreeMap::<u32, Vec<&ModelRecord>>::new();
            for record in records {
                records_by_tx.entry(record.tx).or_default().push(record);
            }

            for tx_records in records_by_tx.values() {
                if !tx_records.iter().any(|record| record.has_lp) {
                    continue;
                }
                for record in tx_records {
                    if record.role != ModelRole::TObjectPub {
                        continue;
                    }
                    if !committed_object_versions.insert((record.logical_id, record.version)) {
                        return false;
                    }
                }
            }

            true
        }

        fn model_history_strategy() -> impl Strategy<Value = Vec<ModelRecord>> {
            (1u32..=4, 1usize..=8, 1u64..=3, 1u64..=3)
                .prop_flat_map(
                    |(tx_count, total_records, object_id_count, tmemory_id_count)| {
                        (
                            Just(tx_count),
                            Just(object_id_count),
                            Just(tmemory_id_count),
                            prop::collection::vec(
                                (
                                    0u32..tx_count,
                                    any::<bool>(),
                                    0u64..object_id_count,
                                    0u64..tmemory_id_count,
                                    1u32..=4,
                                    any::<u8>(),
                                ),
                                total_records,
                            ),
                            prop::collection::vec(any::<bool>(), tx_count as usize),
                        )
                            .prop_map(
                                |(
                                    tx_count,
                                    object_id_count,
                                    tmemory_id_count,
                                    raw_records,
                                    committed_flags,
                                )| {
                                    build_model_records(
                                        tx_count,
                                        object_id_count,
                                        tmemory_id_count,
                                        raw_records,
                                        committed_flags,
                                    )
                                },
                            )
                    },
                )
                .prop_filter(
                    "committed object publications must not reuse the same logical/version pair",
                    |records| model_history_is_supported(records),
                )
        }

        fn pending_publication_for_model(record: &ModelRecord) -> PendingPublication {
            let (_, object_id) = unpack_object_granule_id(record.logical_id).unwrap();
            let payload = encode_object_record_for_test(
                object_id,
                record.version,
                TEST_STRUCT_TYPE_LAYOUT_ID,
                &ObjectPayload::Struct(vec![ObjectValue::I32(i32::from(record.payload_byte))]),
            )
            .unwrap();
            PendingPublication::persistent_object_for_test(
                PackedGranuleDomain::TStruct,
                object_id,
                record.version,
                TEST_STRUCT_TYPE_LAYOUT_ID,
                &payload,
            )
        }

        fn pending_undo_for_model(record: &ModelRecord) -> PendingGranuleUndo {
            PendingGranuleUndo::tmemory(
                record.logical_id,
                record.version,
                repeated_payload(record.payload_byte),
            )
        }

        fn recover_model_history(
            records: &[ModelRecord],
        ) -> Result<crate::runtime::vm::block_region::TransactionPersistenceRecoveredRegion>
        {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("tx-log.bin");
            let mut log = TxDurableLog::create_file_backed(&path, 64)?;
            log.ensure_type_layout(&test_struct_type_layout())?;
            let mut records_by_tx = BTreeMap::<u32, Vec<&ModelRecord>>::new();
            for record in records {
                records_by_tx.entry(record.tx).or_default().push(record);
            }

            for (tx, tx_records) in records_by_tx {
                let stream_id = 100 + tx;
                let mut final_marker = None;
                {
                    let mut sink = log.stream_sink(stream_id);
                    let mut publisher = StreamPublisher::new_for_test(&mut sink, stream_id, tx);
                    for record in tx_records {
                        let marker = match record.role {
                            ModelRole::TObjectPub => publisher
                                .publish_object_publication_before_commit(
                                    &pending_publication_for_model(record),
                                )?,
                            ModelRole::TMemoryUndo => publisher
                                .publish_tmemory_undo_before_in_place_write(
                                    &pending_undo_for_model(record),
                                )?,
                        };
                        if record.has_lp {
                            final_marker = Some(marker);
                        }
                    }
                }
                if let Some(marker) = final_marker {
                    let mut sink = log.stream_sink(stream_id);
                    let mut publisher = StreamPublisher::new_for_test(&mut sink, stream_id, tx);
                    publisher.publish_commit_lp(marker)?;
                }
            }

            drop(log);
            TxDurableLog::recover_file_backed_for_test(&path)
        }

        fn summarize_actual_recovery(
            recovered: &crate::runtime::vm::block_region::TransactionPersistenceRecoveredRegion,
        ) -> ExpectedRecovery {
            ExpectedRecovery {
                object_winners: recovered
                    .object_winners
                    .iter()
                    .map(|winner| (object_logical_id(winner.object_id), winner.version))
                    .collect(),
                tmemory_undo_rollbacks: recovered
                    .tmemory_undo_rollbacks
                    .iter()
                    .map(|rollback| {
                        (
                            rollback.logical_id,
                            rollback.version,
                            rollback.old_granule_bytes.clone(),
                        )
                    })
                    .collect(),
                tmemory_undo_rollback_count: recovered.tmemory_undo_rollbacks.len(),
            }
        }

        #[test]
        fn summarize_actual_recovery_distinguishes_rollback_payload_bytes() {
            let rollback_a =
                crate::runtime::vm::block_region::TransactionPersistenceRecoveredTMemoryUndoRollback {
                    logical_id: tmemory_logical_id(1),
                    version: 7,
                    old_granule_bytes: repeated_payload(0x11),
                };
            let rollback_b =
                crate::runtime::vm::block_region::TransactionPersistenceRecoveredTMemoryUndoRollback {
                    logical_id: rollback_a.logical_id,
                    version: rollback_a.version,
                    old_granule_bytes: repeated_payload(0x22),
                };
            let recovered_a =
                crate::runtime::vm::block_region::TransactionPersistenceRecoveredRegion {
                    winners: Vec::new(),
                    object_winners: Vec::new(),
                    root_object_ids: Vec::new(),
                    tmemory_undo_rollbacks: vec![rollback_a],
                };
            let recovered_b =
                crate::runtime::vm::block_region::TransactionPersistenceRecoveredRegion {
                    winners: Vec::new(),
                    object_winners: Vec::new(),
                    root_object_ids: Vec::new(),
                    tmemory_undo_rollbacks: vec![rollback_b],
                };

            assert_ne!(
                recovered_a.tmemory_undo_rollbacks,
                recovered_b.tmemory_undo_rollbacks
            );
            assert_ne!(
                summarize_actual_recovery(&recovered_a),
                summarize_actual_recovery(&recovered_b)
            );
        }

        #[test]
        fn model_recovery_committed_object_pub_wins_and_committed_undo_is_ignored() {
            let records = [
                ModelRecord {
                    tx: 1,
                    logical_id: tmemory_logical_id(1),
                    version: 1,
                    role: ModelRole::TMemoryUndo,
                    payload_byte: 0x0a,
                    has_lp: false,
                },
                ModelRecord {
                    tx: 1,
                    logical_id: object_logical_id(41),
                    version: 2,
                    role: ModelRole::TObjectPub,
                    payload_byte: 0x0b,
                    has_lp: true,
                },
            ];
            let expected = expected_recovery(&records);
            let recovered = recover_model_history(&records).unwrap();
            let actual = summarize_actual_recovery(&recovered);

            assert_eq!(actual.object_winners.len(), 1);
            assert_eq!(actual.tmemory_undo_rollback_count, 0);
            assert_eq!(actual, expected);
        }

        #[test]
        fn model_recovery_selects_highest_committed_object_version() {
            let records = [
                ModelRecord {
                    tx: 1,
                    logical_id: object_logical_id(41),
                    version: 2,
                    role: ModelRole::TObjectPub,
                    payload_byte: 0x21,
                    has_lp: true,
                },
                ModelRecord {
                    tx: 2,
                    logical_id: object_logical_id(41),
                    version: 9,
                    role: ModelRole::TObjectPub,
                    payload_byte: 0x22,
                    has_lp: true,
                },
                ModelRecord {
                    tx: 3,
                    logical_id: object_logical_id(41),
                    version: 7,
                    role: ModelRole::TObjectPub,
                    payload_byte: 0x23,
                    has_lp: true,
                },
            ];

            let recovered = recover_model_history(&records).unwrap();
            let actual = summarize_actual_recovery(&recovered);

            assert_eq!(
                actual.object_winners,
                BTreeSet::from([(object_logical_id(41), 9)])
            );
            assert_eq!(actual.tmemory_undo_rollback_count, 0);
        }

        #[test]
        fn model_recovery_rejects_distinct_duplicate_committed_object_version() {
            let records = [
                ModelRecord {
                    tx: 1,
                    logical_id: object_logical_id(41),
                    version: 9,
                    role: ModelRole::TObjectPub,
                    payload_byte: 0x11,
                    has_lp: true,
                },
                ModelRecord {
                    tx: 2,
                    logical_id: object_logical_id(41),
                    version: 9,
                    role: ModelRole::TObjectPub,
                    payload_byte: 0x22,
                    has_lp: true,
                },
            ];

            let err = recover_model_history(&records).unwrap_err();
            assert!(
                err.to_string()
                    .contains("duplicate committed object version")
            );
        }

        #[test]
        fn model_recovery_matches_file_backed_recovery_for_small_histories() {
            let mut runner = TestRunner::new(Config {
                cases: 48,
                failure_persistence: None,
                max_shrink_iters: 256,
                rng_seed: RngSeed::Fixed(0x5eed_0001),
                ..Config::default()
            });

            runner
                .run(&model_history_strategy(), |records| {
                    let expected = expected_recovery(&records);
                    let recovered = recover_model_history(&records)
                        .map_err(|error| TestCaseError::fail(error.to_string()))?;
                    let actual = summarize_actual_recovery(&recovered);
                    prop_assert_eq!(actual, expected);
                    Ok(())
                })
                .unwrap();
        }

        mod model_crash {
            use super::*;

            #[derive(Clone, Copy, Debug, Eq, PartialEq)]
            enum CrashCutpoint {
                BeforeAnyRecord,
                AfterUndoRecordBeforeLp,
                AfterObjectRecordBeforeLp,
                AfterAllRecordsBeforeLp,
                AfterLp,
            }

            impl CrashCutpoint {
                fn all() -> [Self; 5] {
                    [
                        Self::BeforeAnyRecord,
                        Self::AfterUndoRecordBeforeLp,
                        Self::AfterObjectRecordBeforeLp,
                        Self::AfterAllRecordsBeforeLp,
                        Self::AfterLp,
                    ]
                }
            }

            fn cutpoint_records(cutpoint: CrashCutpoint) -> Vec<ModelRecord> {
                let undo = ModelRecord {
                    tx: 1,
                    logical_id: tmemory_logical_id(7),
                    version: 1,
                    role: ModelRole::TMemoryUndo,
                    payload_byte: 0x3c,
                    has_lp: false,
                };
                let object_a = ModelRecord {
                    tx: 1,
                    logical_id: object_logical_id(41),
                    version: 2,
                    role: ModelRole::TObjectPub,
                    payload_byte: 0x4d,
                    has_lp: false,
                };
                let object_b = ModelRecord {
                    tx: 1,
                    logical_id: object_logical_id(42),
                    version: 5,
                    role: ModelRole::TObjectPub,
                    payload_byte: 0x5e,
                    has_lp: false,
                };

                match cutpoint {
                    CrashCutpoint::BeforeAnyRecord => Vec::new(),
                    CrashCutpoint::AfterUndoRecordBeforeLp => vec![undo],
                    CrashCutpoint::AfterObjectRecordBeforeLp => vec![undo, object_a],
                    CrashCutpoint::AfterAllRecordsBeforeLp => vec![undo, object_a, object_b],
                    CrashCutpoint::AfterLp => {
                        let mut object_b = object_b;
                        object_b.has_lp = true;
                        vec![undo, object_a, object_b]
                    }
                }
            }

            fn actual_recovery_at_cutpoint(cutpoint: CrashCutpoint) -> ExpectedRecovery {
                let recovered = recover_model_history(&cutpoint_records(cutpoint)).unwrap();
                summarize_actual_recovery(&recovered)
            }

            #[test]
            fn model_crash_before_lp_rolls_back_tmemory_and_drops_objects() {
                let actual = actual_recovery_at_cutpoint(CrashCutpoint::AfterAllRecordsBeforeLp);

                assert!(actual.object_winners.is_empty());
                assert_eq!(actual.tmemory_undo_rollback_count, 1);
                assert_eq!(
                    actual.tmemory_undo_rollbacks,
                    BTreeSet::from([(tmemory_logical_id(7), 1, repeated_payload(0x3c),)])
                );
            }

            #[test]
            fn model_crash_after_lp_commits_objects_and_ignores_undo() {
                let actual = actual_recovery_at_cutpoint(CrashCutpoint::AfterLp);

                assert_eq!(
                    actual.object_winners,
                    BTreeSet::from([(object_logical_id(41), 2), (object_logical_id(42), 5),])
                );
                assert!(actual.tmemory_undo_rollbacks.is_empty());
                assert_eq!(actual.tmemory_undo_rollback_count, 0);
            }

            #[test]
            fn model_crash_generated_cutpoints_match_lp_visibility_expectations() {
                for cutpoint in CrashCutpoint::all() {
                    let expected = expected_recovery(&cutpoint_records(cutpoint));
                    let actual = actual_recovery_at_cutpoint(cutpoint);

                    assert_eq!(actual, expected, "cutpoint: {cutpoint:?}");
                }
            }
        }
    }

    mod model_backend_conformance {
        use super::*;
        use std::path::PathBuf;
        use tempfile::TempDir;

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        enum BackendScenarioRecord {
            TObjectPub {
                object_id: u64,
                version: u32,
                payload_byte: u8,
            },
            TRootPub {
                logical_id: u64,
                version: u32,
                object_ids: &'static [Option<u64>],
            },
            TMemoryUndo {
                logical_id: u64,
                version: u32,
                payload_byte: u8,
            },
        }

        #[derive(Clone, Debug, Eq, PartialEq)]
        struct BackendScenarioTx {
            stream_id: u32,
            txid: u32,
            records: Vec<BackendScenarioRecord>,
            commit: bool,
        }

        #[derive(Clone, Debug, Eq, PartialEq)]
        struct BackendScenario {
            name: &'static str,
            transactions: Vec<BackendScenarioTx>,
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        enum DurableLogTestFactory {
            InMemory,
            FileBacked,
        }

        impl DurableLogTestFactory {
            fn all() -> [Self; 2] {
                [Self::InMemory, Self::FileBacked]
            }

            fn name(self) -> &'static str {
                match self {
                    Self::InMemory => "in_memory",
                    Self::FileBacked => "file_backed",
                }
            }

            fn supports_recovery(self) -> bool {
                matches!(self, Self::FileBacked)
            }

            fn create(self) -> Result<DurableLogTestHandle> {
                match self {
                    Self::InMemory => Ok(DurableLogTestHandle {
                        factory: self,
                        _tempdir: None,
                        path: None,
                        log: TxDurableLog::default(),
                    }),
                    Self::FileBacked => {
                        let tempdir = tempfile::tempdir()?;
                        let path = tempdir.path().join("tx-log.bin");
                        Ok(DurableLogTestHandle {
                            factory: self,
                            _tempdir: Some(tempdir),
                            path: Some(path.clone()),
                            log: TxDurableLog::create_file_backed(&path, 64)?,
                        })
                    }
                }
            }
        }

        #[derive(Debug)]
        struct DurableLogTestHandle {
            factory: DurableLogTestFactory,
            _tempdir: Option<TempDir>,
            path: Option<PathBuf>,
            log: TxDurableLog,
        }

        #[derive(Clone, Debug, Default, Eq, PartialEq)]
        struct BackendRecoverySummary {
            object_winners: BTreeSet<(u64, u32)>,
            root_object_ids: BTreeSet<u64>,
            tmemory_undo_rollbacks: BTreeSet<(u64, u32, Vec<u8>)>,
            tmemory_undo_rollback_count: usize,
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        struct ExpectedLogEntry {
            logical_id: u64,
            version: u32,
            txid: u32,
            role: TxLogEntryRole,
            is_final: bool,
        }

        #[derive(Debug)]
        struct BackendObservation {
            streams: BTreeMap<u32, Vec<TxLogEntry>>,
            file_backed_data_stream_ids: Option<BTreeMap<(u32, usize), u32>>,
            recovery: Option<BackendRecoverySummary>,
        }

        impl BackendScenarioRecord {
            fn logical_id(self) -> u64 {
                match self {
                    Self::TObjectPub { object_id, .. } => object_logical_id(object_id),
                    Self::TRootPub { logical_id, .. } => logical_id,
                    Self::TMemoryUndo { logical_id, .. } => logical_id,
                }
            }

            fn version(self) -> u32 {
                match self {
                    Self::TObjectPub { version, .. }
                    | Self::TRootPub { version, .. }
                    | Self::TMemoryUndo { version, .. } => version,
                }
            }

            fn role(self) -> TxLogEntryRole {
                match self {
                    Self::TObjectPub { .. } | Self::TRootPub { .. } => TxLogEntryRole::TObjectPub,
                    Self::TMemoryUndo { .. } => TxLogEntryRole::TMemoryUndo,
                }
            }

            fn payload_byte(self) -> u8 {
                match self {
                    Self::TObjectPub { payload_byte, .. }
                    | Self::TMemoryUndo { payload_byte, .. } => payload_byte,
                    Self::TRootPub { .. } => {
                        panic!("root publications do not use repeated payload bytes")
                    }
                }
            }

            fn is_object_publication(self) -> bool {
                matches!(self, Self::TObjectPub { .. })
            }

            fn root_object_ids(self) -> &'static [Option<u64>] {
                match self {
                    Self::TRootPub { object_ids, .. } => object_ids,
                    _ => &[],
                }
            }
        }

        impl DurableLogTestHandle {
            fn observe(self, scenario: &BackendScenario) -> Result<BackendObservation> {
                let DurableLogTestHandle {
                    factory,
                    _tempdir,
                    path,
                    log,
                } = self;
                let mut streams = BTreeMap::new();
                let mut file_backed_data_stream_ids = factory
                    .supports_recovery()
                    .then(BTreeMap::<(u32, usize), u32>::new);

                for tx in &scenario.transactions {
                    let entries = log.log_entries_for_test(tx.stream_id);
                    if let Some(data_stream_ids) = file_backed_data_stream_ids.as_mut() {
                        for (entry_index, entry) in entries.iter().enumerate() {
                            data_stream_ids.insert(
                                (tx.stream_id, entry_index),
                                log.data_chunk_stream_id_for_data_block_for_test(entry.data_block)?,
                            );
                        }
                    }
                    streams.insert(tx.stream_id, entries);
                }
                drop(log);

                let recovery = match path {
                    Some(path) => Some(summarize_recovery(
                        &TxDurableLog::recover_file_backed_for_test(&path)?,
                    )),
                    None => None,
                };

                let _ = _tempdir;

                Ok(BackendObservation {
                    streams,
                    file_backed_data_stream_ids,
                    recovery,
                })
            }
        }

        fn object_only_commit_scenario() -> BackendScenario {
            BackendScenario {
                name: "object_only_commit",
                transactions: vec![BackendScenarioTx {
                    stream_id: 110,
                    txid: 7,
                    records: vec![BackendScenarioRecord::TObjectPub {
                        object_id: 41,
                        version: 2,
                        payload_byte: 0x11,
                    }],
                    commit: true,
                }],
            }
        }

        fn tmemory_undo_only_commit_scenario() -> BackendScenario {
            BackendScenario {
                name: "tmemory_undo_only_commit",
                transactions: vec![BackendScenarioTx {
                    stream_id: 111,
                    txid: 8,
                    records: vec![BackendScenarioRecord::TMemoryUndo {
                        logical_id: tmemory_logical_id(1),
                        version: 5,
                        payload_byte: 0x22,
                    }],
                    commit: true,
                }],
            }
        }

        fn mixed_commit_scenario() -> BackendScenario {
            BackendScenario {
                name: "mixed_commit",
                transactions: vec![BackendScenarioTx {
                    stream_id: 112,
                    txid: 9,
                    records: vec![
                        BackendScenarioRecord::TMemoryUndo {
                            logical_id: tmemory_logical_id(2),
                            version: 3,
                            payload_byte: 0x33,
                        },
                        BackendScenarioRecord::TObjectPub {
                            object_id: 42,
                            version: 7,
                            payload_byte: 0x44,
                        },
                    ],
                    commit: true,
                }],
            }
        }

        fn mixed_loose_end_scenario() -> BackendScenario {
            BackendScenario {
                name: "mixed_loose_end",
                transactions: vec![BackendScenarioTx {
                    stream_id: 113,
                    txid: 10,
                    records: vec![
                        BackendScenarioRecord::TMemoryUndo {
                            logical_id: tmemory_logical_id(3),
                            version: 4,
                            payload_byte: 0x55,
                        },
                        BackendScenarioRecord::TObjectPub {
                            object_id: 43,
                            version: 8,
                            payload_byte: 0x66,
                        },
                    ],
                    commit: false,
                }],
            }
        }

        fn multi_stream_transaction_ids_scenario() -> BackendScenario {
            BackendScenario {
                name: "multi_stream_transaction_ids",
                transactions: vec![
                    BackendScenarioTx {
                        stream_id: 120,
                        txid: 41,
                        records: vec![BackendScenarioRecord::TObjectPub {
                            object_id: 60,
                            version: 1,
                            payload_byte: 0xa1,
                        }],
                        commit: true,
                    },
                    BackendScenarioTx {
                        stream_id: 121,
                        txid: 77,
                        records: vec![BackendScenarioRecord::TMemoryUndo {
                            logical_id: tmemory_logical_id(7),
                            version: 2,
                            payload_byte: 0xa2,
                        }],
                        commit: true,
                    },
                    BackendScenarioTx {
                        stream_id: 122,
                        txid: 88,
                        records: vec![
                            BackendScenarioRecord::TMemoryUndo {
                                logical_id: tmemory_logical_id(8),
                                version: 5,
                                payload_byte: 0xa3,
                            },
                            BackendScenarioRecord::TObjectPub {
                                object_id: 61,
                                version: 9,
                                payload_byte: 0xa4,
                            },
                        ],
                        commit: false,
                    },
                ],
            }
        }

        fn mixed_commit_with_root_publication_scenario() -> BackendScenario {
            BackendScenario {
                name: "mixed_commit_with_root_publication",
                transactions: vec![BackendScenarioTx {
                    stream_id: 123,
                    txid: 89,
                    records: vec![
                        BackendScenarioRecord::TMemoryUndo {
                            logical_id: tmemory_logical_id(9),
                            version: 1,
                            payload_byte: 0xb1,
                        },
                        BackendScenarioRecord::TObjectPub {
                            object_id: 62,
                            version: 2,
                            payload_byte: 0xb2,
                        },
                        BackendScenarioRecord::TRootPub {
                            logical_id: root_global_logical_id(0),
                            version: 3,
                            object_ids: &[Some(62)],
                        },
                    ],
                    commit: true,
                }],
            }
        }

        fn mixed_loose_end_with_root_publication_scenario() -> BackendScenario {
            BackendScenario {
                name: "mixed_loose_end_with_root_publication",
                transactions: vec![BackendScenarioTx {
                    stream_id: 124,
                    txid: 90,
                    records: vec![
                        BackendScenarioRecord::TMemoryUndo {
                            logical_id: tmemory_logical_id(10),
                            version: 1,
                            payload_byte: 0xc1,
                        },
                        BackendScenarioRecord::TObjectPub {
                            object_id: 63,
                            version: 2,
                            payload_byte: 0xc2,
                        },
                        BackendScenarioRecord::TRootPub {
                            logical_id: root_global_logical_id(0),
                            version: 3,
                            object_ids: &[Some(63)],
                        },
                    ],
                    commit: false,
                }],
            }
        }

        fn object_logical_id(object_id: u64) -> u64 {
            pack_object_granule_id(PackedGranuleDomain::TStruct, object_id).unwrap()
        }

        fn root_global_logical_id(global_index: u64) -> u64 {
            ((PackedGranuleDomain::TGlobal as u64) << 60) | global_index
        }

        fn tmemory_logical_id(granule_id: u64) -> u64 {
            0x1000_0000_0000_0000 | (granule_id << 12) | 0x2a
        }

        fn root_publication_kind(logical_id: u64) -> u16 {
            match logical_id >> 60 {
                value if value == PackedGranuleDomain::TGlobal as u64 => {
                    PackedGranuleDomain::TGlobal as u16
                }
                value if value == PackedGranuleDomain::TTable as u64 => {
                    PackedGranuleDomain::TTable as u16
                }
                other => panic!("unsupported root publication domain {other:#x}"),
            }
        }

        fn encode_root_object_ids(object_ids: &[Option<u64>]) -> Vec<u8> {
            let mut payload = Vec::with_capacity(object_ids.len() * size_of::<u64>());
            for object_id in object_ids {
                let raw = PersistentObjectRefRaw::from_optional_object_index(*object_id)
                    .unwrap()
                    .as_raw();
                payload.extend_from_slice(&raw.to_le_bytes());
            }
            payload
        }

        fn repeated_payload(byte: u8) -> Vec<u8> {
            vec![byte; 4]
        }

        fn pending_publication(record: BackendScenarioRecord) -> PendingPublication {
            match record {
                BackendScenarioRecord::TObjectPub {
                    object_id,
                    version,
                    payload_byte,
                } => {
                    let payload = encode_object_record_for_test(
                        object_id,
                        version,
                        TEST_STRUCT_TYPE_LAYOUT_ID,
                        &ObjectPayload::Struct(vec![ObjectValue::I32(i32::from(payload_byte))]),
                    )
                    .unwrap();
                    PendingPublication::persistent_object_for_test(
                        PackedGranuleDomain::TStruct,
                        object_id,
                        version,
                        TEST_STRUCT_TYPE_LAYOUT_ID,
                        &payload,
                    )
                }
                BackendScenarioRecord::TRootPub {
                    logical_id,
                    version,
                    object_ids,
                } => {
                    let payload = encode_root_object_ids(object_ids);
                    let mut publication =
                        PendingPublication::tmemory_for_test(logical_id, version, &payload);
                    publication.kind = root_publication_kind(logical_id);
                    publication
                }
                BackendScenarioRecord::TMemoryUndo { .. } => unreachable!(),
            }
        }

        fn pending_undo(record: BackendScenarioRecord) -> PendingGranuleUndo {
            let BackendScenarioRecord::TMemoryUndo {
                logical_id,
                version,
                payload_byte,
            } = record
            else {
                unreachable!();
            };
            PendingGranuleUndo::tmemory(logical_id, version, repeated_payload(payload_byte))
        }

        fn expected_stream_entries(
            scenario: &BackendScenario,
        ) -> BTreeMap<u32, Vec<ExpectedLogEntry>> {
            let mut streams = BTreeMap::new();

            for tx in &scenario.transactions {
                let mut entries = Vec::new();
                for record in &tx.records {
                    entries.push(ExpectedLogEntry {
                        logical_id: record.logical_id(),
                        version: record.version(),
                        txid: tx.txid,
                        role: record.role(),
                        is_final: false,
                    });
                }
                if tx.commit {
                    entries
                        .last_mut()
                        .expect("scenario tx must have records")
                        .is_final = true;
                }
                streams.insert(tx.stream_id, entries);
            }

            streams
        }

        fn expected_recovery(scenario: &BackendScenario) -> BackendRecoverySummary {
            let mut object_versions = BTreeMap::<u64, u32>::new();
            let mut root_versions = BTreeMap::<u64, (u32, &'static [Option<u64>])>::new();
            let mut tmemory_undo_rollbacks = BTreeSet::new();
            let mut tmemory_undo_rollback_count = 0usize;

            for tx in &scenario.transactions {
                if tx.commit {
                    for record in &tx.records {
                        if record.is_object_publication() {
                            object_versions
                                .entry(record.logical_id())
                                .and_modify(|current| *current = (*current).max(record.version()))
                                .or_insert(record.version());
                        } else if matches!(record, BackendScenarioRecord::TRootPub { .. }) {
                            root_versions
                                .entry(record.logical_id())
                                .and_modify(|current| {
                                    if current.0 < record.version() {
                                        *current = (record.version(), record.root_object_ids());
                                    }
                                })
                                .or_insert((record.version(), record.root_object_ids()));
                        }
                    }
                    continue;
                }

                for record in &tx.records {
                    if record.role() != TxLogEntryRole::TMemoryUndo {
                        continue;
                    }
                    tmemory_undo_rollbacks.insert((
                        record.logical_id(),
                        record.version(),
                        repeated_payload(record.payload_byte()),
                    ));
                    tmemory_undo_rollback_count += 1;
                }
            }

            BackendRecoverySummary {
                object_winners: object_versions.into_iter().collect(),
                root_object_ids: root_versions
                    .into_values()
                    .flat_map(|(_, object_ids)| object_ids.iter().filter_map(|id| *id))
                    .collect(),
                tmemory_undo_rollbacks,
                tmemory_undo_rollback_count,
            }
        }

        fn summarize_recovery(
            recovered: &crate::runtime::vm::block_region::TransactionPersistenceRecoveredRegion,
        ) -> BackendRecoverySummary {
            BackendRecoverySummary {
                object_winners: recovered
                    .object_winners
                    .iter()
                    .map(|winner| (object_logical_id(winner.object_id), winner.version))
                    .collect(),
                root_object_ids: recovered.root_object_ids.iter().copied().collect(),
                tmemory_undo_rollbacks: recovered
                    .tmemory_undo_rollbacks
                    .iter()
                    .map(|rollback| {
                        (
                            rollback.logical_id,
                            rollback.version,
                            rollback.old_granule_bytes.clone(),
                        )
                    })
                    .collect(),
                tmemory_undo_rollback_count: recovered.tmemory_undo_rollbacks.len(),
            }
        }

        fn execute_scenario(
            factory: DurableLogTestFactory,
            scenario: &BackendScenario,
        ) -> Result<BackendObservation> {
            let mut handle = factory.create()?;
            if scenario.transactions.iter().any(|tx| {
                tx.records
                    .iter()
                    .any(|record| (*record).is_object_publication())
            }) {
                handle.log.ensure_type_layout(&test_struct_type_layout())?;
            }

            for tx in &scenario.transactions {
                assert!(
                    !tx.records.is_empty(),
                    "scenario transactions must publish at least one record"
                );

                let mut final_marker = None;
                {
                    let mut sink = handle.log.stream_sink(tx.stream_id);
                    let mut publisher =
                        StreamPublisher::new_for_test(&mut sink, tx.stream_id, tx.txid);
                    for record in &tx.records {
                        let marker = match record {
                            BackendScenarioRecord::TObjectPub { .. }
                            | BackendScenarioRecord::TRootPub { .. } => publisher
                                .publish_object_publication_before_commit(&pending_publication(
                                    *record,
                                ))?,
                            BackendScenarioRecord::TMemoryUndo { .. } => publisher
                                .publish_tmemory_undo_before_in_place_write(&pending_undo(
                                    *record,
                                ))?,
                        };
                        final_marker = Some(marker);
                    }
                }

                if tx.commit {
                    let mut sink = handle.log.stream_sink(tx.stream_id);
                    let mut publisher =
                        StreamPublisher::new_for_test(&mut sink, tx.stream_id, tx.txid);
                    publisher.publish_commit_lp(final_marker.unwrap())?;
                }
            }

            handle.observe(scenario)
        }

        fn assert_log_properties(
            factory: DurableLogTestFactory,
            scenario: &BackendScenario,
            observation: &BackendObservation,
        ) {
            let expected_streams = expected_stream_entries(scenario);
            assert_eq!(
                observation.streams.len(),
                expected_streams.len(),
                "backend {} scenario {} stream count",
                factory.name(),
                scenario.name,
            );

            for (stream_id, expected_entries) in expected_streams {
                let actual_entries = observation
                    .streams
                    .get(&stream_id)
                    .unwrap_or_else(|| panic!("missing stream {stream_id}"));
                assert_eq!(
                    actual_entries.len(),
                    expected_entries.len(),
                    "backend {} scenario {} stream {} entry count",
                    factory.name(),
                    scenario.name,
                    stream_id,
                );
                assert!(
                    actual_entries.iter().all(TxLogEntry::validate_crc32),
                    "backend {} scenario {} stream {} must seal crc32",
                    factory.name(),
                    scenario.name,
                    stream_id,
                );

                for (entry_index, (actual, expected)) in actual_entries
                    .iter()
                    .zip(expected_entries.iter())
                    .enumerate()
                {
                    assert_eq!(
                        actual.logical_id,
                        expected.logical_id,
                        "backend {} scenario {} stream {} entry {} logical id",
                        factory.name(),
                        scenario.name,
                        stream_id,
                        entry_index,
                    );
                    assert_eq!(
                        actual.version,
                        expected.version,
                        "backend {} scenario {} stream {} entry {} version",
                        factory.name(),
                        scenario.name,
                        stream_id,
                        entry_index,
                    );
                    assert_eq!(
                        actual.role().unwrap(),
                        expected.role,
                        "backend {} scenario {} stream {} entry {} role",
                        factory.name(),
                        scenario.name,
                        stream_id,
                        entry_index,
                    );
                    assert_eq!(
                        actual.tx_meta >> 1,
                        expected.txid,
                        "backend {} scenario {} stream {} entry {} txid",
                        factory.name(),
                        scenario.name,
                        stream_id,
                        entry_index,
                    );
                    assert_eq!(
                        (actual.tx_meta & 1) != 0,
                        expected.is_final,
                        "backend {} scenario {} stream {} entry {} lp bit",
                        factory.name(),
                        scenario.name,
                        stream_id,
                        entry_index,
                    );
                }

                for window in actual_entries.windows(2) {
                    let current = &window[0];
                    let next = &window[1];
                    assert_ne!(
                        (next.data_block, next.data_offset),
                        (current.data_block, current.data_offset),
                        "backend {} scenario {} stream {} adjacent records must advance to a new data pointer",
                        factory.name(),
                        scenario.name,
                        stream_id,
                    );
                }
            }

            if factory.supports_recovery() {
                assert_file_backed_role_data_stream_ids(scenario, observation);
            }
        }

        fn assert_file_backed_role_data_stream_ids(
            scenario: &BackendScenario,
            observation: &BackendObservation,
        ) {
            let Some(data_stream_ids) = observation.file_backed_data_stream_ids.as_ref() else {
                panic!("file-backed observation must capture data stream ids");
            };

            for tx in &scenario.transactions {
                let actual_entries = observation
                    .streams
                    .get(&tx.stream_id)
                    .unwrap_or_else(|| panic!("missing stream {}", tx.stream_id));
                let expected_object_stream_id = DurableDataStream::ObjectPublication
                    .file_backed_stream_id(tx.stream_id)
                    .unwrap();
                let expected_tmemory_stream_id = DurableDataStream::TMemoryUndo
                    .file_backed_stream_id(tx.stream_id)
                    .unwrap();

                for (entry_index, entry) in actual_entries.iter().enumerate() {
                    if (entry.tx_meta & 1) != 0
                        && entry.role().unwrap() == TxLogEntryRole::TMemoryUndo
                    {
                        continue;
                    }

                    let data_stream_id = *data_stream_ids
                        .get(&(tx.stream_id, entry_index))
                        .unwrap_or_else(|| {
                            panic!(
                                "missing file-backed data stream id for stream {} entry {}",
                                tx.stream_id, entry_index
                            )
                        });
                    match entry.role().unwrap() {
                        TxLogEntryRole::TObjectPub => {
                            assert_eq!(
                                data_stream_id, expected_object_stream_id,
                                "backend file_backed scenario {} stream {} entry {} object records should use the object publication data stream",
                                scenario.name, tx.stream_id, entry_index,
                            );
                        }
                        TxLogEntryRole::TMemoryUndo => {
                            assert_eq!(
                                data_stream_id, expected_tmemory_stream_id,
                                "backend file_backed scenario {} stream {} entry {} tmemory undo records should use the tmemory undo data stream",
                                scenario.name, tx.stream_id, entry_index,
                            );
                        }
                    }
                }
            }
        }

        fn assert_scenario_across_backends(scenario: BackendScenario) {
            let expected_recovery = expected_recovery(&scenario);

            for factory in DurableLogTestFactory::all() {
                let observation = execute_scenario(factory, &scenario).unwrap();
                assert_log_properties(factory, &scenario, &observation);

                match observation.recovery {
                    Some(actual) => assert_eq!(
                        actual,
                        expected_recovery,
                        "backend {} scenario {} recovery",
                        factory.name(),
                        scenario.name,
                    ),
                    None => assert!(
                        !factory.supports_recovery(),
                        "backend {} unexpectedly lacked recovery support",
                        factory.name(),
                    ),
                }
            }
        }

        #[test]
        fn model_backend_conformance_object_only_commit() {
            assert_scenario_across_backends(object_only_commit_scenario());
        }

        #[test]
        fn model_backend_conformance_tmemory_undo_only_commit() {
            assert_scenario_across_backends(tmemory_undo_only_commit_scenario());
        }

        #[test]
        fn model_backend_conformance_mixed_commit() {
            assert_scenario_across_backends(mixed_commit_scenario());
        }

        #[test]
        fn model_backend_conformance_mixed_loose_end() {
            assert_scenario_across_backends(mixed_loose_end_scenario());
        }

        #[test]
        fn model_backend_conformance_mixed_commit_with_root_publication() {
            assert_scenario_across_backends(mixed_commit_with_root_publication_scenario());
        }

        #[test]
        fn model_backend_conformance_mixed_loose_end_with_root_publication() {
            assert_scenario_across_backends(mixed_loose_end_with_root_publication_scenario());
        }

        #[test]
        fn model_backend_conformance_multi_stream_transaction_ids() {
            assert_scenario_across_backends(multi_stream_transaction_ids_scenario());
        }
    }
}
