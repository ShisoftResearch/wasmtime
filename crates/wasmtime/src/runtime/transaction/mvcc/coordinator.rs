use super::super::visibility::CommitTimestamp;
use super::{CommitRecord, CommitState};
use crate::prelude::*;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use std::sync::{Mutex, MutexGuard};

/// Shared lifecycle state for MVCC visibility and persistent GC.
#[derive(Debug, Default)]
struct CoordinatorState {
    visible_timestamp: CommitTimestamp,
    baseline_timestamp: CommitTimestamp,
    active_snapshots: BTreeMap<CommitTimestamp, usize>,
    #[cfg(test)]
    snapshots_begun: usize,
    #[cfg(test)]
    snapshots_finished: usize,
    #[cfg(test)]
    snapshots_dropped: usize,
    #[cfg(test)]
    fail_finish_snapshot_once: bool,
    pending_commits: usize,
    #[cfg(test)]
    pending_commits_registered: usize,
    #[cfg(test)]
    pending_commits_published: usize,
    #[cfg(test)]
    pending_commits_aborted: usize,
    gc_active: bool,
    gc_barrier_generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MvccGcMetadata {
    pub(crate) oldest_active_snapshot: Option<CommitTimestamp>,
    pub(crate) visible_timestamp: CommitTimestamp,
    pub(crate) baseline_timestamp: CommitTimestamp,
    pub(crate) quiescent: bool,
}

#[derive(Debug)]
pub(super) struct MvccGcBarrierEpoch {
    state: Arc<Mutex<CoordinatorState>>,
    generation: u64,
}

/// Coordinates MVCC snapshots, commit publication, and persistent GC.
#[derive(Debug, Default)]
pub(crate) struct MvccCoordinator {
    state: Arc<Mutex<CoordinatorState>>,
}

impl MvccCoordinator {
    /// Returns the timestamp assigned to values without an allocated chain.
    pub(crate) fn baseline_timestamp(&self) -> Result<CommitTimestamp> {
        Ok(self.lock()?.baseline_timestamp)
    }

    /// Returns the timestamp of the latest commit made visible to new snapshots.
    pub(crate) fn visible_timestamp(&self) -> Result<CommitTimestamp> {
        Ok(self.lock()?.visible_timestamp)
    }

    /// Registers a snapshot of the currently visible timestamp.
    pub(crate) fn begin_snapshot(&self) -> Result<SnapshotRegistration> {
        let mut state = self.lock()?;
        ensure!(!state.gc_active, "persistent GC is active");

        let timestamp = state.visible_timestamp;
        *state.active_snapshots.entry(timestamp).or_default() += 1;
        #[cfg(test)]
        {
            state.snapshots_begun += 1;
        }
        Ok(SnapshotRegistration {
            timestamp,
            state: Some(self.state.clone()),
        })
    }

    /// Registers a pending commit that must be published or aborted exactly once.
    pub(crate) fn register_pending_commit(
        &self,
        commit: Arc<CommitRecord>,
    ) -> Result<PendingCommitRegistration> {
        let mut state = self.lock()?;
        ensure!(!state.gc_active, "persistent GC is active");
        state.pending_commits += 1;
        #[cfg(test)]
        {
            state.pending_commits_registered += 1;
        }
        Ok(PendingCommitRegistration {
            commit,
            state: Some(self.state.clone()),
        })
    }

    /// Begins a persistent-GC interval once no MVCC work remains active.
    pub(crate) fn begin_gc_barrier(&self) -> Result<MvccGcBarrierPermit> {
        let mut state = self.lock()?;
        ensure!(!state.gc_active, "persistent GC is already active");
        ensure!(
            state.active_snapshots.is_empty(),
            "MVCC snapshots are active"
        );
        ensure!(state.pending_commits == 0, "MVCC commits are pending");
        let generation = state
            .gc_barrier_generation
            .checked_add(1)
            .context("MVCC GC barrier generation overflow")?;
        state.gc_barrier_generation = generation;
        state.gc_active = true;
        Ok(MvccGcBarrierPermit {
            state: Some(self.state.clone()),
            generation,
        })
    }

    /// Captures a horizon that remains safe if a new snapshot begins before
    /// the domain-side pruning lock is acquired.
    pub(crate) fn pruning_horizon(&self) -> Result<CommitTimestamp> {
        let state = self.lock()?;
        Ok(state
            .active_snapshots
            .keys()
            .next()
            .copied()
            .unwrap_or(state.visible_timestamp))
    }

    pub(crate) fn metadata(&self) -> Result<MvccGcMetadata> {
        let state = self.lock()?;
        Ok(gc_metadata(&state))
    }

