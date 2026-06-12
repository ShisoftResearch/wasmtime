use crate::prelude::*;
#[cfg(test)]
use crate::runtime::vm::unpack_object_granule_id;
use crate::runtime::vm::{
    PackedGranuleDomain, TMemory, TxLogEntry, TxLogEntryRole, pack_object_granule_id,
};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use std::path::Path;

#[derive(Debug, Clone)]
pub(crate) struct PendingPublication {
    pub(crate) logical_id: u64,
    pub(crate) version: u32,
    pub(crate) kind: u16,
    pub(crate) type_info: u32,
    pub(crate) payload: Vec<u8>,
}

pub(crate) fn encode_data_record(pub_: &PendingPublication) -> Result<Vec<u8>> {
    TMemory::encode_publication_data_record(
        pub_.logical_id,
        pub_.version,
        pub_.kind,
        pub_.type_info,
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
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
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
    pub(crate) fn persistent_object(
        domain: PackedGranuleDomain,
        object_id: u64,
        version: u32,
        type_info: u32,
        payload: Vec<u8>,
    ) -> Result<Self> {
        Ok(Self {
            logical_id: pack_object_granule_id(domain, object_id)?,
            version,
            kind: domain as u16,
            type_info,
            payload,
        })
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
            type_info: 0,
            payload: payload.to_vec(),
        }
    }

    fn persistent_object_for_test(
        domain: PackedGranuleDomain,
        object_id: u64,
        version: u32,
        type_info: u32,
        payload: &[u8],
    ) -> Self {
        Self::persistent_object(domain, object_id, version, type_info, payload.to_vec()).unwrap()
    }
}

pub(crate) trait DurableSink {
    fn append_data_record(
        &mut self,
        record: &[u8],
        data_stream: DurableDataStream,
    ) -> Result<(u32, u32)>;
    fn append_log_entry(
        &mut self,
        logical_id: u64,
        version: u32,
        tx_meta: u32,
        data_block: u32,
        data_offset: u32,
        is_final: bool,
        role: TxLogEntryRole,
    ) -> Result<()>;
    fn flush_data(&mut self) -> Result<()>;
    fn flush_log(&mut self) -> Result<()>;
    fn fence(&mut self) -> Result<()>;
}

#[derive(Debug)]
pub(crate) struct TxDurableLog {
    storage: Box<dyn TxDurableLogBackend>,
}

/// Storage backend for the transaction-owned durable log.
///
/// `TxDurableLog` owns transaction ordering and commit protocol decisions; a
/// backend only provides append, flush, and fence primitives for log entries
/// and role-specific data records. File-backed storage is one implementation
/// of this trait, not a separate commit path.
pub(crate) trait TxDurableLogBackend: core::fmt::Debug + Send + Sync {
    fn append_data_record(
        &mut self,
        transaction_stream_id: u32,
        data_stream: DurableDataStream,
        record: &[u8],
    ) -> Result<(u32, u32)>;
    fn append_log_entry(&mut self, transaction_stream_id: u32, entry: TxLogEntry) -> Result<()>;
    fn flush_data(&mut self) -> Result<()>;
    fn flush_log(&mut self) -> Result<()>;
    fn fence(&mut self) -> Result<()>;

    #[cfg(test)]
    fn log_entries_for_test(&self, stream_id: u32) -> Vec<TxLogEntry>;

    #[cfg(test)]
    fn data_chunk_stream_id_for_data_block_for_test(&self, data_block: u32) -> Result<u32>;
}

#[derive(Debug, Default)]
struct InMemoryTxDurableLog {
    streams: BTreeMap<u32, TxDurableStreamState>,
}

#[derive(Debug, Default)]
struct TxDurableStreamState {
    data_records: Vec<Vec<u8>>,
    log_entries: Vec<TxLogEntry>,
    next_data_block: u32,
}

#[derive(Debug)]
struct FileBackedTxDurableLog {
    region: crate::runtime::vm::block_region::FileBackedMemoryBlockRegion,
    streams: BTreeMap<u32, crate::runtime::vm::block_region::StreamCursor>,
    pending_data_chunks: BTreeSet<u32>,
    pending_log_blocks: BTreeSet<u32>,
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

    #[cfg(test)]
    pub(crate) fn log_entries_for_test(&self, stream_id: u32) -> Vec<TxLogEntry> {
        self.storage.log_entries_for_test(stream_id)
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
        let region =
            crate::runtime::vm::block_region::FileBackedMemoryBlockRegion::create_for_test(
                path, num_blocks,
            )?;
        Ok(Self::from_file_backed_region(region))
    }

