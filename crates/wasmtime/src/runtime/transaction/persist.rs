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
    storage: TxDurableLogStorage,
}

#[derive(Debug)]
enum TxDurableLogStorage {
    InMemory(InMemoryTxDurableLog),
    FileBacked(FileBackedTxDurableLog),
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
        Self {
            storage: TxDurableLogStorage::InMemory(InMemoryTxDurableLog::default()),
        }
    }
}

impl TxDurableLog {
    pub(crate) fn stream_sink(&mut self, stream_id: u32) -> TxDurableLogSink<'_> {
        TxDurableLogSink {
            log: self,
            stream_id,
        }
    }

    fn stream_state_mut(&mut self, stream_id: u32) -> &mut TxDurableStreamState {
        match &mut self.storage {
            TxDurableLogStorage::InMemory(log) => log.streams.entry(stream_id).or_default(),
            TxDurableLogStorage::FileBacked(_) => {
                unreachable!("file-backed transaction log does not use in-memory stream state")
            }
        }
    }

    fn file_backed_stream_cursor(
        log: &mut FileBackedTxDurableLog,
        stream_id: u32,
    ) -> Result<crate::runtime::vm::block_region::StreamCursor> {
        if let Some(stream) = log.streams.get(&stream_id).copied() {
            return Ok(stream);
        }
        let stream = log.region.alloc_stream(stream_id)?;
        log.streams.insert(stream_id, stream);
        Ok(stream)
    }

    #[cfg(test)]
    pub(crate) fn log_entries_for_test(&self, stream_id: u32) -> Vec<TxLogEntry> {
        match &self.storage {
            TxDurableLogStorage::InMemory(log) => log
                .streams
                .get(&stream_id)
                .map(|stream| stream.log_entries.clone())
                .unwrap_or_default(),
            TxDurableLogStorage::FileBacked(log) => {
                let mut entries = Vec::new();
                let view = log.region.view();
                for block in 1..view.num_blocks() {
                    let Ok(block) = u32::try_from(block) else {
                        break;
                    };
                    let Ok(header) = view.log_block_header(block) else {
                        continue;
                    };
                    if header.stream_id == stream_id {
                        if let Ok(block_entries) = view.log_block_entries(block) {
                            entries.extend(block_entries);
                        }
                    }
                }
                entries
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn data_chunk_stream_id_for_data_block_for_test(
        &self,
        data_block: u32,
    ) -> Result<u32> {
        match &self.storage {
            TxDurableLogStorage::InMemory(_) => {
                bail!("in-memory transaction log has no file-backed data chunk headers")
            }
            TxDurableLogStorage::FileBacked(log) => {
                let view = log.region.view();
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
        Self {
            storage: TxDurableLogStorage::FileBacked(FileBackedTxDurableLog {
                region,
                streams: BTreeMap::new(),
                pending_data_chunks: BTreeSet::new(),
                pending_log_blocks: BTreeSet::new(),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn recover_file_backed_for_test(
        path: &Path,
    ) -> Result<crate::runtime::vm::block_region::TransactionPersistenceRecoveredRegion> {
        crate::runtime::vm::block_region::reopen_and_recover_file_backed_region(path)
    }
}

impl DurableSink for TxDurableLogSink<'_> {
    fn append_data_record(
        &mut self,
        record: &[u8],
        data_stream: DurableDataStream,
    ) -> Result<(u32, u32)> {
        match &mut self.log.storage {
            TxDurableLogStorage::InMemory(log) => {
                let state = log.streams.entry(self.stream_id).or_default();
                state.data_records.push(record.to_vec());
                let data_block = state.next_data_block;
                state.next_data_block = state
                    .next_data_block
                    .checked_add(1)
                    .context("transaction durable data block overflow")?;
                Ok((data_block, 0))
            }
            TxDurableLogStorage::FileBacked(log) => {
                let data_stream_id = data_stream.file_backed_stream_id(self.stream_id)?;
                let stream = TxDurableLog::file_backed_stream_cursor(log, data_stream_id)?;
                let location = log.region.append_data_record(stream, record)?;
                log.pending_data_chunks.insert(location.chunk_start_block);
                Ok((location.data_block, location.data_offset))
            }
        }
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
        match &mut self.log.storage {
            TxDurableLogStorage::InMemory(log) => {
                let state = log.streams.entry(self.stream_id).or_default();
                state.log_entries.push(entry);
                Ok(())
            }
            TxDurableLogStorage::FileBacked(log) => {
                let stream = TxDurableLog::file_backed_stream_cursor(log, self.stream_id)?;
                let log_block = log.region.append_log_entry(stream, entry)?;
                log.pending_log_blocks.insert(log_block);
                Ok(())
            }
        }
    }

    fn flush_data(&mut self) -> Result<()> {
        match &mut self.log.storage {
            TxDurableLogStorage::InMemory(_) => Ok(()),
            TxDurableLogStorage::FileBacked(log) => {
                for chunk in core::mem::take(&mut log.pending_data_chunks) {
                    log.region.flush_data_chunk(chunk)?;
                }
                Ok(())
            }
        }
    }

    fn flush_log(&mut self) -> Result<()> {
        match &mut self.log.storage {
            TxDurableLogStorage::InMemory(_) => Ok(()),
            TxDurableLogStorage::FileBacked(log) => {
                for block in core::mem::take(&mut log.pending_log_blocks) {
                    log.region.flush_log_block(block)?;
                }
                Ok(())
            }
        }
    }

    fn fence(&mut self) -> Result<()> {
        match &mut self.log.storage {
            TxDurableLogStorage::InMemory(_) => Ok(()),
            TxDurableLogStorage::FileBacked(log) => log.region.fence(),
        }
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
}