    pub(super) fn with_gc_barrier<R>(
        &self,
        permit: &MvccGcBarrierPermit,
        operation: impl FnOnce(MvccGcMetadata, MvccGcBarrierEpoch) -> Result<R>,
    ) -> Result<R> {
        let state = self.lock_gc_barrier(permit)?;
        let metadata = gc_metadata(&state);
        let epoch = MvccGcBarrierEpoch {
            state: self.state.clone(),
            generation: state.gc_barrier_generation,
        };
        operation(metadata, epoch)
    }

    pub(super) fn complete_gc_rebase<R>(
        &self,
        permit: &MvccGcBarrierPermit,
        expected_epoch: &MvccGcBarrierEpoch,
        expected_metadata: MvccGcMetadata,
        operation: impl FnOnce(MvccGcMetadata) -> Result<R>,
    ) -> Result<(R, MvccGcMetadata)> {
        let mut state = self.lock_gc_barrier(permit)?;
        ensure!(
            Arc::ptr_eq(&expected_epoch.state, &self.state),
            "validated MVCC GC rebase plan belongs to another coordinator"
        );
        ensure!(
            expected_epoch.generation == state.gc_barrier_generation,
            "validated MVCC GC rebase plan belongs to another barrier epoch"
        );
        let metadata = gc_metadata(&state);
        ensure!(
            metadata == expected_metadata,
            "MVCC GC coordinator metadata changed after object validation"
        );
        let value = operation(metadata)?;
        state.baseline_timestamp = state.visible_timestamp;
        Ok((value, gc_metadata(&state)))
    }

    #[cfg(test)]
    pub(crate) fn snapshot_lifecycle_counts_for_test(
        &self,
    ) -> Result<(usize, usize, usize, usize)> {
        let state = self.lock()?;
        Ok((
            state.active_snapshots.values().sum(),
            state.snapshots_begun,
            state.snapshots_finished,
            state.snapshots_dropped,
        ))
    }

    #[cfg(test)]
    pub(crate) fn fail_finish_snapshot_once_for_test(&self) -> Result<()> {
        self.lock()?.fail_finish_snapshot_once = true;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn commit_lifecycle_counts_for_test(&self) -> Result<(usize, usize, usize)> {
        let state = self.lock()?;
        Ok((
            state.pending_commits_registered,
            state.pending_commits_published,
            state.pending_commits_aborted,
        ))
    }

    fn lock(&self) -> Result<MutexGuard<'_, CoordinatorState>> {
        self.state
            .lock()
            .map_err(|_| crate::format_err!("MVCC coordinator lock is poisoned"))
    }

    fn lock_gc_barrier<'a>(
        &'a self,
        permit: &MvccGcBarrierPermit,
    ) -> Result<MutexGuard<'a, CoordinatorState>> {
        let permit_state = permit
            .state
            .as_ref()
            .context("MVCC GC barrier permit is no longer active")?;
        ensure!(
            Arc::ptr_eq(permit_state, &self.state),
            "MVCC GC barrier permit belongs to another coordinator"
        );
        let state = self.lock()?;
        ensure!(
            state.gc_active,
            "MVCC GC barrier permit is no longer active"
        );
        ensure!(
            state.gc_barrier_generation == permit.generation,
            "MVCC GC barrier permit belongs to another barrier epoch"
        );
        ensure!(
            state.active_snapshots.is_empty(),
            "MVCC snapshots are active"
        );
        ensure!(state.pending_commits == 0, "MVCC commits are pending");
        Ok(state)
    }
}

fn gc_metadata(state: &CoordinatorState) -> MvccGcMetadata {
    MvccGcMetadata {
        oldest_active_snapshot: state.active_snapshots.keys().next().copied(),
        visible_timestamp: state.visible_timestamp,
        baseline_timestamp: state.baseline_timestamp,
        quiescent: state.active_snapshots.is_empty() && state.pending_commits == 0,
    }
}

/// A non-cloneable registration for one active MVCC snapshot.
#[derive(Debug)]
pub(crate) struct SnapshotRegistration {
    timestamp: CommitTimestamp,
    state: Option<Arc<Mutex<CoordinatorState>>>,
}

impl SnapshotRegistration {
    /// Returns the timestamp captured when this registration was created.
    pub(crate) fn timestamp(&self) -> CommitTimestamp {
        self.timestamp
    }

    /// Explicitly unregisters this snapshot.
    pub(crate) fn finish(mut self) -> Result<()> {
        self.unregister(true)
    }