    pub(crate) fn open_file_backed(path: &Path) -> Result<Self> {
        let region =
            crate::runtime::vm::block_region::FileBackedMemoryBlockRegion::open_for_test(path)?;
        Ok(Self::from_file_backed_region(region))
    }

    fn from_file_backed_region(
        region: crate::runtime::vm::block_region::FileBackedMemoryBlockRegion,
    ) -> Self {
        Self::with_backend(FileBackedTxDurableLog {
            region,
            streams: BTreeMap::new(),
            pending_data_chunks: BTreeSet::new(),
            pending_log_blocks: BTreeSet::new(),
        })
    }

    #[cfg(test)]
    pub(crate) fn recover_file_backed_for_test(
        path: &Path,
    ) -> Result<crate::runtime::vm::block_region::TransactionPersistenceRecoveredRegion> {
        crate::runtime::vm::block_region::reopen_and_recover_file_backed_region(path)
    }
}

impl TxDurableLogBackend for InMemoryTxDurableLog {
    fn append_data_record(
        &mut self,
        transaction_stream_id: u32,
        _data_stream: DurableDataStream,
        record: &[u8],
    ) -> Result<(u32, u32)> {
        let state = self.streams.entry(transaction_stream_id).or_default();
        state.data_records.push(record.to_vec());
        let data_block = state.next_data_block;
        state.next_data_block = state
            .next_data_block
            .checked_add(1)
            .context("transaction durable data block overflow")?;
        Ok((data_block, 0))
    }

