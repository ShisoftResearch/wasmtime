use super::visibility::TransactionVisibility;

mod coordinator;
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
pub(crate) use version_chain::{CommitRecord, CommitState, Version, VersionChain};

/// Feature-selected placeholder for future multiversion visibility state.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MvccVisibility;

impl MvccVisibility {
    pub(crate) const fn is_multiversion(&self) -> bool {
        true
    }
}

impl TransactionVisibility for MvccVisibility {
    type Snapshot = ();
}
