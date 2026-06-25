use crate::prelude::*;
use crate::runtime::transaction::{ObjectId, ObjectKind};
use crate::runtime::vm::block_region::MappedRegionSource;
use alloc::sync::Arc;
use core::fmt;
use wasmtime_environ::VMSharedTypeIndex;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistentObjectRecordLocation {
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) data_record_offset: u64,
    pub(crate) record_len: u64,
}

#[derive(Clone)]
pub(crate) struct PersistentObjectRecordSource {
    pub(crate) mapped_source: Arc<dyn MappedRegionSource>,
    pub(crate) location: PersistentObjectRecordLocation,
}

impl fmt::Debug for PersistentObjectRecordSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PersistentObjectRecordSource")
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PersistentObjectDirectoryEntry {
    pub(crate) object_id: ObjectId,
    pub(crate) kind: ObjectKind,
    pub(crate) directory_version: u64,
    pub(crate) record_version: u32,
    pub(crate) type_layout_id: u32,
    pub(crate) runtime_type_index: Option<VMSharedTypeIndex>,
    pub(crate) record_source: Option<PersistentObjectRecordSource>,
}

impl PersistentObjectDirectoryEntry {
    pub(crate) fn ensure_matches_object(self, object_id: ObjectId) -> Result<Self> {
        ensure!(
            self.object_id == object_id,
            "persistent object directory entry object id mismatch"
        );
        Ok(self)
    }
}
