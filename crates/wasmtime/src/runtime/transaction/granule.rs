use crate::prelude::*;
use crate::runtime::store::InstanceId;
use alloc::vec::Vec;
use core::{cmp::Ordering, ops::Range};

use super::{ObjectId, TableElementSnapshot};

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
    TData {
        instance: Option<u32>,
        data_index: u32,
    },
    TElem {
        instance: Option<u32>,
        elem_index: u32,
    },
    Object {
        object_id: ObjectId,
    },
    VolatileObject {
        object_table_domain: u64,
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
            GranuleId::TData { .. } => 5,
            GranuleId::TElem { .. } => 6,
            // Runtime object permissions use one object-domain key. Durable log
            // publication still distinguishes TStruct/TArray record domains.
            GranuleId::Object { .. } => 7,
            GranuleId::VolatileObject { .. } => 8,
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
                GranuleId::TData {
                    instance: lhs_instance,
                    data_index: lhs_data_index,
                },
                GranuleId::TData {
                    instance: rhs_instance,
                    data_index: rhs_data_index,
                },
            ) => (lhs_instance, lhs_data_index).cmp(&(rhs_instance, rhs_data_index)),
            (
                GranuleId::TElem {
                    instance: lhs_instance,
                    elem_index: lhs_elem_index,
                },
                GranuleId::TElem {
                    instance: rhs_instance,
                    elem_index: rhs_elem_index,
                },
            ) => (lhs_instance, lhs_elem_index).cmp(&(rhs_instance, rhs_elem_index)),
            (
                GranuleId::Object {
                    object_id: lhs_object_id,
                },
                GranuleId::Object {
                    object_id: rhs_object_id,
                },
            ) => lhs_object_id.cmp(rhs_object_id),
            (
                GranuleId::VolatileObject {
                    object_table_domain: lhs_domain,
                    object_id: lhs_object_id,
                },
                GranuleId::VolatileObject {
                    object_table_domain: rhs_domain,
                    object_id: rhs_object_id,
                },
            ) => (lhs_domain, lhs_object_id).cmp(&(rhs_domain, rhs_object_id)),
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

/// A validated snapshot of one transactional table granule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TableGranuleSnapshot(Vec<TableElementSnapshot>);

impl TableGranuleSnapshot {
    pub(crate) const ELEMENT_CAPACITY: u64 = TTABLE_GRANULE_SIZE;

    pub(crate) fn new(elements: Vec<TableElementSnapshot>) -> Result<Self> {
        ensure!(
            elements.len() <= usize::try_from(TTABLE_GRANULE_SIZE).unwrap(),
            "table granule snapshot exceeds {} elements",
            TTABLE_GRANULE_SIZE
        );
        Ok(Self(elements))
    }

    pub(crate) fn elements(&self) -> &[TableElementSnapshot] {
        &self.0
    }

    pub(crate) fn into_elements(self) -> Vec<TableElementSnapshot> {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PendingMemoryStore {
    pub(super) instance: InstanceId,
    pub(super) owner_instance_key: Option<InstanceId>,
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

impl TMemoryGranuleSnapshot {
    pub(crate) fn new(
        granule_index: usize,
        range: Range<usize>,
        version: u64,
        bytes: Vec<u8>,
    ) -> Result<Self> {
        ensure!(
            bytes.len() <= TMEMORY_GRANULE_SIZE,
            "tmemory granule snapshot exceeds {} bytes",
            TMEMORY_GRANULE_SIZE
        );
        ensure!(
            range.len() == bytes.len(),
            "tmemory granule snapshot range length mismatch"
        );
        Ok(Self {
            granule_index,
            range,
            version,
            bytes,
        })
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TMemoryAccessSnapshot {
    pub(super) base: u64,
    pub(super) bytes: Vec<u8>,
    pub(super) byte_len: usize,
    pub(super) granules: Vec<TMemoryGranuleSnapshot>,
}

impl TMemoryAccessSnapshot {
    pub(crate) fn new(
        base: u64,
        bytes: Vec<u8>,
        byte_len: usize,
        granules: Vec<TMemoryGranuleSnapshot>,
    ) -> Result<Self> {
        let start =
            usize::try_from(base).context("tmemory snapshot base does not fit host usize")?;
        let end = start
            .checked_add(bytes.len())
            .context("tmemory snapshot range overflow")?;
        ensure!(
            end <= byte_len,
            "tmemory snapshot range exceeds visible memory size"
        );
        Ok(Self {
            base,
            bytes,
            byte_len,
            granules,
        })
    }

    pub(crate) fn into_granules(self) -> Vec<TMemoryGranuleSnapshot> {
        self.granules
    }
}

pub(crate) const TMEMORY_GRANULE_SHIFT: usize = 6;
pub(crate) const TMEMORY_GRANULE_SIZE: usize = 1 << TMEMORY_GRANULE_SHIFT;
pub(super) const TTABLE_GRANULE_SHIFT: u32 = 4;
pub(crate) const TTABLE_GRANULE_SIZE: u64 = 1 << TTABLE_GRANULE_SHIFT;