    fn unregister(&mut self, _explicit: bool) -> Result<()> {
        let Some(state) = self.state.clone() else {
            return Ok(());
        };
        let mut state = state
            .lock()
            .map_err(|_| crate::format_err!("MVCC coordinator lock is poisoned"))?;
        #[cfg(test)]
        if _explicit && core::mem::take(&mut state.fail_finish_snapshot_once) {
            bail!("injected snapshot finish failure");
        }
        self.state = None;
        let count = state
            .active_snapshots
            .get_mut(&self.timestamp)
            .expect("snapshot registration must have a multiset entry");
        if *count == 1 {
            state.active_snapshots.remove(&self.timestamp);
        } else {
            *count -= 1;
        }
        #[cfg(test)]
        if _explicit {
            state.snapshots_finished += 1;
        } else {
            state.snapshots_dropped += 1;
        }
        Ok(())
    }
}

impl Drop for SnapshotRegistration {
    fn drop(&mut self) {
        let _ = self.unregister(false);
    }
}

/// A non-cloneable registration for a pending commit record.
#[derive(Debug)]
pub(crate) struct PendingCommitRegistration {
    commit: Arc<CommitRecord>,
    state: Option<Arc<Mutex<CoordinatorState>>>,
}

impl PendingCommitRegistration {
    /// Makes the pending record visible at the next consecutive timestamp.
    pub(crate) fn publish(&mut self) -> Result<CommitTimestamp> {
        let state = self
            .state
            .as_ref()
            .expect("pending commit registration must be active");
        let mut state = state
            .lock()
            .map_err(|_| crate::format_err!("MVCC coordinator lock is poisoned"))?;
        ensure!(
            matches!(self.commit.state(), CommitState::Pending),
            "commit record is no longer pending"
        );
        let timestamp = state
            .visible_timestamp
            .checked_add(1)
            .filter(|timestamp| *timestamp != u64::MAX)
            .ok_or_else(|| crate::format_err!("MVCC commit timestamp overflow"))?;
        self.commit.commit(timestamp)?;
        state.visible_timestamp = timestamp;
        state.pending_commits -= 1;
        #[cfg(test)]
        {
            state.pending_commits_published += 1;
        }
        drop(state);
        self.state = None;
        Ok(timestamp)
    }

    /// Aborts the pending record and unregisters this pending commit.
    pub(crate) fn abort(&mut self) -> Result<()> {
        let state = self
            .state
            .as_ref()
            .expect("pending commit registration must be active");
        let mut state = state
            .lock()
            .map_err(|_| crate::format_err!("MVCC coordinator lock is poisoned"))?;
        self.commit.abort()?;
        state.pending_commits -= 1;
        #[cfg(test)]
        {
            state.pending_commits_aborted += 1;
        }
        drop(state);
        self.state = None;
        Ok(())
    }
}

impl Drop for PendingCommitRegistration {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        let Ok(mut state) = state.lock() else {
            return;
        };
        if matches!(self.commit.state(), CommitState::Pending) {
            let _ = self.commit.abort();
            #[cfg(test)]
            {
                state.pending_commits_aborted += 1;
            }
        }
        state.pending_commits -= 1;
    }
}

/// Keeps the persistent-GC begin barrier active for its lifetime.
#[derive(Debug)]
pub(crate) struct MvccGcBarrierPermit {
    state: Option<Arc<Mutex<CoordinatorState>>>,
    generation: u64,
}

impl Drop for MvccGcBarrierPermit {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        let Ok(mut state) = state.lock() else {
            return;
        };
        state.gc_active = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;

    fn snapshot_count(coordinator: &MvccCoordinator, timestamp: CommitTimestamp) -> usize {
        coordinator
            .state
            .lock()
            .unwrap()
            .active_snapshots
            .get(&timestamp)
            .copied()
            .unwrap_or_default()
    }

    #[test]
    fn begin_snapshot_captures_and_registers_visible_timestamp() {
        let coordinator = MvccCoordinator::default();
        let first = coordinator.begin_snapshot().unwrap();

        assert_eq!(first.timestamp(), 0);
        assert_eq!(snapshot_count(&coordinator, 0), 1);

        let commit = Arc::new(CommitRecord::pending());
        coordinator
            .register_pending_commit(commit)
            .unwrap()
            .publish()
            .unwrap();
        let second = coordinator.begin_snapshot().unwrap();

        assert_eq!(second.timestamp(), 1);
        assert_eq!(snapshot_count(&coordinator, 0), 1);
        assert_eq!(snapshot_count(&coordinator, 1), 1);
    }

    #[test]
    fn finishing_snapshot_deregisters_exactly_once() {
        let coordinator = MvccCoordinator::default();
        let snapshot = coordinator.begin_snapshot().unwrap();

        assert_eq!(snapshot_count(&coordinator, 0), 1);
        snapshot.finish().unwrap();
        assert_eq!(snapshot_count(&coordinator, 0), 0);
    }

