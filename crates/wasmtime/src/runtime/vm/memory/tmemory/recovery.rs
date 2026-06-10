use super::block_region::BlockRegionBackendView;
use super::{DATA_CHUNK_MAGIC, LOG_BLOCK_MAGIC, TxLogEntry};
use crate::prelude::*;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

#[cfg(test)]
use super::block_region::{BLOCK_SIZE, StreamCursor, VMemoryBlockRegion};
#[cfg(test)]
use super::{DataRecordLocation, LogBlockHeader, TMemory};
#[cfg(test)]
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
    pub(crate) next_stream_id: u32,
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

    for stream in streams.iter() {
        for update in replay_stream(region, stream)? {
            match winners.get(&update.logical_id) {
                Some(current) if current.version > update.version => {}
                Some(current) if current.version == update.version => {
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

    Ok(RecoveredRegion {
        next_stream_id: streams.iter().map(|stream| stream.stream_id).max().unwrap_or(0) + 1,
        streams,
        winners: winners.into_values().collect(),
    })
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
) -> Result<Vec<RecoveryWinner>> {
    let mut winners = Vec::new();
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
                pending = None;
            }
            let txn = pending.get_or_insert_with(|| PendingTransaction {
                txid,
                entries: Vec::new(),
            });

            if is_final_lp(entry) {
                txn.entries.push(entry);
                winners.extend(txn.entries.iter().map(|committed| RecoveryWinner {
                    logical_id: committed.logical_id,
                    version: committed.version,
                    data_block: committed.data_block,
                    data_offset: committed.data_offset,
                }));
                pending = None;
            } else {
                txn.entries.push(entry);
            }
        }
    }

    Ok(winners)
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

        let mut ordinary = TxLogEntry::new(
            0x1000,
            1,
            1 << 1,
            first.1.data_block,
            first.1.data_offset,
        );
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

    fn write_log_entries(region: &mut VMemoryBlockRegion, start_block: u32, entries: &[TxLogEntry]) {
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

    fn corrupt_log_entry_data_block(region: &mut VMemoryBlockRegion, start_block: u32, entry_index: usize) {
        let entry_offset = usize::try_from(start_block).unwrap() * BLOCK_SIZE
            + size_of::<LogBlockHeader>()
            + entry_index * size_of::<TxLogEntry>();
        let field_offset = entry_offset + 16;
        let mut bytes = region.read(field_offset, 4).unwrap();
        bytes[0] ^= 0x01;
        region.write(field_offset, &bytes).unwrap();
    }
}
