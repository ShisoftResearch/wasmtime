use super::block_region::BlockRegionBackendView;
use super::{
    DATA_CHUNK_MAGIC, LOG_BLOCK_MAGIC, PackedGranuleDomain, TxDataRecordHeader, TxDataRecordRole,
    TxLogEntry, TxLogEntryRole, packed_granule_domain, unpack_object_granule_id,
};
use crate::prelude::*;
use crate::runtime::transaction::TxObjectHeader;
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
    pub(crate) root_object_ids: Vec<u64>,
    pub(crate) tmemory_undo_rollbacks: Vec<RecoveredTMemoryUndoRollback>,
    pub(crate) next_stream_id: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveredObjectWinner {
    pub(crate) object_id: u64,
    pub(crate) version: u32,
    pub(crate) kind: u16,
    pub(crate) type_index: u32,
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

#[derive(Debug, Default)]
struct PendingTransaction {
    txid: u32,
    entries: Vec<TxLogEntry>,
}

pub(crate) fn recover_region(region: &BlockRegionBackendView<'_>) -> Result<RecoveredRegion> {
    let streams = discover_streams(region)?;
    let mut winners = BTreeMap::<u64, RecoveryWinner>::new();
    let mut tmemory_undo_rollbacks = Vec::new();

    for stream in streams.iter() {
        let replay = replay_stream(region, stream)?;
        tmemory_undo_rollbacks.extend(replay.tmemory_undo_rollbacks);
        for update in replay.winners {
            match winners.get(&update.logical_id) {
                Some(current) if current.version > update.version => {}
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
    let object_winners = replay_object_winners(region, &winners)?;
    let root_object_ids = replay_root_object_ids(region, &winners)?;

    Ok(RecoveredRegion {
        next_stream_id: streams
            .iter()
            .map(|stream| stream.stream_id)
            .max()
            .unwrap_or(0)
            + 1,
        streams,
        winners,
        object_winners,
        root_object_ids,
        tmemory_undo_rollbacks,
    })
}

impl RecoveredRegion {
    pub(crate) fn committed_object_winners(&self) -> Result<Vec<RecoveredObjectWinner>> {
        Ok(self.object_winners.clone())
    }
}

fn replay_root_object_ids(
    region: &BlockRegionBackendView<'_>,
    winners: &[RecoveryWinner],
) -> Result<Vec<u64>> {
    let mut roots = Vec::new();

    for winner in winners {
        let Ok(domain) = packed_granule_domain(winner.logical_id) else {
            continue;
        };
        match domain {
            PackedGranuleDomain::TGlobal => {
                let payload = load_root_publication_payload(region, winner, domain)?;
                roots.extend(decode_root_object_refs(&payload)?.into_iter().take(1));
            }
            PackedGranuleDomain::TTable => {
                let payload = load_root_publication_payload(region, winner, domain)?;
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
    winner: &RecoveryWinner,
    expected_domain: PackedGranuleDomain,
) -> Result<Vec<u8>> {
    let (data_header, payload) =
        load_publication_payload(region, winner.data_block, winner.data_offset)?;
    ensure!(
        data_header.logical_id == winner.logical_id,
        "recovered root publication logical id does not match log winner"
    );
    ensure!(
        data_header.version == winner.version,
        "recovered root publication version does not match log winner"
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
    winners: &[RecoveryWinner],
) -> Result<Vec<RecoveredObjectWinner>> {
    let mut object_winners = BTreeMap::<u64, RecoveredObjectWinner>::new();

    for winner in winners {
        let Ok((domain, object_id)) = unpack_object_granule_id(winner.logical_id) else {
            continue;
        };
        let (data_header, record_bytes) =
            load_publication_payload(region, winner.data_block, winner.data_offset)?;
        ensure!(
            data_header.logical_id == winner.logical_id,
            "recovered object publication logical id does not match log winner"
        );
        ensure!(
            data_header.version == winner.version,
            "recovered object publication version does not match log winner"
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
        let candidate = RecoveredObjectWinner {
            object_id,
            version: winner.version,
            kind: object_header.kind,
            type_index: object_header.type_index,
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

fn load_publication_payload(
    region: &BlockRegionBackendView<'_>,
    data_block: u32,
    data_offset: u32,
) -> Result<(TxDataRecordHeader, Vec<u8>)> {
    let block = usize::try_from(data_block).context("publication data block overflow")?;
    let in_block = usize::try_from(data_offset).context("publication data offset overflow")?;
    let record_offset = block
        .checked_mul(region.block_size())
        .and_then(|offset| offset.checked_add(in_block))
        .context("publication data record offset overflow")?;
    let header = TxDataRecordHeader::from_bytes(
        region.read(record_offset, size_of::<TxDataRecordHeader>())?,
    )?;
    let payload_len =
        usize::try_from(header.payload_len).context("publication payload length overflow")?;
    let payload = region.read(record_offset + size_of::<TxDataRecordHeader>(), payload_len)?;
    Ok((header, payload))
}

fn discover_streams(region: &BlockRegionBackendView<'_>) -> Result<Vec<RecoveredStream>> {
    let mut streams = BTreeMap::<u32, DiscoveredStreamParts>::new();
    let mut block = 0usize;

    while block < region.num_blocks() {
        let start_block = u32::try_from(block).context("transactional recovery block overflow")?;
        match region.block_magic(start_block)? {
            LOG_BLOCK_MAGIC => {
                let header = region.log_block_header(start_block)?;
                streams
                    .entry(header.stream_id)
                    .or_default()
                    .log_blocks
                    .push((header.block_seq, start_block));
                block += 1;
            }
            DATA_CHUNK_MAGIC => {
                let header = region.data_chunk_header(start_block)?;
                let chunk_blocks = usize::try_from(header.chunk_blocks)
                    .context("transactional data chunk block count overflow")?;
                ensure!(
                    chunk_blocks > 0,
                    "transactional data chunk at block {start_block} has zero blocks"
                );
                ensure!(
                    block + chunk_blocks <= region.num_blocks(),
                    "transactional data chunk at block {start_block} exceeds region"
                );
                streams
                    .entry(header.stream_id)
                    .or_default()
                    .data_chunks
                    .push((header.chunk_seq, start_block));
                block += chunk_blocks;
            }
            _ => {
                block += 1;
            }
        }
    }

    Ok(streams
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
        .collect())
}

fn replay_stream(
    region: &BlockRegionBackendView<'_>,
    stream: &RecoveredStream,
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
                        .extend(loose_end_tmemory_undo_rollbacks(region, txn.entries)?);
                }
            }
            let txn = pending.get_or_insert_with(|| PendingTransaction {
                txid,
                entries: Vec::new(),
            });

            if is_final_lp(entry) {
                txn.entries.push(entry);
                for committed in &txn.entries {
                    if committed.role()? != TxLogEntryRole::TObjectPub {
                        continue;
                    }
                    replay.winners.push(RecoveryWinner {
                        logical_id: committed.logical_id,
                        version: committed.version,
                        data_block: committed.data_block,
                        data_offset: committed.data_offset,
                    });
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
            .extend(loose_end_tmemory_undo_rollbacks(region, txn.entries)?);
    }

    Ok(replay)
}

fn loose_end_tmemory_undo_rollbacks(
    region: &BlockRegionBackendView<'_>,
    entries: Vec<TxLogEntry>,
) -> Result<Vec<RecoveredTMemoryUndoRollback>> {
    let mut rollbacks = Vec::new();
    for entry in entries {
        if entry.role()? != TxLogEntryRole::TMemoryUndo {
            continue;
        }
        let (data_header, old_granule_bytes) =
            load_publication_payload(region, entry.data_block, entry.data_offset)?;
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
    recover_region(&region.view())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::transaction::{ObjectPayload, ObjectValue, encode_object_record_for_test};
    use crate::runtime::vm::{PackedGranuleDomain, pack_object_granule_id};

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
    fn recovery_ignores_tmemory_undo_for_committed_transaction() {
        let region = sample_region_with_committed_tmemory_undo();
        let recovered = recover_region_for_test(&region).unwrap();

        assert_eq!(recovered.winners.len(), 0);
        assert_eq!(recovered.tmemory_undo_rollbacks.len(), 0);
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
            true,
        );
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
    ) {
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
    }

    fn encode_root_object_refs(object_ids: &[Option<u64>]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for object_id in object_ids {
            let raw = object_id.map(|id| id + 1).unwrap_or(0);
            bytes.extend_from_slice(&raw.to_le_bytes());
        }
        bytes
    }

    fn pack_test_granule_id(domain: PackedGranuleDomain, payload: u64) -> u64 {
        ((domain as u64) << 60) | payload
    }

    fn append_committed_object_update(
        region: &mut VMemoryBlockRegion,
        stream_id: u32,
        stream: StreamCursor,
        block_seq: u32,
        logical_id: u64,
        version: u32,
        type_index: u32,
        payload: ObjectPayload,
    ) {
        let (domain, object_id) = unpack_object_granule_id(logical_id).unwrap();
        let object_record =
            encode_object_record_for_test(object_id, version, type_index, &payload).unwrap();
        let publication = TMemory::encode_publication_data_record(
            logical_id,
            version,
            domain as u16,
            type_index,
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
            true,
        );
        write_log_entries(region, log_block, &[entry]);
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
            true,
        );
        write_log_entries(region, log_block, &[entry]);
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
        bytes[28..32].copy_from_slice(&entry.reserved.to_le_bytes());
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
}
