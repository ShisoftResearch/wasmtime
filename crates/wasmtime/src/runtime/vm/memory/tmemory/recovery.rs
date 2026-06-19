use super::block_region::BlockRegionBackendView;
use super::durable_log::BlockKind;
use super::{
    DATA_CHUNK_MAGIC, DataChunkHeader, LOG_BLOCK_MAGIC, PackedGranuleDomain, TxDataRecordHeader,
    TxDataRecordRole, TxLogEntry, TxLogEntryRole, packed_granule_domain, unpack_object_granule_id,
    unpack_tmemory_size_logical_id,
};
use crate::prelude::*;
#[cfg(test)]
use crate::runtime::transaction::PersistentObjectRefRaw;
use crate::runtime::transaction::{
    ObjectKind, TxObjectHeader,
    type_layout::{TypeLayoutId, TypeLayoutRegistry},
};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

#[cfg(test)]
use super::block_region::{BLOCK_SIZE, StreamCursor, VMemoryBlockRegion};
#[cfg(test)]
use super::{DataRecordLocation, LogBlockHeader, TMemory};
use core::mem::size_of;

#[derive(Debug)]
pub(crate) struct RecoveredStream {
    pub(crate) stream_id: u32,
    pub(crate) log_blocks: Vec<u32>,
    pub(crate) data_chunks: Vec<u32>,
}

#[derive(Debug)]
pub(crate) struct RecoveryWinner {
    pub(crate) logical_id: u64,
    pub(crate) version: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
}

#[derive(Debug)]
pub(crate) struct RecoveredRegion {
    pub(crate) streams: Vec<RecoveredStream>,
    pub(crate) winners: Vec<RecoveryWinner>,
    pub(crate) object_winners: Vec<RecoveredObjectWinner>,
    pub(crate) tmemory_size_winners: Vec<RecoveredTMemorySizeWinner>,
    pub(crate) type_layouts: TypeLayoutRegistry,
    pub(crate) root_object_ids: Vec<u64>,
    pub(crate) tmemory_undo_rollbacks: Vec<RecoveredTMemoryUndoRollback>,
    pub(crate) next_stream_id: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveredObjectWinner {
    pub(crate) object_id: u64,
    pub(crate) version: u32,
    pub(crate) kind: u16,
    pub(crate) type_layout_id: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) record_len: u64,
    pub(crate) record_bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveredTMemoryUndoRollback {
    pub(crate) logical_id: u64,
    pub(crate) version: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) old_granule_bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveredTMemorySizeWinner {
    pub(crate) owner_instance: Option<u32>,
    pub(crate) memory_index: u32,
    pub(crate) version: u32,
    pub(crate) new_pages: u64,
}

#[derive(Debug, Default)]
struct StreamReplay {
    winners: Vec<RecoveryWinner>,
    tmemory_undo_rollbacks: Vec<RecoveredTMemoryUndoRollback>,
}

#[derive(Debug, Default)]
struct DiscoveredStreamParts {
    log_blocks: Vec<(u32, u32)>,
    data_chunks: Vec<(u32, u32)>,
}

#[derive(Debug)]
struct IndexedDataChunk {
    start_block: u32,
    end_block_exclusive: u32,
    chunk_start_byte_offset: usize,
    chunk_tail_offset: usize,
    header_boundary_offset: usize,
}

#[derive(Debug)]
struct DataChunkIndex {
    chunks: Vec<IndexedDataChunk>,
    block_to_chunk: Vec<Option<usize>>,
}

#[derive(Debug)]
struct DiscoveredRegion {
    streams: Vec<RecoveredStream>,
    data_chunk_index: DataChunkIndex,
}

#[derive(Debug, Default)]
struct PendingTransaction {
    txid: u32,
    entries: Vec<TxLogEntry>,
}

pub(crate) fn recover_region(
    region: &BlockRegionBackendView<'_>,
    type_layouts: TypeLayoutRegistry,
) -> Result<RecoveredRegion> {
    let discovered = discover_region(region)?;
    let mut winners = BTreeMap::<u64, RecoveryWinner>::new();
    let mut tmemory_undo_rollbacks = Vec::new();

    for stream in discovered.streams.iter() {
        let replay = replay_stream(region, stream, &discovered.data_chunk_index)?;
        tmemory_undo_rollbacks.extend(replay.tmemory_undo_rollbacks);
        for update in replay.winners {
            match winners.get(&update.logical_id) {
                Some(current) if current.version > update.version => {}
                Some(current)
                    if current.version == update.version
                        && current.data_block == update.data_block
                        && current.data_offset == update.data_offset => {}
                Some(current) if current.version == update.version => {
                    if let Ok((_, object_id)) = unpack_object_granule_id(update.logical_id) {
                        bail!(
                            "duplicate committed object version {} for object id {}",
                            update.version,
                            object_id
                        );
                    }
                    bail!(
                        "duplicate committed version {} for logical id {}",
                        update.version,
                        update.logical_id
                    );
                }
                _ => {
                    winners.insert(update.logical_id, update);
                }
            }
        }
    }

    let winners = winners.into_values().collect::<Vec<_>>();
    let object_winners = replay_object_winners(region, &discovered.data_chunk_index, &winners)?;
    let tmemory_size_winners =
        replay_tmemory_size_winners(region, &discovered.data_chunk_index, &winners)?;
    let root_object_ids = replay_root_object_ids(region, &discovered.data_chunk_index, &winners)?;

    Ok(RecoveredRegion {
        next_stream_id: discovered
            .streams
            .iter()
            .map(|stream| stream.stream_id)
            .max()
            .unwrap_or(0)
            + 1,
        streams: discovered.streams,
        winners,
        object_winners,
        tmemory_size_winners,
        type_layouts,
        root_object_ids,
        tmemory_undo_rollbacks,
    })
}

impl RecoveredRegion {
    pub(crate) fn committed_object_winners(&self) -> Result<Vec<RecoveredObjectWinner>> {
        Ok(self.object_winners.clone())
    }

