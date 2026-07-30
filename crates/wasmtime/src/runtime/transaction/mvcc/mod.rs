use super::visibility::{CommitTimestamp, TransactionVisibility};
use super::{GlobalSnapshot, GranuleId, ObjectId, ObjectPayload, TableGranuleSnapshot};
use crate::prelude::*;
use alloc::sync::Arc;

mod coordinator;
mod domains;
mod version_chain;

#[allow(
    unused_imports,
    reason = "reserved for the next MVCC implementation stages"
)]
pub(crate) use coordinator::{
    MvccCoordinator, MvccGcBarrierPermit, PendingCommitRegistration, SnapshotRegistration,
};
#[allow(
    unused_imports,
    reason = "reserved for the next MVCC implementation stages"
)]
pub(crate) use domains::{
    MvccPrepareGuard, MvccRuntime, PreparedDomainValues, PreparedObjectValue, PreparedValue,
};
#[allow(
    unused_imports,
    reason = "reserved for the next MVCC implementation stages"
)]
pub(crate) use version_chain::{CommitRecord, CommitState, Version, VersionChain};

/// Feature-selected multiversion visibility facade.
#[derive(Clone, Debug)]
pub(crate) struct MvccVisibility(Arc<MvccRuntime>);

impl MvccVisibility {
    pub(crate) fn new(runtime: Arc<MvccRuntime>) -> Self {
        Self(runtime)
    }

    pub(crate) fn runtime(&self) -> &Arc<MvccRuntime> {
        &self.0
    }

    pub(crate) const fn is_multiversion(&self) -> bool {
        true
    }
}

impl Default for MvccVisibility {
    fn default() -> Self {
        Self(Arc::new(MvccRuntime::default()))
    }
}

impl TransactionVisibility for MvccVisibility {
    type Snapshot = SnapshotRegistration;

    fn begin_snapshot(&self) -> Result<Self::Snapshot> {
        self.0.coordinator.begin_snapshot()
    }

    fn snapshot_timestamp(snapshot: &Self::Snapshot) -> CommitTimestamp {
        snapshot.timestamp()
    }

    fn finish_snapshot(&self, snapshot: Self::Snapshot) -> Result<()> {
        snapshot.finish()
    }

    fn read_memory<F>(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: F,
    ) -> Result<Vec<u8>>
    where
        F: FnOnce() -> Result<Vec<u8>>,
    {
        self.0.read_memory(snapshot, granule, current)
    }

    fn read_memory_size<F>(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: F,
    ) -> Result<u64>
    where
        F: FnOnce() -> Result<u64>,
    {
        self.0.read_memory_size(snapshot, granule, current)
    }

    fn read_global<F>(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: F,
    ) -> Result<GlobalSnapshot>
    where
        F: FnOnce() -> Result<GlobalSnapshot>,
    {
        self.0.read_global(snapshot, granule, current)
    }

    fn read_table<F>(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: F,
    ) -> Result<TableGranuleSnapshot>
    where
        F: FnOnce() -> Result<TableGranuleSnapshot>,
    {
        self.0.read_table(snapshot, granule, current)
    }

    fn read_table_size<F>(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: F,
    ) -> Result<u64>
    where
        F: FnOnce() -> Result<u64>,
    {
        self.0.read_table_size(snapshot, granule, current)
    }

    fn read_object<F>(
        &self,
        snapshot: CommitTimestamp,
        object: ObjectId,
        current: F,
    ) -> Result<Option<ObjectPayload>>
    where
        F: FnOnce() -> Result<Option<ObjectPayload>>,
    {
        self.0.read_object(snapshot, object, current)
    }
}
