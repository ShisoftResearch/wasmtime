//! Wizard-style block/chunk region layout for transactional storage.

#![allow(dead_code)]

use core::mem::size_of;

pub(super) const BLOCK_SIZE: usize = 512 * 1024;
pub(super) const IMMIX_LINE_SIZE: usize = 256;

pub(super) const PW_REGION_HEADER_SIZE: usize = size_of::<PWRegionHeader>();
pub(super) const META_DATA_DESC_SIZE: usize = size_of::<MetaDataDesc>();
pub(super) const BLOCK_ENTRY_SIZE: usize = size_of::<BlockEntry>();
pub(super) const CHUNK_HEADER_SIZE: usize = size_of::<ChunkHeader>();
pub(super) const LINE_MARK_SIZE: usize = size_of::<LineMark>();

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct PWRegionHeader {
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
pub(super) struct MetaDataDesc {
    pub kind: u64,
    pub fixed_bytes: u32,
    pub unit_size: u32,
    pub bytes_per_unit: u32,
    padding: u32,
    pub offset: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct BlockEntry {
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
pub(super) struct ChunkHeader {
    pub offset: u64,
    pub limit: u64,
    pub chunk_limit: u64,
    pub linemarks: u64,
}

#[repr(i16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ListKind {
    Metadata = -1,
    None = -2,
    Used = -3,
    SmallFree = 0,
    LargeFree = 1,
}

pub(super) struct BlockLists;

impl BlockLists {
    pub(super) const MAX_LISTS: usize = 8;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct LineMark {
    pub mark: u8,
}

pub(super) const IMMIX_LINE_MARK_META_KIND: u64 = 0x0000_0001_0000_0000;
pub(super) const IMMIX_LINE_MARK_RESET_VALUE: u8 = 0;
pub(super) const LINE_FREE: u8 = 0;
pub(super) const LINE_MARKED: u8 = 1;