    pub(crate) fn committed_file_backed_tmemory_pages(&self) -> Result<Option<u64>> {
        let mut pages = None;
        let mut key = None;
        for winner in &self.tmemory_size_winners {
            let winner_key = (winner.owner_instance, winner.memory_index);
            if let Some(current_key) = key {
                ensure!(
                    current_key == winner_key,
                    "file-backed tmemory recovery found multiple tmemory size winners"
                );
            } else {
                key = Some(winner_key);
            }
            pages = Some(winner.new_pages);
        }
        Ok(pages)
    }
}

fn replay_tmemory_size_winners(
    region: &BlockRegionBackendView<'_>,
    data_chunk_index: &DataChunkIndex,
    winners: &[RecoveryWinner],
) -> Result<Vec<RecoveredTMemorySizeWinner>> {
    let mut tmemory_sizes = Vec::new();

    for winner in winners {
        let Ok(domain) = packed_granule_domain(winner.logical_id) else {
            continue;
        };
        if domain != PackedGranuleDomain::TMemorySize {
            continue;
        }

        let (data_header, payload) = load_publication_payload(
            region,
            data_chunk_index,
            winner.data_block,
            winner.data_offset,
        )?;
        ensure!(
            data_header.logical_id == winner.logical_id,
            "recovered tmemory size logical id does not match log winner"
        );
        ensure!(
            data_header.version == winner.version,
            "recovered tmemory size version does not match log winner"
        );
        ensure!(
            data_header.role()? == TxDataRecordRole::TObjectPub,
            "recovered tmemory size data record has non-publication role"
        );
        ensure!(
            data_header.kind == PackedGranuleDomain::TMemorySize as u16,
            "recovered tmemory size kind does not match log winner"
        );
        ensure!(
            payload.len() == size_of::<u64>(),
            "recovered tmemory size payload must be exactly 8 bytes"
        );
        let (owner_instance, memory_index) = unpack_tmemory_size_logical_id(winner.logical_id)?;
        tmemory_sizes.push(RecoveredTMemorySizeWinner {
            owner_instance,
            memory_index,
            version: winner.version,
            new_pages: u64::from_le_bytes(payload.try_into().unwrap()),
        });
    }

    Ok(tmemory_sizes)
}

fn replay_root_object_ids(
    region: &BlockRegionBackendView<'_>,
    data_chunk_index: &DataChunkIndex,
    winners: &[RecoveryWinner],
) -> Result<Vec<u64>> {
    let mut roots = Vec::new();

    for winner in winners {
        let Ok(domain) = packed_granule_domain(winner.logical_id) else {
            continue;
        };
        match domain {
            PackedGranuleDomain::TGlobal => {
                let payload =
                    load_root_publication_payload(region, data_chunk_index, winner, domain)?;
                roots.extend(decode_root_object_refs(&payload)?.into_iter().take(1));
            }
            PackedGranuleDomain::TTable => {
                let payload =
                    load_root_publication_payload(region, data_chunk_index, winner, domain)?;
                roots.extend(decode_root_object_refs(&payload)?);
            }
            _ => {}
        }
    }

    roots.sort_unstable();
    roots.dedup();
    Ok(roots)
}

fn load_root_publication_payload(
    region: &BlockRegionBackendView<'_>,
    data_chunk_index: &DataChunkIndex,
    winner: &RecoveryWinner,
    expected_domain: PackedGranuleDomain,
) -> Result<Vec<u8>> {
    let (data_header, payload) = load_publication_payload(
        region,
        data_chunk_index,
        winner.data_block,
        winner.data_offset,
    )?;
    ensure!(
        data_header.logical_id == winner.logical_id,
        "recovered root publication logical id does not match log winner"
    );
    ensure!(
        data_header.version == winner.version,
        "recovered root publication version does not match log winner"
    );
    ensure!(
        data_header.role()? == TxDataRecordRole::TObjectPub,
        "recovered root publication data record has non-publication role"
    );
    ensure!(
        data_header.kind == expected_domain as u16,
        "recovered root publication kind does not match granule domain"
    );
    Ok(payload)
}

fn decode_root_object_refs(payload: &[u8]) -> Result<Vec<u64>> {
    ensure!(
        payload.len() % size_of::<u64>() == 0,
        "root object reference payload is not u64-aligned"
    );
    Ok(payload
        .chunks_exact(size_of::<u64>())
        .filter_map(|bytes| {
            let raw = u64::from_le_bytes(bytes.try_into().unwrap());
            raw.checked_sub(1)
        })
        .collect::<Vec<_>>())
}

fn replay_object_winners(
    region: &BlockRegionBackendView<'_>,
    data_chunk_index: &DataChunkIndex,
    winners: &[RecoveryWinner],
) -> Result<Vec<RecoveredObjectWinner>> {
    let mut object_winners = BTreeMap::<u64, RecoveredObjectWinner>::new();

    for winner in winners {
        let Ok((domain, object_id)) = unpack_object_granule_id(winner.logical_id) else {
            continue;
        };
        let (data_header, record_bytes) = load_publication_payload(
            region,
            data_chunk_index,
            winner.data_block,
            winner.data_offset,
        )?;
        ensure!(
            data_header.logical_id == winner.logical_id,
            "recovered object publication logical id does not match log winner"
        );
        ensure!(
            data_header.version == winner.version,
            "recovered object publication version does not match log winner"
        );
        ensure!(
            data_header.role()? == TxDataRecordRole::TObjectPub,
            "recovered object publication data record has non-publication role"
        );
        ensure!(
            data_header.kind == domain as u16,
            "recovered object publication kind does not match object domain"
        );
        let object_header = TxObjectHeader::read_from_prefix(&record_bytes)?;
        ensure!(
            object_header.object_id == object_id,
            "recovered object record id does not match logical id"
        );
        ensure!(
            object_header.version == winner.version,
            "recovered object record version does not match log winner"
        );
        ensure!(
            data_header.type_info == object_header.type_layout_id,
            "recovered object publication outer type layout id does not match object record"
        );
        ensure!(
            object_domain_matches_object_kind(domain, object_header.kind),
            "recovered object publication outer kind does not match object record kind"
        );
        TypeLayoutId::new(object_header.type_layout_id)
            .context("recovered object record type layout id cannot be zero")?;
        let candidate = RecoveredObjectWinner {
            object_id,
            version: winner.version,
            kind: object_header.kind,
            type_layout_id: object_header.type_layout_id,
            data_block: winner.data_block,
            data_offset: winner.data_offset,
            record_len: object_header.record_len,
            record_bytes,
        };
        match object_winners.get(&object_id) {
            Some(current) if current.version > candidate.version => {}
            Some(current) if current.version == candidate.version => {
                bail!(
                    "duplicate committed object version {} for object id {}",
                    candidate.version,
                    object_id
                );
            }
            _ => {
                object_winners.insert(object_id, candidate);
            }
        }
    }

    Ok(object_winners.into_values().collect())
}

fn object_domain_matches_object_kind(domain: PackedGranuleDomain, object_kind: u16) -> bool {
    match domain {
        PackedGranuleDomain::TStruct => object_kind == ObjectKind::Struct as u16,
        PackedGranuleDomain::TArray => object_kind == ObjectKind::Array as u16,
        _ => false,
    }
}

fn load_publication_payload(
    region: &BlockRegionBackendView<'_>,
    data_chunk_index: &DataChunkIndex,
    data_block: u32,
    data_offset: u32,
) -> Result<(TxDataRecordHeader, Vec<u8>)> {
    let (record_offset, chunk_tail_offset) =
        data_chunk_index.locate_data_record_offset(region, data_block, data_offset)?;
    let header = TxDataRecordHeader::from_bytes(
        region.read(record_offset, size_of::<TxDataRecordHeader>())?,
    )?;
    let payload_len =
        usize::try_from(header.payload_len).context("publication payload length overflow")?;
    let payload_offset = record_offset
        .checked_add(size_of::<TxDataRecordHeader>())
        .context("publication payload offset overflow")?;
    let payload_end = payload_offset
        .checked_add(payload_len)
        .context("publication payload end overflow")?;
    ensure!(
        payload_end <= chunk_tail_offset,
        "publication data record extends beyond data chunk tail"
    );
    let payload = region.read(payload_offset, payload_len)?;
    Ok((header, payload))
}

fn validated_chunk_blocks(
    region: &BlockRegionBackendView<'_>,
    cursor: usize,
    start_block: u32,
    header: DataChunkHeader,
) -> Result<usize> {
    let chunk_blocks = usize::try_from(header.chunk_blocks)
        .context("transactional data chunk block count overflow")?;
    ensure!(
        chunk_blocks > 0,
        "transactional data chunk at block {start_block} has zero blocks"
    );
    ensure!(
        cursor + chunk_blocks <= region.num_blocks(),
        "transactional data chunk at block {start_block} exceeds region"
    );
    Ok(chunk_blocks)
}

fn chunk_tail_offset(
    region: &BlockRegionBackendView<'_>,
    header: DataChunkHeader,
) -> Result<usize> {
    let tail_block_delta = usize::try_from(header.tail_block_delta)
        .context("transactional data chunk tail block delta conversion overflow")?;
    let tail_in_block = usize::try_from(header.tail_in_block)
        .context("transactional data chunk tail offset conversion overflow")?;
    let tail = tail_block_delta
        .checked_mul(region.block_size())
        .and_then(|offset| offset.checked_add(tail_in_block))
        .context("transactional data chunk tail overflow")?;
    let capacity = usize::try_from(header.chunk_blocks)
        .context("transactional data chunk block count conversion overflow")?
        .checked_mul(region.block_size())
        .context("transactional data chunk capacity overflow")?;
    ensure!(
        tail <= capacity,
        "transactional data chunk tail exceeds chunk capacity"
    );
    Ok(tail)
}

impl DataChunkIndex {
    fn new(num_blocks: usize) -> Self {
        Self {
            chunks: Vec::new(),
            block_to_chunk: vec![None; num_blocks],
        }
    }