    fn append_log_entry(&mut self, transaction_stream_id: u32, entry: TxLogEntry) -> Result<()> {
        let state = self.streams.entry(transaction_stream_id).or_default();
        state.log_entries.push(entry);
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

impl FileBackedTxDurableLog {
    fn stream_cursor(
        &mut self,
        stream_id: u32,
    ) -> Result<crate::runtime::vm::block_region::StreamCursor> {
        if let Some(stream) = self.streams.get(&stream_id).copied() {
            return Ok(stream);
        }
        let stream = self.region.alloc_stream(stream_id)?;
        self.streams.insert(stream_id, stream);
        Ok(stream)
    }
}

impl TxDurableLogBackend for FileBackedTxDurableLog {
    fn append_data_record(
        &mut self,
        transaction_stream_id: u32,
        data_stream: DurableDataStream,
        record: &[u8],
    ) -> Result<(u32, u32)> {
        let data_stream_id = data_stream.file_backed_stream_id(transaction_stream_id)?;
        let stream = self.stream_cursor(data_stream_id)?;
        let location = self.region.append_data_record(stream, record)?;
        self.pending_data_chunks.insert(location.chunk_start_block);
        Ok((location.data_block, location.data_offset))
    }

    fn append_log_entry(&mut self, transaction_stream_id: u32, entry: TxLogEntry) -> Result<()> {
        let stream = self.stream_cursor(transaction_stream_id)?;
        let log_block = self.region.append_log_entry(stream, entry)?;
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
            let Ok(header) = view.data_chunk_header(chunk_start) else {
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
    ) -> Result<(u32, u32)> {
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
        is_final: bool,
        role: TxLogEntryRole,
    ) -> Result<()> {
        let mut entry = TMemory::publication_log_entry(
            logical_id,
            version,
            tx_meta,
            data_block,
            data_offset,
            is_final,
        );
        entry.set_role(role);
        entry.seal_crc32();
        self.log.storage.append_log_entry(self.stream_id, entry)
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

    pub(crate) fn publish_transaction(&mut self, pubs: &[PendingPublication]) -> Result<()> {
        let _ = self.stream_id;

        let mut ordinary = Vec::new();
        for pub_ in pubs {
            let record = encode_data_record(pub_)?;
            let (data_block, data_offset) = self
                .sink
                .append_data_record(&record, DurableDataStream::ObjectPublication)?;
            ordinary.push((
                pub_.logical_id,
                pub_.version,
                self.txid << 1,
                data_block,
                data_offset,
            ));
        }

        self.sink.flush_data()?;
        self.sink.fence()?;

        for &(logical_id, version, tx_meta, data_block, data_offset) in
            ordinary.iter().take(ordinary.len().saturating_sub(1))
        {
            self.sink.append_log_entry(
                logical_id,
                version,
                tx_meta,
                data_block,
                data_offset,
                false,
                TxLogEntryRole::TObjectPub,
            )?;
        }
        self.sink.flush_log()?;

        if let Some(&(logical_id, version, tx_meta, data_block, data_offset)) = ordinary.last() {
            self.sink.append_log_entry(
                logical_id,
                version,
                tx_meta,
                data_block,
                data_offset,
                true,
                TxLogEntryRole::TObjectPub,
            )?;
            self.sink.flush_log()?;
            self.sink.fence()?;
        }

        Ok(())
    }

    pub(crate) fn publish_tmemory_undo_before_in_place_write(
        &mut self,
        undo: &PendingGranuleUndo,
    ) -> Result<PendingCommitLogEntry> {
        let _ = self.stream_id;

        let record = encode_undo_data_record(undo)?;
        let (data_block, data_offset) = self
            .sink
            .append_data_record(&record, DurableDataStream::TMemoryUndo)?;

        self.sink.flush_data()?;
        self.sink.fence()?;

        self.sink.append_log_entry(
            undo.logical_id,
            undo.version,
            self.txid << 1,
            data_block,
            data_offset,
            false,
            TxLogEntryRole::TMemoryUndo,
        )?;
        self.sink.flush_log()?;
        self.sink.fence()?;

        Ok(PendingCommitLogEntry {
            logical_id: undo.logical_id,
            version: undo.version,
            data_block,
            data_offset,
            role: TxLogEntryRole::TMemoryUndo,
        })
    }

    pub(crate) fn publish_object_publication_before_commit(
        &mut self,
        pub_: &PendingPublication,
    ) -> Result<PendingCommitLogEntry> {
        let _ = self.stream_id;

        let record = encode_data_record(pub_)?;
        let (data_block, data_offset) = self
            .sink
            .append_data_record(&record, DurableDataStream::ObjectPublication)?;

        self.sink.flush_data()?;
        self.sink.fence()?;

        self.sink.append_log_entry(
            pub_.logical_id,
            pub_.version,
            self.txid << 1,
            data_block,
            data_offset,
            false,
            TxLogEntryRole::TObjectPub,
        )?;
        self.sink.flush_log()?;
        self.sink.fence()?;

        Ok(PendingCommitLogEntry {
            logical_id: pub_.logical_id,
            version: pub_.version,
            data_block,
            data_offset,
            role: TxLogEntryRole::TObjectPub,
        })
    }

    pub(crate) fn publish_commit_lp(&mut self, marker: PendingCommitLogEntry) -> Result<()> {
        self.sink.append_log_entry(
            marker.logical_id,
            marker.version,
            self.txid << 1,
            marker.data_block,
            marker.data_offset,
            true,
            marker.role,
        )?;
        self.sink.flush_log()?;
        self.sink.fence()?;

        Ok(())
    }

    pub(crate) fn abort_transaction(&mut self, _pubs: &[PendingPublication]) -> Result<()> {
        let _ = self.stream_id;
        Ok(())
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
impl DurableSink for RecordingDurability {
    fn append_data_record(
        &mut self,
        _record: &[u8],
        _data_stream: DurableDataStream,
    ) -> Result<(u32, u32)> {
        if self.events.last() != Some(&DurabilityEvent::DataWrite) {
            self.events.push(DurabilityEvent::DataWrite);
        }
        let data_block = self.next_data_block;
        self.next_data_block = self
            .next_data_block
            .checked_add(1)
            .context("recording durability data block overflow")?;
        Ok((data_block, 0))
    }

    fn append_log_entry(
        &mut self,
        _logical_id: u64,
        _version: u32,
        tx_meta: u32,
        _data_block: u32,
        _data_offset: u32,
        is_final: bool,
        role: TxLogEntryRole,
    ) -> Result<()> {
        self.events.push(DurabilityEvent::LogWrite);
        let stored_tx_meta = if is_final { tx_meta | 1 } else { tx_meta & !1 };
        self.tx_meta.push(stored_tx_meta);
        self.roles.push(role);
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
            type_info: 0x10,
            payload: vec![1, 2, 3, 4],
        },
        PendingPublication {
            logical_id: 0x1001,
            version: 2,
            kind: 9,
            type_info: 0x20,
            payload: vec![5, 6, 7, 8, 9],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::transaction::{
        ObjectPayload, ObjectValue, object_heap::TxObjectHeader,
        object_heap::encode_object_record_for_test,
    };

    #[test]
    fn commit_publishes_only_the_last_entry_with_lp() {
        let mut recorder = RecordingDurability::default();
        let mut publisher = StreamPublisher::new_for_test(&mut recorder, 4, 21);
        publisher
            .publish_transaction(&sample_publications())
            .unwrap();

        assert_eq!(recorder.final_lp_count(), 1);
        assert!(recorder.non_final_log_entries_before_final_lp());
    }

    #[test]
    fn commit_orders_data_before_final_lp() {
        let mut recorder = RecordingDurability::default();
        let mut publisher = StreamPublisher::new_for_test(&mut recorder, 4, 99);
        publisher
            .publish_transaction(&sample_publications())
            .unwrap();

        assert!(recorder.events().starts_with(&[
            DurabilityEvent::DataWrite,
            DurabilityEvent::DataFlush,
            DurabilityEvent::Fence,
        ]));
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
                DurabilityEvent::LogWrite,
                DurabilityEvent::LogFlush,
                DurabilityEvent::Fence,
            ]
        );
        assert_eq!(recorder.final_lp_count(), 1);
        assert_eq!(
            recorder.roles,
            vec![TxLogEntryRole::TMemoryUndo, TxLogEntryRole::TMemoryUndo]
        );
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
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].logical_id, first_marker.logical_id);
        assert_eq!(entries[1].logical_id, second_marker.logical_id);
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.tx_meta & 1 != 0)
                .count(),
            1
        );
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
        let undo = PendingGranuleUndo::tmemory(0x1000_0000_0000_002a, 13, vec![1, 2, 3, 4]);
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
            log.log_entries_for_test(12)
                .iter()
                .filter(|entry| entry.tx_meta & 1 != 0)
                .count(),
            1
        );
        drop(log);

        let recovered = TxDurableLog::recover_file_backed_for_test(&path).unwrap();
        assert_eq!(recovered.object_winners.len(), 1);
        assert_eq!(recovered.object_winners[0].object_id, 41);
        assert!(recovered.tmemory_undo_rollbacks.is_empty());
    }

