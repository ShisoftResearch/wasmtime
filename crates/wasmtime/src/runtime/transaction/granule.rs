use crate::runtime::store::InstanceId;
use alloc::vec::Vec;
use core::{cmp::Ordering, ops::Range};

use super::ObjectId;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum GranuleId {
    TMemory {
        instance: Option<u32>,
        memory_index: u32,
        granule_index: u64,
    },
    TMemorySize {
        instance: Option<u32>,
        memory_index: u32,
    },
    TGlobal {
        instance: Option<u32>,
        global_index: u32,
    },
    TTable {
        instance: Option<u32>,
        table_index: u32,
        granule_index: u64,
    },
    TTableSize {
        instance: Option<u32>,
        table_index: u32,
    },
    Object {
        object_id: ObjectId,
    },
}

impl GranuleId {
    fn domain_order(self) -> u8 {
        match self {
            GranuleId::TMemory { .. } => 0,
            GranuleId::TMemorySize { .. } => 1,
            GranuleId::TGlobal { .. } => 2,
            GranuleId::TTable { .. } => 3,
            GranuleId::TTableSize { .. } => 4,
            // Runtime object permissions use one object-domain key. Durable log
            // publication still distinguishes TStruct/TArray record domains.
            GranuleId::Object { .. } => 5,
        }
    }
}

impl Ord for GranuleId {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.domain_order().cmp(&other.domain_order()) {
            Ordering::Equal => {}
            order => return order,
        }

        match (self, other) {
            (
                GranuleId::TMemory {
                    instance: lhs_instance,
                    memory_index: lhs_memory_index,
                    granule_index: lhs_granule_index,
                },
                GranuleId::TMemory {
                    instance: rhs_instance,
                    memory_index: rhs_memory_index,
                    granule_index: rhs_granule_index,
                },
            ) => (lhs_instance, lhs_memory_index, lhs_granule_index).cmp(&(
                rhs_instance,
                rhs_memory_index,
                rhs_granule_index,
            )),
            (
                GranuleId::TMemorySize {
                    instance: lhs_instance,
                    memory_index: lhs_memory_index,
                },
                GranuleId::TMemorySize {
                    instance: rhs_instance,
                    memory_index: rhs_memory_index,
                },
            ) => (lhs_instance, lhs_memory_index).cmp(&(rhs_instance, rhs_memory_index)),
            (
                GranuleId::TGlobal {
                    instance: lhs_instance,
                    global_index: lhs_global_index,
                },
                GranuleId::TGlobal {
                    instance: rhs_instance,
                    global_index: rhs_global_index,
                },
            ) => (lhs_instance, lhs_global_index).cmp(&(rhs_instance, rhs_global_index)),
            (
                GranuleId::TTable {
                    instance: lhs_instance,
                    table_index: lhs_table_index,
                    granule_index: lhs_granule_index,
                },
                GranuleId::TTable {
                    instance: rhs_instance,
                    table_index: rhs_table_index,
                    granule_index: rhs_granule_index,
                },
            ) => (lhs_instance, lhs_table_index, lhs_granule_index).cmp(&(
                rhs_instance,
                rhs_table_index,
                rhs_granule_index,
            )),
            (
                GranuleId::TTableSize {
                    instance: lhs_instance,
                    table_index: lhs_table_index,
                },
                GranuleId::TTableSize {
                    instance: rhs_instance,
                    table_index: rhs_table_index,
                },
            ) => (lhs_instance, lhs_table_index).cmp(&(rhs_instance, rhs_table_index)),
            (
                GranuleId::Object {
                    object_id: lhs_object_id,
                },
                GranuleId::Object {
                    object_id: rhs_object_id,
                },
            ) => lhs_object_id.cmp(rhs_object_id),
            _ => unreachable!("granule domain order must be unique for each variant"),
        }
    }
}

impl PartialOrd for GranuleId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct TableElementKey {
    pub(super) instance: Option<u32>,
    pub(super) table_index: u32,
    pub(super) element_index: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PendingMemoryStore {
    pub(super) instance: InstanceId,
    pub(super) memory_index: u32,
    pub(super) addr: u64,
    pub(super) len: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct TMemoryGranuleSnapshot {
    pub(super) granule_index: usize,
    pub(super) range: Range<usize>,
    pub(super) version: u64,
    pub(super) bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub(crate) struct TMemoryAccessSnapshot {
    pub(super) base: u64,
    pub(super) bytes: Vec<u8>,
    pub(super) byte_len: usize,
    pub(super) granules: Vec<TMemoryGranuleSnapshot>,
}

pub(crate) const TMEMORY_GRANULE_SHIFT: usize = 6;
pub(crate) const TMEMORY_GRANULE_SIZE: usize = 1 << TMEMORY_GRANULE_SHIFT;
pub(super) const TTABLE_GRANULE_SHIFT: u32 = 4;
pub(crate) const TTABLE_GRANULE_SIZE: u64 = 1 << TTABLE_GRANULE_SHIFT;