    fn push_chunk(
        &mut self,
        region: &BlockRegionBackendView<'_>,
        cursor: usize,
        start_block: u32,
        chunk_blocks: usize,
        header: DataChunkHeader,
    ) -> Result<()> {
        let chunk_end = cursor
            .checked_add(chunk_blocks)
            .context("transactional data chunk range overflow")?;
        let start_block_index =
            usize::try_from(start_block).context("transactional data chunk start overflow")?;
        let end_block_exclusive = start_block
            .checked_add(
                u32::try_from(chunk_blocks)
                    .context("transactional data chunk end block conversion overflow")?,
            )
            .context("transactional data chunk end block overflow")?;
        let chunk_start_byte_offset = cursor
            .checked_mul(region.block_size())
            .context("transactional data chunk byte offset overflow")?;
        let chunk_tail_offset = chunk_tail_offset(region, header)?;
        let chunk_index = self.chunks.len();

        for block in start_block_index..chunk_end {
            self.block_to_chunk[block] = Some(chunk_index);
        }

        self.chunks.push(IndexedDataChunk {
            start_block,
            end_block_exclusive,
            chunk_start_byte_offset,
            chunk_tail_offset,
            header_boundary_offset: size_of::<DataChunkHeader>(),
        });
        Ok(())
    }

    fn locate_data_record_offset(
        &self,
        region: &BlockRegionBackendView<'_>,
        data_block: u32,
        data_offset: u32,
    ) -> Result<(usize, usize)> {
        let block = usize::try_from(data_block).context("publication data block overflow")?;
        let in_block = usize::try_from(data_offset).context("publication data offset overflow")?;
        ensure!(
            in_block < region.block_size(),
            "publication data offset exceeds block size"
        );

        let chunk_index = self
            .block_to_chunk
            .get(block)
            .copied()
            .flatten()
            .context("publication data record is not inside a data chunk")?;
        let chunk = &self.chunks[chunk_index];
        let chunk_start_block =
            usize::try_from(chunk.start_block).context("publication chunk start overflow")?;
        let chunk_end_block_exclusive =
            usize::try_from(chunk.end_block_exclusive).context("publication chunk end overflow")?;
        ensure!(
            block >= chunk_start_block && block < chunk_end_block_exclusive,
            "publication data record is not inside a data chunk"
        );

        let in_chunk = block
            .checked_sub(chunk_start_block)
            .and_then(|delta| delta.checked_mul(region.block_size()))
            .and_then(|offset| offset.checked_add(in_block))
            .context("publication data record offset overflow")?;
        ensure!(
            in_chunk >= chunk.header_boundary_offset,
            "publication data record points into data chunk header"
        );
        let record_end = in_chunk
            .checked_add(size_of::<TxDataRecordHeader>())
            .context("publication data record offset overflow")?;
        ensure!(
            record_end <= chunk.chunk_tail_offset,
            "publication data record is not fully inside a data chunk"
        );

        let record_offset = chunk
            .chunk_start_byte_offset
            .checked_add(in_chunk)
            .context("publication data record offset overflow")?;
        let chunk_tail_offset = chunk
            .chunk_start_byte_offset
            .checked_add(chunk.chunk_tail_offset)
            .context("publication data record offset overflow")?;
        Ok((record_offset, chunk_tail_offset))
    }
}

fn discover_region(region: &BlockRegionBackendView<'_>) -> Result<DiscoveredRegion> {
    let mut streams = BTreeMap::<u32, DiscoveredStreamParts>::new();
    let mut data_chunk_index = DataChunkIndex::new(region.num_blocks());
    let mut block = 0usize;

    while block < region.num_blocks() {
        let start_block = u32::try_from(block).context("transactional recovery block overflow")?;
        let meta = region.block_meta(start_block)?;
        if !meta.is_active_or_sealed()? {
            block += 1;
            continue;
        }
        match region.block_magic(start_block)? {
            LOG_BLOCK_MAGIC => {
                if meta.kind()? != BlockKind::Log {
                    block += 1;
                    continue;
                }
                let header = region.log_block_header(start_block)?;
                streams
                    .entry(header.stream_id)
                    .or_default()
                    .log_blocks
                    .push((header.block_seq, start_block));
                block += 1;
            }
            DATA_CHUNK_MAGIC => {
                if !matches!(meta.kind()?, BlockKind::ObjectData | BlockKind::LinearUndo) {
                    block += 1;
                    continue;
                }
                let header = region.data_chunk_header(start_block)?;
                let chunk_blocks = validated_chunk_blocks(region, block, start_block, header)?;
                streams
                    .entry(header.stream_id)
                    .or_default()
                    .data_chunks
                    .push((header.chunk_seq, start_block));
                data_chunk_index.push_chunk(region, block, start_block, chunk_blocks, header)?;
                block += chunk_blocks;
            }
            _ => {
                block += 1;
            }
        }
    }

    Ok(DiscoveredRegion {
        streams: streams
            .into_iter()
            .map(|(stream_id, mut parts)| {
                parts.log_blocks.sort_unstable();
                parts.data_chunks.sort_unstable();
                RecoveredStream {
                    stream_id,
                    log_blocks: parts
                        .log_blocks
                        .into_iter()
                        .map(|(_, start_block)| start_block)
                        .collect(),
                    data_chunks: parts
                        .data_chunks
                        .into_iter()
                        .map(|(_, start_block)| start_block)
                        .collect(),
                }
            })
            .collect(),
        data_chunk_index,
    })
}

fn expected_data_kind(role: TxLogEntryRole) -> BlockKind {
    match role {
        TxLogEntryRole::TObjectPub => BlockKind::ObjectData,
        TxLogEntryRole::TMemoryUndo => BlockKind::LinearUndo,
    }
}

fn validate_entry_data_block(
    region: &BlockRegionBackendView<'_>,
    entry: TxLogEntry,
) -> Result<bool> {
    let meta = region.block_meta(entry.data_block)?;
    if !meta.is_active_or_sealed()? {
        return Ok(false);
    }
    if meta.kind()? != expected_data_kind(entry.role()?) {
        return Ok(false);
    }
    Ok(meta.generation == entry.data_block_generation())
}

fn replay_stream(
    region: &BlockRegionBackendView<'_>,
    stream: &RecoveredStream,
    data_chunk_index: &DataChunkIndex,
) -> Result<StreamReplay> {
    let mut replay = StreamReplay::default();
    let mut pending: Option<PendingTransaction> = None;

    for &log_block in &stream.log_blocks {
        let entries = region.log_block_entries(log_block)?;
        for entry in entries {
            ensure!(
                entry.validate_crc32(),
                "transactional recovery found corrupt log entry in stream {} block {}",
                stream.stream_id,
                log_block
            );
            let txid = txid(entry);

            if pending.as_ref().is_some_and(|txn| txn.txid != txid) {
                if let Some(txn) = pending.take() {
                    replay
                        .tmemory_undo_rollbacks
                        .extend(loose_end_tmemory_undo_rollbacks(
                            region,
                            data_chunk_index,
                            txn.entries,
                        )?);
                }
            }
            let txn = pending.get_or_insert_with(|| PendingTransaction {
                txid,
                entries: Vec::new(),
            });

            if is_final_lp(entry) {
                txn.entries.push(entry);
                for committed in &txn.entries {
                    if !validate_entry_data_block(region, *committed)? {
                        continue;
                    }
                    validate_committed_entry(region, data_chunk_index, *committed)?;
                    if committed.role()? == TxLogEntryRole::TObjectPub {
                        replay.winners.push(RecoveryWinner {
                            logical_id: committed.logical_id,
                            version: committed.version,
                            data_block: committed.data_block,
                            data_offset: committed.data_offset,
                        });
                    }
                }
                pending = None;
            } else {
                txn.entries.push(entry);
            }
        }
    }

    if let Some(txn) = pending.take() {
        replay
            .tmemory_undo_rollbacks
            .extend(loose_end_tmemory_undo_rollbacks(
                region,
                data_chunk_index,
                txn.entries,
            )?);
    }

    Ok(replay)
}

fn validate_committed_entry(
    region: &BlockRegionBackendView<'_>,
    data_chunk_index: &DataChunkIndex,
    entry: TxLogEntry,
) -> Result<()> {
    let (data_header, _) = load_publication_payload(
        region,
        data_chunk_index,
        entry.data_block,
        entry.data_offset,
    )?;
    ensure!(
        data_header.logical_id == entry.logical_id,
        "committed data record logical id does not match log entry"
    );
    ensure!(
        data_header.version == entry.version,
        "committed data record version does not match log entry"
    );

    match entry.role()? {
        TxLogEntryRole::TObjectPub => ensure!(
            data_header.role()? == TxDataRecordRole::TObjectPub,
            "recovered object publication data record has non-publication role"
        ),
        TxLogEntryRole::TMemoryUndo => ensure!(
            data_header.role()? == TxDataRecordRole::TMemoryUndo,
            "tmemory undo data record has non-undo role"
        ),
    }

    Ok(())
}

fn loose_end_tmemory_undo_rollbacks(
    region: &BlockRegionBackendView<'_>,
    data_chunk_index: &DataChunkIndex,
    entries: Vec<TxLogEntry>,
) -> Result<Vec<RecoveredTMemoryUndoRollback>> {
    let mut rollbacks = Vec::new();
    for entry in entries {
        if entry.role()? != TxLogEntryRole::TMemoryUndo {
            continue;
        }
        if !validate_entry_data_block(region, entry)? {
            continue;
        }
        let (data_header, old_granule_bytes) = load_publication_payload(
            region,
            data_chunk_index,
            entry.data_block,
            entry.data_offset,
        )?;
        ensure!(
            data_header.role()? == TxDataRecordRole::TMemoryUndo,
            "tmemory undo data record has non-undo role"
        );
        ensure!(
            data_header.logical_id == entry.logical_id,
            "tmemory undo logical id does not match log entry"
        );
        ensure!(
            data_header.version == entry.version,
            "tmemory undo version does not match log entry"
        );
        ensure!(
            data_header.kind == PackedGranuleDomain::TMemory as u16,
            "tmemory undo data record kind is not TMemory"
        );
        rollbacks.push(RecoveredTMemoryUndoRollback {
            logical_id: entry.logical_id,
            version: entry.version,
            data_block: entry.data_block,
            data_offset: entry.data_offset,
            old_granule_bytes,
        });
    }
    Ok(rollbacks)
}

fn txid(entry: TxLogEntry) -> u32 {
    entry.tx_meta >> 1
}

fn is_final_lp(entry: TxLogEntry) -> bool {
    (entry.tx_meta & 1) != 0
}

#[cfg(test)]
pub(crate) fn recover_region_for_test(region: &VMemoryBlockRegion) -> Result<RecoveredRegion> {
    let type_layouts = region.load_type_layout_metadata()?;
    recover_region(&region.view(), type_layouts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::transaction::{
        ObjectPayload, ObjectValue, encode_object_record_for_test,
        type_layout::{PersistentTypeLayout, StructTraceField, TraceSlotKind, TypeLayoutId},
    };
    use crate::runtime::vm::memory::tmemory::durable_log::{BlockKind, BlockMeta};
    use crate::runtime::vm::memory::tmemory::metadata::{
        TYPE_LAYOUT_METADATA_HEADER_LEN, TYPE_LAYOUT_METADATA_START_BLOCK,
    };
    use crate::runtime::vm::{
        PackedGranuleDomain, pack_object_granule_id, pack_tmemory_size_logical_id,
    };

    #[test]
    fn recovery_discovers_streams_from_chunk_starts() {
        let region = sample_region_with_two_streams();
        let recovered = recover_region_for_test(&region).unwrap();
        assert_eq!(recovered.streams.len(), 2);
        assert_eq!(recovered.next_stream_id, 3);
    }

    #[test]
    fn recovery_discards_loose_end_without_lp() {
        let region = sample_region_with_trailing_uncommitted_tx();
        let recovered = recover_region_for_test(&region).unwrap();
        assert_eq!(recovered.winners.len(), 1);
        assert_eq!(recovered.winners[0].version, 1);
    }

    #[test]
    fn recovery_picks_highest_version_per_logical_id() {
        let region = sample_region_with_competing_stream_updates();
        let recovered = recover_region_for_test(&region).unwrap();
        assert_eq!(recovered.winners[0].version, 9);
    }

    #[test]
    fn recovery_rejects_corrupt_non_final_log_entry() {
        let region = sample_region_with_corrupt_non_final_log_entry();
        let err = recover_region_for_test(&region).unwrap_err();
        assert!(err.to_string().contains("corrupt log entry"));
    }

    #[test]
    fn recovery_replays_latest_object_record_per_object_id() {
        let region = sample_region_with_competing_object_updates();
        let recovered = recover_region_for_test(&region).unwrap();
        let objects = recovered.committed_object_winners().unwrap();

        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].object_id, 41);
        assert_eq!(objects[0].version, 9);
        assert_eq!(
            objects[0].kind,
            crate::runtime::transaction::ObjectKind::Struct as u16
        );
    }