    #[test]
    fn file_backed_transaction_log_separates_object_and_tmemory_data_streams() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx-log.bin");
        let mut log = TxDurableLog::create_file_backed(&path, 32).unwrap();
        let undo = PendingGranuleUndo::tmemory(0x1000_0000_0000_002a, 13, vec![1, 2, 3, 4]);
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
        assert_eq!(header.type_index, 12);
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
        assert_eq!(pub_.type_info, 12);
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
                12,
                &ObjectPayload::Struct(vec![ObjectValue::I32(i32::from(record.payload_byte))]),
            )
            .unwrap();
            PendingPublication::persistent_object_for_test(
                PackedGranuleDomain::TStruct,
                object_id,
                record.version,
                12,
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
                    Self::TMemoryUndo { logical_id, .. } => logical_id,
                }
            }

            fn version(self) -> u32 {
                match self {
                    Self::TObjectPub { version, .. } | Self::TMemoryUndo { version, .. } => version,
                }
            }

            fn role(self) -> TxLogEntryRole {
                match self {
                    Self::TObjectPub { .. } => TxLogEntryRole::TObjectPub,
                    Self::TMemoryUndo { .. } => TxLogEntryRole::TMemoryUndo,
                }
            }

            fn payload_byte(self) -> u8 {
                match self {
                    Self::TObjectPub { payload_byte, .. }
                    | Self::TMemoryUndo { payload_byte, .. } => payload_byte,
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

        fn object_logical_id(object_id: u64) -> u64 {
            pack_object_granule_id(PackedGranuleDomain::TStruct, object_id).unwrap()
        }

        fn tmemory_logical_id(granule_id: u64) -> u64 {
            0x1000_0000_0000_0000 | (granule_id << 12) | 0x2a
        }

        fn repeated_payload(byte: u8) -> Vec<u8> {
            vec![byte; 4]
        }

        fn pending_publication(record: BackendScenarioRecord) -> PendingPublication {
            let BackendScenarioRecord::TObjectPub {
                object_id,
                version,
                payload_byte,
            } = record
            else {
                unreachable!();
            };
            let payload = encode_object_record_for_test(
                object_id,
                version,
                12,
                &ObjectPayload::Struct(vec![ObjectValue::I32(i32::from(payload_byte))]),
            )
            .unwrap();
            PendingPublication::persistent_object_for_test(
                PackedGranuleDomain::TStruct,
                object_id,
                version,
                12,
                &payload,
            )
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
                    let last = tx
                        .records
                        .last()
                        .copied()
                        .expect("scenario tx must have records");
                    entries.push(ExpectedLogEntry {
                        logical_id: last.logical_id(),
                        version: last.version(),
                        txid: tx.txid,
                        role: last.role(),
                        is_final: true,
                    });
                }
                streams.insert(tx.stream_id, entries);
            }

            streams
        }

        fn expected_recovery(scenario: &BackendScenario) -> BackendRecoverySummary {
            let mut object_versions = BTreeMap::<u64, u32>::new();
            let mut tmemory_undo_rollbacks = BTreeSet::new();
            let mut tmemory_undo_rollback_count = 0usize;

            for tx in &scenario.transactions {
                if tx.commit {
                    for record in &tx.records {
                        if record.role() != TxLogEntryRole::TObjectPub {
                            continue;
                        }
                        object_versions
                            .entry(record.logical_id())
                            .and_modify(|current| *current = (*current).max(record.version()))
                            .or_insert(record.version());
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
                            BackendScenarioRecord::TObjectPub { .. } => publisher
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
                    let next_is_final = (next.tx_meta & 1) != 0;
                    if next_is_final {
                        assert_eq!(
                            (next.data_block, next.data_offset),
                            (current.data_block, current.data_offset),
                            "backend {} scenario {} stream {} final lp must reuse previous data pointer",
                            factory.name(),
                            scenario.name,
                            stream_id,
                        );
                    } else {
                        assert_ne!(
                            (next.data_block, next.data_offset),
                            (current.data_block, current.data_offset),
                            "backend {} scenario {} stream {} new records must advance to a new data pointer",
                            factory.name(),
                            scenario.name,
                            stream_id,
                        );
                    }
                }
            }

            if factory.supports_recovery() {
                assert_file_backed_role_data_stream_separation(scenario, observation);
            }
        }

        fn assert_file_backed_role_data_stream_separation(
            scenario: &BackendScenario,
            observation: &BackendObservation,
        ) {
            let Some(data_stream_ids) = observation.file_backed_data_stream_ids.as_ref() else {
                panic!("file-backed observation must capture data stream ids");
            };

            for tx in &scenario.transactions {
                let has_object = tx
                    .records
                    .iter()
                    .any(|record| matches!(record, BackendScenarioRecord::TObjectPub { .. }));
                let has_tmemory = tx
                    .records
                    .iter()
                    .any(|record| matches!(record, BackendScenarioRecord::TMemoryUndo { .. }));
                if !has_object || !has_tmemory {
                    continue;
                }

                let actual_entries = observation
                    .streams
                    .get(&tx.stream_id)
                    .unwrap_or_else(|| panic!("missing stream {}", tx.stream_id));
                let mut object_stream_ids = BTreeSet::new();
                let mut tmemory_stream_ids = BTreeSet::new();

                for (entry_index, entry) in actual_entries.iter().enumerate() {
                    if (entry.tx_meta & 1) != 0 {
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
                            object_stream_ids.insert(data_stream_id);
                        }
                        TxLogEntryRole::TMemoryUndo => {
                            tmemory_stream_ids.insert(data_stream_id);
                        }
                    }
                }

                assert_eq!(
                    object_stream_ids.len(),
                    1,
                    "backend file_backed scenario {} stream {} object records should use one object data stream",
                    scenario.name,
                    tx.stream_id,
                );
                assert_eq!(
                    tmemory_stream_ids.len(),
                    1,
                    "backend file_backed scenario {} stream {} tmemory records should use one tmemory data stream",
                    scenario.name,
                    tx.stream_id,
                );

                let object_stream_id = *object_stream_ids.iter().next().unwrap();
                let tmemory_stream_id = *tmemory_stream_ids.iter().next().unwrap();

                assert_ne!(
                    object_stream_id, tmemory_stream_id,
                    "backend file_backed scenario {} stream {} object and tmemory data must use distinct data streams",
                    scenario.name, tx.stream_id,
                );
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
        fn model_backend_conformance_multi_stream_transaction_ids() {
            assert_scenario_across_backends(multi_stream_transaction_ids_scenario());
        }
    }
}
