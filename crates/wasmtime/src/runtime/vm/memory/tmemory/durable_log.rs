//! Durable transactional log and data record codecs for transactional memory.

#![allow(dead_code)]

use crate::prelude::*;

pub(crate) const NO_NEXT_BLOCK: u32 = u32::MAX;
pub(crate) const REGION_MAGIC: u32 = 0x5452_4547;
pub(crate) const LOG_BLOCK_MAGIC: u32 = 0x544c_4f47;
pub(crate) const DATA_CHUNK_MAGIC: u32 = 0x5444_4154;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlockState {
    Free = 0,
    Active = 1,
    Sealed = 2,
    Retired = 3,
}

impl BlockState {
    pub(crate) fn from_byte(byte: u8) -> Result<Self> {
        Ok(match byte {
            0 => BlockState::Free,
            1 => BlockState::Active,
            2 => BlockState::Sealed,
            3 => BlockState::Retired,
            state => bail!("unknown durable block state {state}"),
        })
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlockKind {
    Free = 0,
    Log = 1,
    ObjectData = 2,
    LinearUndo = 3,
    Metadata = 4,
}

impl BlockKind {
    pub(crate) fn from_byte(byte: u8) -> Result<Self> {
        Ok(match byte {
            0 => BlockKind::Free,
            1 => BlockKind::Log,
            2 => BlockKind::ObjectData,
            3 => BlockKind::LinearUndo,
            4 => BlockKind::Metadata,
            kind => bail!("unknown durable block kind {kind}"),
        })
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BlockMeta {
    pub(crate) state: u8,
    pub(crate) kind: u8,
    pub(crate) reserved0: u16,
    pub(crate) generation: u32,
    pub(crate) owner_thread: u32,
    pub(crate) chunk_start: u32,
    pub(crate) chunk_blocks: u32,
    pub(crate) reserved1: u64,
}

impl Default for BlockMeta {
    fn default() -> Self {
        Self::free()
    }
}

impl BlockMeta {
    pub(crate) const BYTE_LEN: usize = 32;

    pub(crate) fn free() -> Self {
        Self {
            state: BlockState::Free as u8,
            kind: BlockKind::Free as u8,
            reserved0: 0,
            generation: 0,
            owner_thread: 0,
            chunk_start: NO_NEXT_BLOCK,
            chunk_blocks: 0,
            reserved1: 0,
        }
    }

    pub(crate) fn active(
        kind: BlockKind,
        generation: u32,
        owner_thread: u32,
        chunk_start: u32,
        chunk_blocks: u32,
    ) -> Self {
        Self {
            state: BlockState::Active as u8,
            kind: kind as u8,
            reserved0: 0,
            generation,
            owner_thread,
            chunk_start,
            chunk_blocks,
            reserved1: 0,
        }
    }

    pub(crate) fn state(&self) -> Result<BlockState> {
        BlockState::from_byte(self.state)
    }

    pub(crate) fn kind(&self) -> Result<BlockKind> {
        BlockKind::from_byte(self.kind)
    }

    pub(crate) fn is_active_or_sealed(&self) -> Result<bool> {
        Ok(matches!(
            self.state()?,
            BlockState::Active | BlockState::Sealed
        ))
    }

    pub(crate) fn as_bytes(&self) -> [u8; Self::BYTE_LEN] {
        let mut bytes = [0u8; Self::BYTE_LEN];
        bytes[0] = self.state;
        bytes[1] = self.kind;
        bytes[2..4].copy_from_slice(&self.reserved0.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.generation.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.owner_thread.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.chunk_start.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.chunk_blocks.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.reserved1.to_le_bytes());
        bytes
    }

    pub(crate) fn from_bytes(bytes: impl AsRef<[u8]>) -> Result<Self> {
        let bytes = bytes.as_ref();
        ensure!(
            bytes.len() == Self::BYTE_LEN,
            "durable block metadata length mismatch"
        );
        let meta = Self {
            state: bytes[0],
            kind: bytes[1],
            reserved0: u16::from_le_bytes(bytes[2..4].try_into().unwrap()),
            generation: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            owner_thread: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            chunk_start: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
            chunk_blocks: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            reserved1: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
        };
        ensure!(
            meta.reserved0 == 0,
            "durable block metadata reserved0 must be zero"
        );
        ensure!(
            bytes[20..24] == [0; 4],
            "durable block metadata padding must be zero"
        );
        ensure!(
            meta.reserved1 == 0,
            "durable block metadata reserved1 must be zero"
        );
        meta.validate()?;
        Ok(meta)
    }

    fn validate(&self) -> Result<()> {
        let state = self.state()?;
        let kind = self.kind()?;

        match state {
            BlockState::Free => {
                ensure!(
                    kind == BlockKind::Free,
                    "durable free block metadata must use free kind"
                );
                ensure!(
                    self.owner_thread == 0,
                    "durable free block metadata must have zero owner thread"
                );
                ensure!(
                    self.chunk_start == NO_NEXT_BLOCK,
                    "durable free block metadata must use NO_NEXT_BLOCK chunk start"
                );
                ensure!(
                    self.chunk_blocks == 0,
                    "durable free block metadata must have zero chunk blocks"
                );
            }
            BlockState::Active | BlockState::Sealed | BlockState::Retired => {
                ensure!(
                    kind != BlockKind::Free,
                    "durable non-free block metadata must not use free kind"
                );
                ensure!(
                    self.chunk_start != NO_NEXT_BLOCK,
                    "durable non-free block metadata must have a chunk start"
                );
                ensure!(
                    self.chunk_blocks > 0,
                    "durable non-free block metadata must have positive chunk blocks"
                );
            }
        }

        Ok(())
    }
}

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

impl RegionHeader {
    const BYTE_LEN: usize = 32;

    pub(crate) fn as_bytes(&self) -> [u8; Self::BYTE_LEN] {
        let mut bytes = [0u8; Self::BYTE_LEN];
        bytes[0..4].copy_from_slice(&self.magic.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.block_size.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.num_blocks.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.block_table_start_block.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.block_table_block_count.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.metadata_descs_start_block.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.metadata_descs_block_count.to_le_bytes());
        bytes[28..32].copy_from_slice(&self.num_descs.to_le_bytes());
        bytes
    }

    pub(crate) fn from_bytes(bytes: impl AsRef<[u8]>) -> Result<Self> {
        let bytes = bytes.as_ref();
        ensure!(
            bytes.len() == Self::BYTE_LEN,
            "durable region header length mismatch"
        );
        Ok(Self {
            magic: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            block_size: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            num_blocks: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            block_table_start_block: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
            block_table_block_count: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            metadata_descs_start_block: u32::from_le_bytes(bytes[20..24].try_into().unwrap()),
            metadata_descs_block_count: u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            num_descs: u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
        })
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LogBlockHeader {
    pub(crate) magic: u32,
    pub(crate) stream_id: u32,
    pub(crate) block_seq: u32,
    pub(crate) next_block: u32,
    pub(crate) entry_count: u32,
}

impl LogBlockHeader {
    const BYTE_LEN: usize = 20;

    pub(crate) fn as_bytes(&self) -> [u8; Self::BYTE_LEN] {
        let mut bytes = [0u8; Self::BYTE_LEN];
        bytes[0..4].copy_from_slice(&self.magic.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.stream_id.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.block_seq.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.next_block.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.entry_count.to_le_bytes());
        bytes
    }

    pub(crate) fn from_bytes(bytes: impl AsRef<[u8]>) -> Result<Self> {
        let bytes = bytes.as_ref();
        ensure!(
            bytes.len() == Self::BYTE_LEN,
            "durable log block header length mismatch"
        );
        Ok(Self {
            magic: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            stream_id: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            block_seq: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            next_block: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
            entry_count: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
        })
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DataChunkHeader {
    pub(crate) magic: u32,
    pub(crate) stream_id: u32,
    pub(crate) chunk_seq: u32,
    pub(crate) next_chunk: u32,
    pub(crate) chunk_blocks: u32,
    pub(crate) tail_block_delta: u32,
    pub(crate) tail_in_block: u32,
}

impl DataChunkHeader {
    const BYTE_LEN: usize = 28;

    pub(crate) fn as_bytes(&self) -> [u8; Self::BYTE_LEN] {
        let mut bytes = [0u8; Self::BYTE_LEN];
        bytes[0..4].copy_from_slice(&self.magic.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.stream_id.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.chunk_seq.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.next_chunk.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.chunk_blocks.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.tail_block_delta.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.tail_in_block.to_le_bytes());
        bytes
    }

    pub(crate) fn from_bytes(bytes: impl AsRef<[u8]>) -> Result<Self> {
        let bytes = bytes.as_ref();
        ensure!(
            bytes.len() == Self::BYTE_LEN,
            "durable data chunk header length mismatch"
        );
        Ok(Self {
            magic: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            stream_id: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            chunk_seq: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            next_chunk: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
            chunk_blocks: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            tail_block_delta: u32::from_le_bytes(bytes[20..24].try_into().unwrap()),
            tail_in_block: u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
        })
    }
}

#[repr(C, align(32))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
// Keep this entry exactly one half cache line. Promotion metadata must live in
// object data records or type-layout records, not in the fixed transaction log
// entry, so two entries can share one 64-byte cache line.
pub(crate) struct TxLogEntry {
    pub(crate) logical_id: u64,
    pub(crate) version: u32,
    pub(crate) tx_meta: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) crc32: u32,
    pub(crate) entry_meta: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TxLogEntryRole {
    TObjectPub,
    TMemoryUndo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TxEntryMeta(u32);

impl TxEntryMeta {
    pub(crate) const ROLE_BITS: u32 = 0b11;
    pub(crate) const GENERATION_SHIFT: u32 = 2;
    pub(crate) const MAX_DATA_BLOCK_GENERATION: u32 = (1 << 30) - 1;

    pub(crate) fn new(role: TxLogEntryRole, data_block_generation: u32) -> Result<Self> {
        ensure!(
            data_block_generation <= Self::MAX_DATA_BLOCK_GENERATION,
            "data block generation exceeds log entry capacity"
        );
        Ok(Self(
            role.to_bits() | (data_block_generation << Self::GENERATION_SHIFT),
        ))
    }

    pub(crate) fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    pub(crate) fn bits(self) -> u32 {
        self.0
    }

    pub(crate) fn role(self) -> Result<TxLogEntryRole> {
        TxLogEntryRole::from_bits(self.0 & Self::ROLE_BITS)
    }

    pub(crate) fn data_block_generation(self) -> u32 {
        self.0 >> Self::GENERATION_SHIFT
    }

    pub(crate) fn with_role(self, role: TxLogEntryRole) -> Self {
        Self((self.0 & !Self::ROLE_BITS) | role.to_bits())
    }
}

impl TxLogEntryRole {
    fn to_bits(self) -> u32 {
        match self {
            TxLogEntryRole::TObjectPub => 0,
            TxLogEntryRole::TMemoryUndo => 1,
        }
    }

    fn from_bits(bits: u32) -> Result<Self> {
        Ok(match bits {
            0 => TxLogEntryRole::TObjectPub,
            1 => TxLogEntryRole::TMemoryUndo,
            role => bail!("unknown transaction log entry role {role}"),
        })
    }
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
            entry_meta: TxEntryMeta::new(TxLogEntryRole::TObjectPub, 0)
                .unwrap()
                .bits(),
        }
    }

    pub(crate) fn seal_crc32(&mut self) {
        self.crc32 = crc32fast::hash(&self.bytes_without_crc32());
    }

    pub(crate) fn validate_crc32(&self) -> bool {
        self.crc32 == crc32fast::hash(&self.bytes_without_crc32())
    }

    pub(crate) fn role(&self) -> Result<TxLogEntryRole> {
        TxEntryMeta::from_bits(self.entry_meta).role()
    }

    pub(crate) fn set_role(&mut self, role: TxLogEntryRole) {
        self.entry_meta = TxEntryMeta::from_bits(self.entry_meta)
            .with_role(role)
            .bits();
    }

    pub(crate) fn data_block_generation(&self) -> u32 {
        TxEntryMeta::from_bits(self.entry_meta).data_block_generation()
    }

    pub(crate) fn set_data_block_generation(&mut self, generation: u32) -> Result<()> {
        let role = self.role()?;
        self.entry_meta = TxEntryMeta::new(role, generation)?.bits();
        Ok(())
    }

    fn bytes_without_crc32(&self) -> [u8; 28] {
        let mut bytes = [0u8; 28];
        bytes[0..8].copy_from_slice(&self.logical_id.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.version.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.tx_meta.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.data_block.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.data_offset.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.entry_meta.to_le_bytes());
        bytes
    }
}

#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PackedGranuleDomain {
    TMemory = 1,
    TMemorySize = 2,
    TGlobal = 3,
    TTable = 4,
    TTableSize = 5,
    TStruct = 6,
    TArray = 7,
}

pub(crate) fn packed_granule_domain(logical_id: u64) -> Result<PackedGranuleDomain> {
    let domain_bits = u16::try_from(logical_id >> 60).unwrap();
    Ok(match domain_bits {
        1 => PackedGranuleDomain::TMemory,
        2 => PackedGranuleDomain::TMemorySize,
        3 => PackedGranuleDomain::TGlobal,
        4 => PackedGranuleDomain::TTable,
        5 => PackedGranuleDomain::TTableSize,
        6 => PackedGranuleDomain::TStruct,
        7 => PackedGranuleDomain::TArray,
        _ => bail!("unknown packed granule domain {domain_bits}"),
    })
}

pub(crate) fn pack_object_granule_id(domain: PackedGranuleDomain, object_id: u64) -> Result<u64> {
    match domain {
        PackedGranuleDomain::TStruct | PackedGranuleDomain::TArray => {
            ensure!(
                object_id < (1u64 << 60),
                "object id does not fit in packed granule id payload"
            );
            Ok(((domain as u64) << 60) | object_id)
        }
        _ => bail!("domain {domain:?} is not an object granule"),
    }
}

pub(crate) fn pack_tmemory_granule_id(
    instance: Option<u32>,
    memory_index: u32,
    granule_index: u64,
) -> Result<u64> {
    let instance_code = match instance {
        Some(instance) => u64::from(instance)
            .checked_add(1)
            .context("tmemory instance id overflow")?,
        None => 0,
    };
    ensure!(
        instance_code < (1u64 << 20),
        "tmemory instance id does not fit in packed granule id payload"
    );
    ensure!(
        memory_index < (1u32 << 12),
        "tmemory memory index does not fit in packed granule id payload"
    );
    ensure!(
        granule_index < (1u64 << 28),
        "tmemory granule index does not fit in packed granule id payload"
    );

    let payload = (instance_code << 40) | (u64::from(memory_index) << 28) | granule_index;
    Ok(((PackedGranuleDomain::TMemory as u64) << 60) | payload)
}

pub(crate) fn unpack_tmemory_granule_id(logical_id: u64) -> Result<(Option<u32>, u32, u64)> {
    let domain = packed_granule_domain(logical_id)?;
    ensure!(
        domain == PackedGranuleDomain::TMemory,
        "logical id {logical_id:#x} is not a tmemory granule"
    );
    let payload = logical_id & ((1u64 << 60) - 1);
    let instance_code = (payload >> 40) & ((1u64 << 20) - 1);
    let memory_index = u32::try_from((payload >> 28) & ((1u64 << 12) - 1)).unwrap();
    let granule_index = payload & ((1u64 << 28) - 1);
    let instance = if instance_code == 0 {
        None
    } else {
        Some(u32::try_from(instance_code - 1).unwrap())
    };
    Ok((instance, memory_index, granule_index))
}

pub(crate) fn unpack_object_granule_id(logical_id: u64) -> Result<(PackedGranuleDomain, u64)> {
    let object_id = logical_id & ((1u64 << 60) - 1);
    let domain = packed_granule_domain(logical_id)?;
    ensure!(
        matches!(
            domain,
            PackedGranuleDomain::TStruct | PackedGranuleDomain::TArray
        ),
        "logical id {logical_id:#x} is not an object granule"
    );
    Ok((domain, object_id))
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TxDataRecordHeader {
    pub(crate) logical_id: u64,
    pub(crate) version: u32,
    pub(crate) kind: u16,
    pub(crate) role: u16,
    pub(crate) payload_len: u32,
    pub(crate) type_info: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TxDataRecordRole {
    TObjectPub,
    TMemoryUndo,
}

impl TxDataRecordHeader {
    const BYTE_LEN: usize = 24;

    pub(crate) fn as_bytes(&self) -> [u8; Self::BYTE_LEN] {
        let logical_id = self.logical_id;
        let version = self.version;
        let kind = self.kind;
        let role = self.role;
        let payload_len = self.payload_len;
        let type_info = self.type_info;

        let mut bytes = [0u8; Self::BYTE_LEN];
        bytes[0..8].copy_from_slice(&logical_id.to_le_bytes());
        bytes[8..12].copy_from_slice(&version.to_le_bytes());
        bytes[12..14].copy_from_slice(&kind.to_le_bytes());
        bytes[14..16].copy_from_slice(&role.to_le_bytes());
        bytes[16..20].copy_from_slice(&payload_len.to_le_bytes());
        bytes[20..24].copy_from_slice(&type_info.to_le_bytes());
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
            role: u16::from_le_bytes(bytes[14..16].try_into().unwrap()),
            payload_len: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            type_info: u32::from_le_bytes(bytes[20..24].try_into().unwrap()),
        })
    }

    pub(crate) fn role(&self) -> Result<TxDataRecordRole> {
        Ok(match self.role {
            0 => TxDataRecordRole::TObjectPub,
            1 => TxDataRecordRole::TMemoryUndo,
            role => bail!("unknown transaction data record role {role}"),
        })
    }

    pub(crate) fn set_role(&mut self, role: TxDataRecordRole) {
        self.role = match role {
            TxDataRecordRole::TObjectPub => 0,
            TxDataRecordRole::TMemoryUndo => 1,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_granule_id_roundtrips_struct_object_id() {
        let logical_id = pack_object_granule_id(PackedGranuleDomain::TStruct, 41).unwrap();
        let (domain, object_id) = unpack_object_granule_id(logical_id).unwrap();
        assert_eq!(domain, PackedGranuleDomain::TStruct);
        assert_eq!(object_id, 41);
    }

    #[test]
    fn packed_granule_id_roundtrips_array_object_id() {
        let logical_id = pack_object_granule_id(PackedGranuleDomain::TArray, 99).unwrap();
        let (domain, object_id) = unpack_object_granule_id(logical_id).unwrap();
        assert_eq!(domain, PackedGranuleDomain::TArray);
        assert_eq!(object_id, 99);
    }

    #[test]
    fn pack_tmemory_granule_id_encodes_domain_and_fields() {
        let logical_id = pack_tmemory_granule_id(Some(41), 7, 9).unwrap();

        assert_eq!(
            packed_granule_domain(logical_id).unwrap(),
            PackedGranuleDomain::TMemory
        );
        assert_eq!((logical_id >> 40) & ((1 << 20) - 1), 42);
        assert_eq!((logical_id >> 28) & ((1 << 12) - 1), 7);
        assert_eq!(logical_id & ((1 << 28) - 1), 9);
    }

    #[test]
    fn pack_tmemory_granule_id_rejects_out_of_range_fields() {
        assert!(pack_tmemory_granule_id(Some((1 << 20) - 1), 0, 0).is_err());
        assert!(pack_tmemory_granule_id(None, 1 << 12, 0).is_err());
        assert!(pack_tmemory_granule_id(None, 0, 1 << 28).is_err());
    }

    #[test]
    fn tmemory_undo_log_entry_role_roundtrips() {
        let mut entry = TxLogEntry::new(0x1000_0000_0000_0003, 7, 11 << 1, 4, 32);
        assert_eq!(entry.role().unwrap(), TxLogEntryRole::TObjectPub);

        entry.set_role(TxLogEntryRole::TMemoryUndo);
        entry.seal_crc32();

        assert_eq!(entry.role().unwrap(), TxLogEntryRole::TMemoryUndo);
        assert!(entry.validate_crc32());
    }

    #[test]
    fn tx_log_entry_meta_roundtrips_role_and_generation() {
        let mut entry = TxLogEntry::new(0x1000_0000_0000_0003, 7, 11 << 1, 4, 32);

        assert_eq!(entry.role().unwrap(), TxLogEntryRole::TObjectPub);
        assert_eq!(entry.data_block_generation(), 0);

        entry.set_role(TxLogEntryRole::TMemoryUndo);
        entry.set_data_block_generation(17).unwrap();
        entry.seal_crc32();

        assert_eq!(entry.role().unwrap(), TxLogEntryRole::TMemoryUndo);
        assert_eq!(entry.data_block_generation(), 17);
        assert!(entry.validate_crc32());
    }

    #[test]
    fn tx_log_entry_meta_rejects_generation_overflow() {
        let mut entry = TxLogEntry::new(0x1000_0000_0000_0003, 7, 11 << 1, 4, 32);

        let err = entry
            .set_data_block_generation(TxEntryMeta::MAX_DATA_BLOCK_GENERATION + 1)
            .unwrap_err()
            .to_string();

        assert!(err.contains("data block generation exceeds log entry capacity"));
    }

    #[test]
    fn block_meta_roundtrips_active_object_data_chunk() {
        let meta = BlockMeta {
            state: BlockState::Active as u8,
            kind: BlockKind::ObjectData as u8,
            reserved0: 0,
            generation: 9,
            owner_thread: 7,
            chunk_start: 11,
            chunk_blocks: 3,
            reserved1: 0,
        };

        let decoded = BlockMeta::from_bytes(&meta.as_bytes()).unwrap();

        assert_eq!(decoded.state().unwrap(), BlockState::Active);
        assert_eq!(decoded.kind().unwrap(), BlockKind::ObjectData);
        assert_eq!(decoded.generation, 9);
        assert_eq!(decoded.owner_thread, 7);
        assert_eq!(decoded.chunk_start, 11);
        assert_eq!(decoded.chunk_blocks, 3);
    }

    #[test]
    fn block_meta_rejects_unknown_state() {
        let mut meta = BlockMeta::free();
        meta.state = 0xff;

        let err = BlockMeta::from_bytes(&meta.as_bytes())
            .unwrap_err()
            .to_string();

        assert!(err.contains("unknown durable block state"));
    }

    #[test]
    fn block_meta_rejects_nonzero_padding_bytes() {
        let mut bytes = BlockMeta::free().as_bytes();
        bytes[20] = 1;

        let err = BlockMeta::from_bytes(bytes).unwrap_err().to_string();

        assert!(err.contains("durable block metadata padding must be zero"));
    }

    #[test]
    fn block_meta_rejects_active_with_free_kind() {
        let meta = BlockMeta {
            state: BlockState::Active as u8,
            kind: BlockKind::Free as u8,
            reserved0: 0,
            generation: 9,
            owner_thread: 7,
            chunk_start: 11,
            chunk_blocks: 3,
            reserved1: 0,
        };

        let err = BlockMeta::from_bytes(meta.as_bytes())
            .unwrap_err()
            .to_string();

        assert!(err.contains("durable non-free block metadata must not use free kind"));
    }

    #[test]
    fn block_meta_rejects_free_with_chunk_blocks() {
        let mut meta = BlockMeta::free();
        meta.chunk_blocks = 1;

        let err = BlockMeta::from_bytes(meta.as_bytes())
            .unwrap_err()
            .to_string();

        assert!(err.contains("durable free block metadata must have zero chunk blocks"));
    }

    #[test]
    fn tx_log_entry_fits_half_cache_line() {
        assert_eq!(core::mem::size_of::<TxLogEntry>(), 32);
        assert_eq!(core::mem::align_of::<TxLogEntry>(), 32);
    }

    #[test]
    fn tx_data_record_role_defaults_to_tobject_pub() {
        let header = TxDataRecordHeader {
            logical_id: 0x1000_0000_0000_0007,
            version: 3,
            kind: PackedGranuleDomain::TMemory as u16,
            role: 0,
            payload_len: 4,
            type_info: 0,
        };

        assert_eq!(header.role().unwrap(), TxDataRecordRole::TObjectPub);
    }
}