    #[test]
    fn recovery_rejects_duplicate_committed_object_version() {
        let region = sample_region_with_duplicate_object_version();
        let err = recover_region_for_test(&region).unwrap_err();
        assert!(
            err.to_string()
                .contains("duplicate committed object version")
        );
    }

    #[test]
    fn recovery_keeps_unreachable_object_record_with_missing_type_layout_metadata() {
        let region = sample_region_with_missing_object_type_layout_metadata();
        let recovered = recover_region_for_test(&region).unwrap();
        let objects = recovered.committed_object_winners().unwrap();

        assert!(recovered.root_object_ids.is_empty());
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].object_id, 41);
        assert_eq!(objects[0].version, 1);
        assert_eq!(objects[0].kind, ObjectKind::Struct as u16);
        assert_eq!(objects[0].type_layout_id, 101);
    }

    #[test]
    fn recovery_keeps_unreachable_object_record_kind_layout_mismatch() {
        let region = sample_region_with_object_kind_layout_mismatch();
        let recovered = recover_region_for_test(&region).unwrap();
        let objects = recovered.committed_object_winners().unwrap();

        assert!(recovered.root_object_ids.is_empty());
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].object_id, 41);
        assert_eq!(objects[0].version, 1);
        assert_eq!(objects[0].kind, ObjectKind::Array as u16);
        assert_eq!(objects[0].type_layout_id, 101);
    }

    #[test]
    fn recovery_rejects_corrupt_in_memory_type_layout_metadata() {
        let region = sample_region_with_corrupt_type_layout_metadata();
        let err = recover_region_for_test(&region).unwrap_err();

        assert!(err.to_string().contains("unknown persistent type kind"));
    }

    #[test]
    fn recovery_accepts_struct_object_with_matching_struct_layout() {
        let layout = struct_layout(101);
        let region = sample_region_with_matching_struct_layout(layout.clone());
        let recovered = recover_region_for_test(&region).unwrap();
        let objects = recovered.committed_object_winners().unwrap();

        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].object_id, 41);
        assert_eq!(objects[0].type_layout_id, layout.id().get());
        assert!(recovered.type_layouts.contains(layout.id()));
    }

    #[test]
    fn recovery_accepts_array_object_with_matching_array_layout() {
        let layout = array_layout(102);
        let region = sample_region_with_matching_array_layout(layout.clone());
        let recovered = recover_region_for_test(&region).unwrap();
        let objects = recovered.committed_object_winners().unwrap();

        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].object_id, 42);
        assert_eq!(objects[0].type_layout_id, layout.id().get());
        assert!(recovered.type_layouts.contains(layout.id()));
    }

    #[test]
    fn recovery_rebuilds_persistent_object_root_from_tglobal() {
        let region = sample_region_with_object_rooted_by_global();
        let recovered = recover_region_for_test(&region).unwrap();

        assert_eq!(recovered.root_object_ids, vec![41]);
    }

    #[test]
    fn recovery_rebuilds_persistent_object_roots_from_ttable() {
        let region = sample_region_with_object_rooted_by_table();
        let recovered = recover_region_for_test(&region).unwrap();

        assert_eq!(recovered.root_object_ids, vec![41, 42]);
    }

    #[test]
    fn recovery_selects_latest_committed_tglobal_root_version() {
        let region = sample_region_with_rewritten_global_root();
        let recovered = recover_region_for_test(&region).unwrap();

        assert_eq!(recovered.root_object_ids, vec![42]);
    }

    #[test]
    fn recovery_ignores_loose_end_ttable_root_update() {
        let region = sample_region_with_loose_end_table_root_update();
        let recovered = recover_region_for_test(&region).unwrap();

        assert_eq!(recovered.root_object_ids, vec![41]);
    }

    #[test]
    fn recovery_replays_tmemory_undo_for_loose_end_transaction() {
        let region = sample_region_with_loose_end_tmemory_undo();
        let recovered = recover_region_for_test(&region).unwrap();

        assert_eq!(recovered.winners.len(), 0);
        assert_eq!(recovered.tmemory_undo_rollbacks.len(), 1);
        assert_eq!(
            recovered.tmemory_undo_rollbacks[0].logical_id,
            pack_test_granule_id(PackedGranuleDomain::TMemory, 7)
        );
        assert_eq!(
            recovered.tmemory_undo_rollbacks[0].old_granule_bytes,
            vec![9, 8, 7, 6]
        );
    }

    #[test]
    fn recovery_skips_stale_generation_committed_entry_before_payload_decode() {
        let region = sample_region_with_stale_generation_committed_entry();
        let recovered = recover_region_for_test(&region).unwrap();

        assert!(recovered.winners.is_empty());
        assert!(recovered.object_winners.is_empty());
    }

    #[test]
    fn recovery_skips_stale_generation_loose_end_tmemory_undo_before_payload_decode() {
        let region = sample_region_with_stale_generation_loose_end_tmemory_undo();
        let recovered = recover_region_for_test(&region).unwrap();

        assert_eq!(recovered.winners.len(), 0);
        assert_eq!(recovered.tmemory_undo_rollbacks.len(), 0);
    }

    #[test]
    fn recovery_ignores_tmemory_undo_for_committed_transaction() {
        let region = sample_region_with_committed_tmemory_undo();
        let recovered = recover_region_for_test(&region).unwrap();

        assert_eq!(recovered.winners.len(), 0);
        assert_eq!(recovered.tmemory_undo_rollbacks.len(), 0);
    }

    #[test]
    fn model_recovery_accepts_idempotent_lp_duplicate_pointer() {
        let region = sample_region_with_duplicate_lp_pointer();
        let recovered = recover_region_for_test(&region).unwrap();
        let objects = recovered.committed_object_winners().unwrap();

        assert_eq!(recovered.winners.len(), 1);
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].object_id, 41);
        assert_eq!(objects[0].version, 9);
    }

    #[test]
    fn model_recovery_rejects_corrupt_log_entry_crc() {
        let region = sample_region_with_corrupt_non_final_log_entry();
        let err = recover_region_for_test(&region).unwrap_err();

        assert!(err.to_string().contains("corrupt log entry"));
    }

    #[test]
    fn model_recovery_rejects_committed_entry_role_mismatch() {
        let region = sample_region_with_committed_entry_role_mismatch();
        let err = recover_region_for_test(&region).unwrap_err();

        assert!(err.to_string().contains("non-undo role"));
    }

    #[test]
    fn model_recovery_rejects_object_publication_data_role_mismatch() {
        let region = sample_region_with_object_publication_data_role_mismatch();
        let err = recover_region_for_test(&region).unwrap_err();

        assert!(err.to_string().contains("non-publication role"));
    }

    #[test]
    fn model_recovery_rejects_tmemory_size_publication_logical_id_mismatch() {
        let (region, winner) =
            sample_region_with_tmemory_size_publication_header_mismatch(|header| {
                header.logical_id = pack_tmemory_size_logical_id(Some(8), 5).unwrap();
            });
        let discovered = discover_region(&region.view()).unwrap();
        let err =
            replay_tmemory_size_winners(&region.view(), &discovered.data_chunk_index, &[winner])
                .unwrap_err();

        assert!(
            err.to_string()
                .contains("recovered tmemory size logical id does not match log winner")
        );
    }

    #[test]
    fn model_recovery_rejects_tmemory_size_publication_version_mismatch() {
        let (region, winner) =
            sample_region_with_tmemory_size_publication_header_mismatch(|header| {
                header.version += 1;
            });
        let discovered = discover_region(&region.view()).unwrap();
        let err =
            replay_tmemory_size_winners(&region.view(), &discovered.data_chunk_index, &[winner])
                .unwrap_err();

        assert!(
            err.to_string()
                .contains("recovered tmemory size version does not match log winner")
        );
    }

    #[test]
    fn model_recovery_rejects_tmemory_size_publication_kind_mismatch() {
        let (region, winner) =
            sample_region_with_tmemory_size_publication_header_mismatch(|header| {
                header.kind = PackedGranuleDomain::TMemory as u16;
            });
        let discovered = discover_region(&region.view()).unwrap();
        let err =
            replay_tmemory_size_winners(&region.view(), &discovered.data_chunk_index, &[winner])
                .unwrap_err();

        assert!(
            err.to_string()
                .contains("recovered tmemory size kind does not match log winner")
        );
    }

    #[test]
    fn model_recovery_rejects_object_publication_outer_type_layout_id_mismatch() {
        let region = sample_region_with_object_publication_outer_type_layout_id_mismatch();
        let err = recover_region_for_test(&region).unwrap_err();

        assert!(
            err.to_string()
                .contains("outer type layout id does not match object record")
        );
    }

    #[test]
    fn model_recovery_rejects_object_publication_outer_domain_kind_mismatch() {
        let region = sample_region_with_object_publication_outer_domain_kind_mismatch();
        let err = recover_region_for_test(&region).unwrap_err();

        assert!(
            err.to_string()
                .contains("outer kind does not match object record kind")
        );
    }

    #[test]
    fn model_recovery_rejects_publication_pointer_outside_data_chunk() {
        let region = sample_region_with_publication_pointer_outside_data_chunk();
        let err = recover_region_for_test(&region).unwrap_err();

        assert!(err.to_string().contains("data chunk"));
    }

    #[test]
    fn model_recovery_accepts_publication_pointer_in_later_block_of_multi_block_chunk() {
        let region = sample_region_with_multiblock_publication_pointer_in_later_block();
        let recovered = recover_region_for_test(&region).unwrap();

        assert_eq!(recovered.winners.len(), 1);
        assert_eq!(recovered.winners[0].logical_id, 0x3000);
        assert_eq!(recovered.winners[0].version, 5);
    }

    #[test]
    fn model_recovery_rejects_publication_payload_crossing_multi_block_chunk_tail() {
        let region = sample_region_with_multiblock_publication_payload_crossing_tail();
        let err = recover_region_for_test(&region).unwrap_err();

        assert!(err.to_string().contains("extends beyond data chunk tail"));
    }

    #[test]
    fn model_recovery_rejects_publication_pointer_into_multi_block_chunk_header() {
        let region = sample_region_with_multiblock_publication_pointer_into_header();
        let err = recover_region_for_test(&region).unwrap_err();

        assert!(err.to_string().contains("points into data chunk header"));
    }

    fn sample_region_with_two_streams() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream1 = region.alloc_stream(1).unwrap();
        let stream2 = region.alloc_stream(2).unwrap();
        append_committed_update(&mut region, 1, stream1, 0, 0x1000, 1, 11);
        append_committed_update(&mut region, 2, stream2, 0, 0x2000, 1, 22);
        region
    }

    fn sample_region_with_trailing_uncommitted_tx() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        append_committed_update(&mut region, 1, stream, 0, 0x1000, 1, 11);
        append_loose_end_update(&mut region, 1, stream, 1, 0x1000, 2, 22);
        region
    }

    fn sample_region_with_competing_stream_updates() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream1 = region.alloc_stream(1).unwrap();
        let stream2 = region.alloc_stream(2).unwrap();
        append_committed_update(&mut region, 1, stream1, 0, 0x1000, 7, 11);
        append_committed_update(&mut region, 2, stream2, 0, 0x1000, 9, 22);
        region
    }

    fn sample_region_with_corrupt_non_final_log_entry() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();

        let first = append_publication_record(&mut region, 1, stream, 0, 0x1000, 1, 11);
        let second = append_publication_record(&mut region, 1, stream, 0, 0x1001, 1, 22);

        let mut ordinary =
            TxLogEntry::new(0x1000, 1, 1 << 1, first.1.data_block, first.1.data_offset);
        ordinary.seal_crc32();

        let mut final_lp = TxLogEntry::new(
            0x1001,
            1,
            (1 << 1) | 1,
            second.1.data_block,
            second.1.data_offset,
        );
        final_lp.seal_crc32();

        write_log_entries(&mut region, first.0, &[ordinary, final_lp]);
        corrupt_log_entry_data_block(&mut region, first.0, 0);
        region
    }

    fn sample_region_with_competing_object_updates() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream1 = region.alloc_stream(1).unwrap();
        let stream2 = region.alloc_stream(2).unwrap();
        region
            .append_type_layout_metadata(&struct_layout(11))
            .unwrap();
        region
            .append_type_layout_metadata(&struct_layout(22))
            .unwrap();
        let logical_id = pack_object_granule_id(PackedGranuleDomain::TStruct, 41).unwrap();
        append_committed_object_update(
            &mut region,
            1,
            stream1,
            0,
            logical_id,
            7,
            11,
            ObjectPayload::Struct(vec![ObjectValue::I32(7)]),
        );
        append_committed_object_update(
            &mut region,
            2,
            stream2,
            0,
            logical_id,
            9,
            22,
            ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
        );
        region
    }

    fn sample_region_with_duplicate_object_version() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream1 = region.alloc_stream(1).unwrap();
        let stream2 = region.alloc_stream(2).unwrap();
        region
            .append_type_layout_metadata(&struct_layout(11))
            .unwrap();
        region
            .append_type_layout_metadata(&struct_layout(22))
            .unwrap();
        let logical_id = pack_object_granule_id(PackedGranuleDomain::TStruct, 41).unwrap();
        append_committed_object_update(
            &mut region,
            1,
            stream1,
            0,
            logical_id,
            9,
            11,
            ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
        );
        append_committed_object_update(
            &mut region,
            2,
            stream2,
            0,
            logical_id,
            9,
            22,
            ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
        );
        region
    }

    fn sample_region_with_missing_object_type_layout_metadata() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        append_committed_object_update(
            &mut region,
            1,
            stream,
            0,
            pack_object_granule_id(PackedGranuleDomain::TStruct, 41).unwrap(),
            1,
            101,
            ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
        );
        region
    }

    fn sample_region_with_object_kind_layout_mismatch() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        region
            .append_type_layout_metadata(&struct_layout(101))
            .unwrap();
        append_committed_object_update(
            &mut region,
            1,
            stream,
            0,
            pack_object_granule_id(PackedGranuleDomain::TArray, 41).unwrap(),
            1,
            101,
            ObjectPayload::Array(vec![ObjectValue::I32(9)]),
        );
        region
    }

    fn sample_region_with_matching_struct_layout(
        layout: PersistentTypeLayout,
    ) -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        let type_layout_id = layout.id().get();
        region.append_type_layout_metadata(&layout).unwrap();
        append_committed_object_update(
            &mut region,
            1,
            stream,
            0,
            pack_object_granule_id(PackedGranuleDomain::TStruct, 41).unwrap(),
            1,
            type_layout_id,
            ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
        );
        region
    }

    fn sample_region_with_matching_array_layout(
        layout: PersistentTypeLayout,
    ) -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        let type_layout_id = layout.id().get();
        region.append_type_layout_metadata(&layout).unwrap();
        append_committed_object_update(
            &mut region,
            1,
            stream,
            0,
            pack_object_granule_id(PackedGranuleDomain::TArray, 42).unwrap(),
            1,
            type_layout_id,
            ObjectPayload::Array(vec![ObjectValue::I32(11)]),
        );
        region
    }

    fn sample_region_with_corrupt_type_layout_metadata() -> VMemoryBlockRegion {
        let layout = struct_layout(101);
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        region.append_type_layout_metadata(&layout).unwrap();
        let corrupt_offset = usize::try_from(TYPE_LAYOUT_METADATA_START_BLOCK).unwrap()
            * BLOCK_SIZE
            + TYPE_LAYOUT_METADATA_HEADER_LEN
            + 12;
        region
            .write(corrupt_offset, &0xffff_u16.to_le_bytes())
            .unwrap();
        region
    }

    fn sample_region_with_object_rooted_by_global() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        append_committed_root_update(
            &mut region,
            1,
            stream,
            0,
            pack_test_granule_id(PackedGranuleDomain::TGlobal, 1),
            1,
            &[Some(41)],
        );
        region
    }

    fn sample_region_with_object_rooted_by_table() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        append_committed_root_update(
            &mut region,
            1,
            stream,
            0,
            pack_test_granule_id(PackedGranuleDomain::TTable, 1),
            1,
            &[None, Some(42), Some(41), Some(41)],
        );
        region
    }

    fn sample_region_with_rewritten_global_root() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        let logical_id = pack_test_granule_id(PackedGranuleDomain::TGlobal, 1);
        append_committed_root_update(&mut region, 1, stream, 0, logical_id, 1, &[Some(41)]);
        append_committed_root_update(&mut region, 1, stream, 1, logical_id, 2, &[Some(42)]);
        region
    }

    fn sample_region_with_loose_end_table_root_update() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let committed_stream = region.alloc_stream(1).unwrap();
        let loose_stream = region.alloc_stream(2).unwrap();
        let logical_id = pack_test_granule_id(PackedGranuleDomain::TTable, 1);
        append_committed_root_update(
            &mut region,
            1,
            committed_stream,
            0,
            logical_id,
            1,
            &[Some(41)],
        );
        append_loose_end_root_update(&mut region, 2, loose_stream, 0, logical_id, 2, &[Some(42)]);
        region
    }

    fn sample_region_with_loose_end_tmemory_undo() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        append_tmemory_undo_record(
            &mut region,
            1,
            stream,
            0,
            pack_test_granule_id(PackedGranuleDomain::TMemory, 7),
            3,
            &[9, 8, 7, 6],
            false,
        );
        region
    }

    fn sample_region_with_stale_generation_loose_end_tmemory_undo() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        let (_, location) = append_tmemory_undo_record(
            &mut region,
            1,
            stream,
            0,
            pack_test_granule_id(PackedGranuleDomain::TMemory, 7),
            3,
            &[9, 8, 7, 6],
            false,
        );
        region
            .bump_block_generation_for_test(location.data_block)
            .unwrap();
        rewrite_data_record_header(&mut region, location, |header| {
            header.role = 0xa5a5;
        });
        region
    }

    fn sample_region_with_committed_tmemory_undo() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        append_tmemory_undo_record(
            &mut region,
            1,
            stream,
            0,
            pack_test_granule_id(PackedGranuleDomain::TMemory, 7),
            3,
            &[9, 8, 7, 6],
            true,
        );
        region
    }

    fn sample_region_with_stale_generation_committed_entry() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        region
            .append_type_layout_metadata(&struct_layout(22))
            .unwrap();
        let logical_id = pack_object_granule_id(PackedGranuleDomain::TStruct, 41).unwrap();
        let (_, location) = append_committed_object_update_raw(
            &mut region,
            1,
            stream,
            0,
            logical_id,
            9,
            22,
            ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
        );
        region
            .bump_block_generation_for_test(location.data_block)
            .unwrap();
        rewrite_data_record_header(&mut region, location, |header| {
            header.role = 0xa5a5;
        });
        region
    }

    fn sample_region_with_duplicate_lp_pointer() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        region
            .append_type_layout_metadata(&struct_layout(22))
            .unwrap();
        let logical_id = pack_object_granule_id(PackedGranuleDomain::TStruct, 41).unwrap();
        let (log_block, location) = append_committed_object_update_raw(
            &mut region,
            1,
            stream,
            0,
            logical_id,
            9,
            22,
            ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
        );
        let entry = TMemory::publication_log_entry(
            logical_id,
            9,
            1 << 1,
            location.data_block,
            location.data_offset,
            0,
            true,
        )
        .unwrap();
        write_log_entries(&mut region, log_block, &[entry, entry]);
        region
    }

    fn sample_region_with_committed_entry_role_mismatch() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        let logical_id = pack_test_granule_id(PackedGranuleDomain::TMemory, 7);
        let (_, location) = append_tmemory_undo_record(
            &mut region,
            1,
            stream,
            0,
            logical_id,
            3,
            &[9, 8, 7, 6],
            true,
        );
        rewrite_data_record_header(&mut region, location, |header| {
            header.set_role(TxDataRecordRole::TObjectPub);
        });
        region
    }

    fn sample_region_with_object_publication_data_role_mismatch() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        region
            .append_type_layout_metadata(&struct_layout(22))
            .unwrap();
        let logical_id = pack_object_granule_id(PackedGranuleDomain::TStruct, 41).unwrap();
        let (_, location) = append_committed_object_update_raw(
            &mut region,
            1,
            stream,
            0,
            logical_id,
            9,
            22,
            ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
        );
        rewrite_data_record_header(&mut region, location, |header| {
            header.set_role(TxDataRecordRole::TMemoryUndo);
        });
        region
    }

    fn sample_region_with_tmemory_size_publication_header_mismatch(
        update: impl FnOnce(&mut TxDataRecordHeader),
    ) -> (VMemoryBlockRegion, RecoveryWinner) {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        let logical_id = pack_tmemory_size_logical_id(Some(7), 4).unwrap();
        let payload = 2u64.to_le_bytes();
        let record = TMemory::encode_publication_data_record(
            logical_id,
            9,
            PackedGranuleDomain::TMemorySize as u16,
            0,
            &payload,
        )
        .unwrap();
        let location = region.append_data_record(stream, &record).unwrap();
        let log_block = region.alloc_log_block(1, 0).unwrap();
        let entry = TMemory::publication_log_entry(
            logical_id,
            9,
            1 << 1,
            location.data_block,
            location.data_offset,
            0,
            true,
        )
        .unwrap();
        write_log_entries(&mut region, log_block, &[entry]);
        rewrite_data_record_header(&mut region, location, update);
        (
            region,
            RecoveryWinner {
                logical_id,
                version: 9,
                data_block: location.data_block,
                data_offset: location.data_offset,
            },
        )
    }

    fn sample_region_with_publication_pointer_outside_data_chunk() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        region
            .append_type_layout_metadata(&struct_layout(22))
            .unwrap();
        let logical_id = pack_object_granule_id(PackedGranuleDomain::TStruct, 41).unwrap();
        let (log_block, _) = append_committed_object_update_raw(
            &mut region,
            1,
            stream,
            0,
            logical_id,
            9,
            22,
            ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
        );
        let stray_block = region.alloc_log_block(2, 0).unwrap();
        rewrite_log_entry(&mut region, log_block, 0, |entry| {
            entry.data_block = stray_block;
            entry.data_offset = 0;
        });
        region
            .write_block_meta_for_test(
                stray_block,
                BlockMeta::active(BlockKind::ObjectData, 0, 2, stray_block, 1),
            )
            .unwrap();
        region
    }

    fn sample_region_with_object_publication_outer_type_layout_id_mismatch() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        region
            .append_type_layout_metadata(&struct_layout(22))
            .unwrap();
        region
            .append_type_layout_metadata(&struct_layout(23))
            .unwrap();
        let logical_id = pack_object_granule_id(PackedGranuleDomain::TStruct, 41).unwrap();
        let (_, location) = append_committed_object_update_raw(
            &mut region,
            1,
            stream,
            0,
            logical_id,
            9,
            22,
            ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
        );
        rewrite_data_record_header(&mut region, location, |header| {
            header.type_info = 23;
        });
        region
    }

    fn sample_region_with_object_publication_outer_domain_kind_mismatch() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        region
            .append_type_layout_metadata(&struct_layout(22))
            .unwrap();
        let logical_id = pack_object_granule_id(PackedGranuleDomain::TArray, 41).unwrap();
        let _ = append_committed_object_update_raw(
            &mut region,
            1,
            stream,
            0,
            logical_id,
            9,
            22,
            ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
        );
        region
    }

    fn sample_region_with_multiblock_publication_pointer_in_later_block() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        append_committed_multiblock_update(&mut region, 1, stream, 0, 0x3000, 5, 0x5a);
        region
    }

    fn sample_region_with_multiblock_publication_payload_crossing_tail() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        let (_, location) =
            append_committed_multiblock_update(&mut region, 1, stream, 0, 0x3000, 5, 0x5a);
        rewrite_data_record_header(&mut region, location, |header| {
            header.payload_len += 1;
        });
        region
    }

    fn sample_region_with_multiblock_publication_pointer_into_header() -> VMemoryBlockRegion {
        let mut region = VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream = region.alloc_stream(1).unwrap();
        let (log_block, location) =
            append_committed_multiblock_update(&mut region, 1, stream, 0, 0x3000, 5, 0x5a);
        rewrite_log_entry(&mut region, log_block, 0, |entry| {
            entry.data_block = location.chunk_start_block;
            entry.data_offset = 0;
        });
        region
    }

    fn append_committed_root_update(
        region: &mut VMemoryBlockRegion,
        stream_id: u32,
        stream: StreamCursor,
        block_seq: u32,
        logical_id: u64,
        version: u32,
        object_ids: &[Option<u64>],
    ) {
        let domain = packed_granule_domain(logical_id).unwrap();
        let payload = encode_root_object_refs(object_ids);
        let record = TMemory::encode_publication_data_record(
            logical_id,
            version,
            domain as u16,
            0,
            &payload,
        )
        .unwrap();
        let location = region.append_data_record(stream, &record).unwrap();
        let log_block = region.alloc_log_block(stream_id, block_seq).unwrap();
        let entry = TMemory::publication_log_entry(
            logical_id,
            version,
            stream_id << 1,
            location.data_block,
            location.data_offset,
            0,
            true,
        )
        .unwrap();
        write_log_entries(region, log_block, &[entry]);
    }

    fn append_loose_end_root_update(
        region: &mut VMemoryBlockRegion,
        stream_id: u32,
        stream: StreamCursor,
        block_seq: u32,
        logical_id: u64,
        version: u32,
        object_ids: &[Option<u64>],
    ) {
        let domain = packed_granule_domain(logical_id).unwrap();
        let payload = encode_root_object_refs(object_ids);
        let record = TMemory::encode_publication_data_record(
            logical_id,
            version,
            domain as u16,
            0,
            &payload,
        )
        .unwrap();
        let location = region.append_data_record(stream, &record).unwrap();
        let log_block = region.alloc_log_block(stream_id, block_seq).unwrap();
        let entry = TMemory::publication_log_entry(
            logical_id,
            version,
            stream_id << 1,
            location.data_block,
            location.data_offset,
            0,
            false,
        )
        .unwrap();
        write_log_entries(region, log_block, &[entry]);
    }

    fn append_tmemory_undo_record(
        region: &mut VMemoryBlockRegion,
        stream_id: u32,
        stream: StreamCursor,
        block_seq: u32,
        logical_id: u64,
        version: u32,
        old_granule_bytes: &[u8],
        is_final_lp: bool,
    ) -> (u32, DataRecordLocation) {
        let record = TMemory::encode_granule_undo_data_record(
            logical_id,
            version,
            PackedGranuleDomain::TMemory as u16,
            0,
            old_granule_bytes,
        )
        .unwrap();
        let location = region.append_data_record(stream, &record).unwrap();
        let log_block = region.alloc_log_block(stream_id, block_seq).unwrap();
        let tx_meta = if is_final_lp {
            (stream_id << 1) | 1
        } else {
            stream_id << 1
        };
        let mut entry = TxLogEntry::new(
            logical_id,
            version,
            tx_meta,
            location.data_block,
            location.data_offset,
        );
        entry.set_role(TxLogEntryRole::TMemoryUndo);
        entry.seal_crc32();
        write_log_entries(region, log_block, &[entry]);
        (log_block, location)
    }

    fn encode_root_object_refs(object_ids: &[Option<u64>]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for object_id in object_ids {
            let raw = PersistentObjectRefRaw::from_optional_object_index(*object_id)
                .unwrap()
                .as_raw();
            bytes.extend_from_slice(&raw.to_le_bytes());
        }
        bytes
    }

    fn pack_test_granule_id(domain: PackedGranuleDomain, payload: u64) -> u64 {
        ((domain as u64) << 60) | payload
    }

    fn struct_layout(raw_id: u32) -> PersistentTypeLayout {
        PersistentTypeLayout::Struct {
            id: TypeLayoutId::new(raw_id).unwrap(),
            fingerprint: 0x5354_5255_4354_0000 | u64::from(raw_id),
            body_size: 8,
            fields: vec![StructTraceField {
                field_index: 0,
                field_offset: 0,
                value_size: 4,
                kind: TraceSlotKind::Scalar,
            }],
        }
    }

    fn array_layout(raw_id: u32) -> PersistentTypeLayout {
        PersistentTypeLayout::Array {
            id: TypeLayoutId::new(raw_id).unwrap(),
            fingerprint: 0x4152_5241_5900_0000 | u64::from(raw_id),
            element_size: 4,
            element_kind: TraceSlotKind::Scalar,
        }
    }

    fn append_committed_object_update(
        region: &mut VMemoryBlockRegion,
        stream_id: u32,
        stream: StreamCursor,
        block_seq: u32,
        logical_id: u64,
        version: u32,
        type_layout_id: u32,
        payload: ObjectPayload,
    ) {
        let _ = append_committed_object_update_raw(
            region,
            stream_id,
            stream,
            block_seq,
            logical_id,
            version,
            type_layout_id,
            payload,
        );
    }

    fn append_committed_object_update_raw(
        region: &mut VMemoryBlockRegion,
        stream_id: u32,
        stream: StreamCursor,
        block_seq: u32,
        logical_id: u64,
        version: u32,
        type_layout_id: u32,
        payload: ObjectPayload,
    ) -> (u32, DataRecordLocation) {
        let (domain, object_id) = unpack_object_granule_id(logical_id).unwrap();
        let object_record =
            encode_object_record_for_test(object_id, version, type_layout_id, &payload).unwrap();
        let publication = TMemory::encode_publication_data_record(
            logical_id,
            version,
            domain as u16,
            type_layout_id,
            &object_record,
        )
        .unwrap();
        let location = region.append_data_record(stream, &publication).unwrap();
        let log_block = region.alloc_log_block(stream_id, block_seq).unwrap();
        let entry = TMemory::publication_log_entry(
            logical_id,
            version,
            stream_id << 1,
            location.data_block,
            location.data_offset,
            0,
            true,
        )
        .unwrap();
        write_log_entries(region, log_block, &[entry]);
        (log_block, location)
    }

    fn append_committed_update(
        region: &mut VMemoryBlockRegion,
        stream_id: u32,
        stream: StreamCursor,
        block_seq: u32,
        logical_id: u64,
        version: u32,
        fill: u8,
    ) {
        let (log_block, location) = append_publication_record(
            region, stream_id, stream, block_seq, logical_id, version, fill,
        );
        let entry = TMemory::publication_log_entry(
            logical_id,
            version,
            stream_id << 1,
            location.data_block,
            location.data_offset,
            0,
            true,
        )
        .unwrap();
        write_log_entries(region, log_block, &[entry]);
    }

    fn append_committed_multiblock_update(
        region: &mut VMemoryBlockRegion,
        stream_id: u32,
        stream: StreamCursor,
        block_seq: u32,
        logical_id: u64,
        version: u32,
        fill: u8,
    ) -> (u32, DataRecordLocation) {
        let filler = TMemory::encode_publication_data_record(
            0x2000,
            1,
            PackedGranuleDomain::TMemory as u16,
            0,
            &vec![
                0xa5;
                BLOCK_SIZE - size_of::<DataChunkHeader>() + 1 - size_of::<TxDataRecordHeader>()
            ],
        )
        .unwrap();
        let filler_location = region.append_data_record(stream, &filler).unwrap();
        let (log_block, location) = append_publication_record(
            region, stream_id, stream, block_seq, logical_id, version, fill,
        );
        let entry = TMemory::publication_log_entry(
            logical_id,
            version,
            stream_id << 1,
            location.data_block,
            location.data_offset,
            0,
            true,
        )
        .unwrap();
        write_log_entries(region, log_block, &[entry]);

        assert_eq!(
            location.chunk_start_block,
            filler_location.chunk_start_block
        );
        assert!(location.data_block > location.chunk_start_block);

        (log_block, location)
    }

    fn append_loose_end_update(
        region: &mut VMemoryBlockRegion,
        stream_id: u32,
        stream: StreamCursor,
        block_seq: u32,
        logical_id: u64,
        version: u32,
        fill: u8,
    ) {
        let (log_block, location) = append_publication_record(
            region, stream_id, stream, block_seq, logical_id, version, fill,
        );
        let mut entry = TxLogEntry::new(
            logical_id,
            version,
            stream_id << 1,
            location.data_block,
            location.data_offset,
        );
        entry.seal_crc32();
        write_log_entries(region, log_block, &[entry]);
    }

    fn append_publication_record(
        region: &mut VMemoryBlockRegion,
        stream_id: u32,
        stream: StreamCursor,
        block_seq: u32,
        logical_id: u64,
        version: u32,
        fill: u8,
    ) -> (u32, DataRecordLocation) {
        let record = TMemory::encode_publication_data_record(
            logical_id,
            version,
            1,
            0,
            &[fill, fill, fill, fill],
        )
        .unwrap();
        let location = region.append_data_record(stream, &record).unwrap();
        let log_block = region.alloc_log_block(stream_id, block_seq).unwrap();
        (log_block, location)
    }

    fn write_log_entries(
        region: &mut VMemoryBlockRegion,
        start_block: u32,
        entries: &[TxLogEntry],
    ) {
        let mut header = region.log_block_header(start_block).unwrap();
        header.entry_count = u32::try_from(entries.len()).unwrap();
        let block_offset = usize::try_from(start_block).unwrap() * BLOCK_SIZE;
        region.write(block_offset, &header.as_bytes()).unwrap();
        let mut offset = block_offset + size_of::<LogBlockHeader>();
        for entry in entries {
            region.write(offset, &encode_tx_log_entry(*entry)).unwrap();
            offset += size_of::<TxLogEntry>();
        }
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

    fn corrupt_log_entry_data_block(
        region: &mut VMemoryBlockRegion,
        start_block: u32,
        entry_index: usize,
    ) {
        let entry_offset = usize::try_from(start_block).unwrap() * BLOCK_SIZE
            + size_of::<LogBlockHeader>()
            + entry_index * size_of::<TxLogEntry>();
        let field_offset = entry_offset + 16;
        let mut bytes = region.read(field_offset, 4).unwrap();
        bytes[0] ^= 0x01;
        region.write(field_offset, &bytes).unwrap();
    }

    fn rewrite_log_entry(
        region: &mut VMemoryBlockRegion,
        start_block: u32,
        entry_index: usize,
        update: impl FnOnce(&mut TxLogEntry),
    ) {
        let entry_offset = usize::try_from(start_block).unwrap() * BLOCK_SIZE
            + size_of::<LogBlockHeader>()
            + entry_index * size_of::<TxLogEntry>();
        let bytes = region.read(entry_offset, size_of::<TxLogEntry>()).unwrap();
        let mut entry = TxLogEntry {
            logical_id: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            version: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            tx_meta: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
            data_block: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            data_offset: u32::from_le_bytes(bytes[20..24].try_into().unwrap()),
            crc32: u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            entry_meta: u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
        };
        update(&mut entry);
        entry.seal_crc32();
        region
            .write(entry_offset, &encode_tx_log_entry(entry))
            .unwrap();
    }

    fn rewrite_data_record_header(
        region: &mut VMemoryBlockRegion,
        location: DataRecordLocation,
        update: impl FnOnce(&mut TxDataRecordHeader),
    ) {
        let record_offset = usize::try_from(location.data_block).unwrap() * BLOCK_SIZE
            + usize::try_from(location.data_offset).unwrap();
        let bytes = region
            .read(record_offset, size_of::<TxDataRecordHeader>())
            .unwrap();
        let mut header = TxDataRecordHeader::from_bytes(bytes).unwrap();
        update(&mut header);
        region.write(record_offset, &header.as_bytes()).unwrap();
    }
}