    #[test]
    fn dropping_snapshot_deregisters_exactly_once() {
        let coordinator = MvccCoordinator::default();
        let snapshot = coordinator.begin_snapshot().unwrap();

        assert_eq!(snapshot_count(&coordinator, 0), 1);
        drop(snapshot);
        assert_eq!(snapshot_count(&coordinator, 0), 0);
    }

    #[test]
    fn same_timestamp_snapshots_are_counted_separately() {
        let coordinator = MvccCoordinator::default();
        let first = coordinator.begin_snapshot().unwrap();
        let second = coordinator.begin_snapshot().unwrap();

        assert_eq!(snapshot_count(&coordinator, 0), 2);
        first.finish().unwrap();
        assert_eq!(snapshot_count(&coordinator, 0), 1);
        second.finish().unwrap();
        assert_eq!(snapshot_count(&coordinator, 0), 0);
    }

    #[test]
    fn coordinator_assigns_timestamp_at_visibility_transition() {
        let coordinator = MvccCoordinator::default();
        let commit = Arc::new(CommitRecord::pending());
        let mut prepared = coordinator.register_pending_commit(commit.clone()).unwrap();

        assert_eq!(coordinator.visible_timestamp().unwrap(), 0);
        assert_eq!(commit.state(), CommitState::Pending);
        let timestamp = prepared.publish().unwrap();
        assert_eq!(timestamp, 1);
        assert_eq!(commit.state(), CommitState::Committed(1));
        assert_eq!(coordinator.visible_timestamp().unwrap(), 1);
    }

    #[test]
    fn publication_allocates_consecutive_timestamps() {
        let coordinator = MvccCoordinator::default();
        let mut first = coordinator
            .register_pending_commit(Arc::new(CommitRecord::pending()))
            .unwrap();
        let mut second = coordinator
            .register_pending_commit(Arc::new(CommitRecord::pending()))
            .unwrap();

        assert_eq!(first.publish().unwrap(), 1);
        assert_eq!(second.publish().unwrap(), 2);
        assert_eq!(coordinator.visible_timestamp().unwrap(), 2);
    }

    #[test]
    fn gc_barrier_rejects_active_snapshots_and_pending_commits() {
        let coordinator = MvccCoordinator::default();
        let snapshot = coordinator.begin_snapshot().unwrap();

        assert_eq!(
            coordinator.begin_gc_barrier().unwrap_err().to_string(),
            "MVCC snapshots are active"
        );
        snapshot.finish().unwrap();

        let mut pending = coordinator
            .register_pending_commit(Arc::new(CommitRecord::pending()))
            .unwrap();
        assert_eq!(
            coordinator.begin_gc_barrier().unwrap_err().to_string(),
            "MVCC commits are pending"
        );
        pending.abort().unwrap();
    }

    #[test]
    fn gc_barrier_blocks_new_snapshots_until_dropped() {
        let coordinator = MvccCoordinator::default();
        let barrier = coordinator.begin_gc_barrier().unwrap();

        assert_eq!(
            coordinator.begin_snapshot().unwrap_err().to_string(),
            "persistent GC is active"
        );
        drop(barrier);
        coordinator.begin_snapshot().unwrap().finish().unwrap();
    }

    #[test]
    fn failed_registration_abort_retains_registration_until_explicit_drop() {
        let coordinator = MvccCoordinator::default();
        let commit = Arc::new(CommitRecord::pending());
        let mut prepared = coordinator.register_pending_commit(commit.clone()).unwrap();
        commit.commit(1).unwrap();

        assert_eq!(
            prepared.abort().unwrap_err().to_string(),
            "commit record is no longer pending"
        );
        assert!(prepared.state.is_some());
        assert_eq!(commit.state(), CommitState::Committed(1));
        drop(prepared);
        coordinator.begin_gc_barrier().unwrap();
        assert_eq!(
            coordinator.commit_lifecycle_counts_for_test().unwrap(),
            (1, 0, 0)
        );
    }

    #[test]
    fn pending_commit_drop_does_not_abort_after_lock_poisoning() {
        let coordinator = MvccCoordinator::default();
        let commit = Arc::new(CommitRecord::pending());
        let pending = coordinator.register_pending_commit(commit.clone()).unwrap();
        let state = coordinator.state.clone();

        let _ = std::thread::spawn(move || {
            let _guard = state.lock().unwrap();
            panic!("poison the coordinator mutex");
        })
        .join();

        drop(pending);
        assert_eq!(commit.state(), CommitState::Pending);
    }
}
