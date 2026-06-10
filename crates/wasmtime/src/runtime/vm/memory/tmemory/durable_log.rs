//! Durable transactional log and data record codecs for transactional memory.

#![allow(dead_code)]

use crate::prelude::*;

pub(crate) const NO_NEXT_BLOCK: u32 = u32::MAX;
pub(crate) const REGION_MAGIC: u32 = 0x5452_4547;
pub(crate) const LOG_BLOCK_MAGIC: u32 = 0x544c_4f47;
pub(crate) const DATA_CHUNK_MAGIC: u32 = 0x5444_4154;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RegionHeader {
    pub(crate) magic: u32,
    pub(crate) block_size: u32,
    pub(crate) num_blocks: u32,
    pub(crate) block_table_start_block: u32,
    pub(crate) block_table_block_count: u32,
    pub(crate) metadata_descs_start_block: u32,
    pub(crate) metadata_descs_block_count: u32,
    pub(crate) num_descs: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LogBlockHeader {
    pub(crate) magic: u32,
    pub(crate) next_block: u32,
    pub(crate) entry_count: u32,
    pub(crate) entries_offset: u32,
    pub(crate) sealed_tx: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DataChunkHeader {
    pub(crate) magic: u32,
    pub(crate) next_block: u32,
    pub(crate) payload_offset: u32,
    pub(crate) payload_len: u32,
    pub(crate) record_count: u32,
    pub(crate) tx_ordinal: u32,
    pub(crate) reserved: u32,
}

#[repr(C, align(32))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TxLogEntry {
    pub(crate) logical_id: u64,
    pub(crate) version: u32,
    pub(crate) tx_meta: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) crc32: u32,
    pub(crate) reserved: u32,
}

impl TxLogEntry {
    pub(crate) fn new(
        logical_id: u64,
        version: u32,
        tx_meta: u32,
        data_block: u32,
        data_offset: u32,
    ) -> Self {
        Self {
            logical_id,
            version,
            tx_meta,
            data_block,
            data_offset,
            crc32: 0,
            reserved: 0,
        }
    }

    pub(crate) fn seal_crc32(&mut self) {
        self.crc32 = crc32fast::hash(&self.bytes_without_crc32());
    }

    pub(crate) fn validate_crc32(&self) -> bool {
        self.crc32 == crc32fast::hash(&self.bytes_without_crc32())
    }

    fn bytes_without_crc32(&self) -> [u8; 28] {
        let mut bytes = [0u8; 28];
        bytes[0..8].copy_from_slice(&self.logical_id.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.version.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.tx_meta.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.data_block.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.data_offset.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.reserved.to_le_bytes());
        bytes
    }
}

#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PackedGranuleDomain {
    TMemory = 1,
}

#[repr(C, packed(4))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TxDataRecordHeader {
    pub(crate) logical_id: u64,
    pub(crate) version: u32,
    pub(crate) kind: u16,
    pub(crate) reserved: u16,
    pub(crate) payload_len: u16,
    pub(crate) type_info: u16,
}

impl TxDataRecordHeader {
    const BYTE_LEN: usize = 20;

    pub(crate) fn as_bytes(&self) -> [u8; Self::BYTE_LEN] {
        let logical_id = self.logical_id;
        let version = self.version;
        let kind = self.kind;
        let reserved = self.reserved;
        let payload_len = self.payload_len;
        let type_info = self.type_info;

        let mut bytes = [0u8; Self::BYTE_LEN];
        bytes[0..8].copy_from_slice(&logical_id.to_le_bytes());
        bytes[8..12].copy_from_slice(&version.to_le_bytes());
        bytes[12..14].copy_from_slice(&kind.to_le_bytes());
        bytes[14..16].copy_from_slice(&reserved.to_le_bytes());
        bytes[16..18].copy_from_slice(&payload_len.to_le_bytes());
        bytes[18..20].copy_from_slice(&type_info.to_le_bytes());
        bytes
    }

    pub(crate) fn from_bytes(bytes: impl AsRef<[u8]>) -> Result<Self> {
        let bytes = bytes.as_ref();
        ensure!(
            bytes.len() == Self::BYTE_LEN,
            "durable tx data record header length mismatch"
        );

        Ok(Self {
            logical_id: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            version: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            kind: u16::from_le_bytes(bytes[12..14].try_into().unwrap()),
            reserved: u16::from_le_bytes(bytes[14..16].try_into().unwrap()),
            payload_len: u16::from_le_bytes(bytes[16..18].try_into().unwrap()),
            type_info: u16::from_le_bytes(bytes[18..20].try_into().unwrap()),
        })
    }
}
