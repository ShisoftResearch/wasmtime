use super::super::visibility::{BASELINE_TIMESTAMP, CommitTimestamp};
use crate::prelude::*;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

const PENDING_STATE: u64 = 0;
const ABORTED_STATE: u64 = u64::MAX;

/// The outcome of a transaction's commit attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommitState {
    Pending,
    Committed(CommitTimestamp),
    Aborted,
}

/// A commit outcome shared by every version written by one transaction.
#[derive(Debug)]
pub(crate) struct CommitRecord {
    state: AtomicU64,
    is_baseline: bool,
}

impl CommitRecord {
    pub(crate) fn pending() -> Self {
        Self {
            state: AtomicU64::new(PENDING_STATE),
            is_baseline: false,
        }
    }

    pub(crate) fn committed_for_baseline(timestamp: CommitTimestamp) -> Result<Self> {
        if timestamp == ABORTED_STATE {
            bail!("commit timestamp must not be u64::MAX");
        }

        Ok(Self {
            state: AtomicU64::new(timestamp),
            is_baseline: timestamp == BASELINE_TIMESTAMP,
        })
    }

    pub(crate) fn state(&self) -> CommitState {
        match self.state.load(Ordering::Acquire) {
            PENDING_STATE if self.is_baseline => CommitState::Committed(BASELINE_TIMESTAMP),
            PENDING_STATE => CommitState::Pending,
            ABORTED_STATE => CommitState::Aborted,
            timestamp => CommitState::Committed(timestamp),
        }
    }

    pub(crate) fn commit(&self, timestamp: CommitTimestamp) -> Result<()> {
        if !matches!(self.state(), CommitState::Pending) {
            bail!("commit record is no longer pending");
        }
        if timestamp == PENDING_STATE || timestamp == ABORTED_STATE {
            bail!("commit timestamp must be in 1..u64::MAX");
        }

        self.state
            .compare_exchange(
                PENDING_STATE,
                timestamp,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| crate::format_err!("commit record is no longer pending"))
    }

    pub(crate) fn abort(&self) -> Result<()> {
        if !matches!(self.state(), CommitState::Pending) {
            bail!("commit record is no longer pending");
        }

        self.state
            .compare_exchange(
                PENDING_STATE,
                ABORTED_STATE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| crate::format_err!("commit record is no longer pending"))
    }
}

/// A value together with the transaction that made it visible.
#[derive(Clone, Debug)]
pub(crate) struct Version<T> {
    pub(crate) commit: Arc<CommitRecord>,
    pub(crate) value: T,
}

/// Versions of one value, ordered from newest to oldest.
#[derive(Clone, Debug, Default)]
pub(crate) struct VersionChain<T> {
    versions: VecDeque<Version<T>>,
}

impl<T> VersionChain<T> {
    pub(crate) fn new_baseline(commit: Arc<CommitRecord>, value: T) -> Result<Self> {
        if !matches!(commit.state(), CommitState::Committed(_)) {
            bail!("baseline version requires a committed commit record");
        }

        let mut chain = Self::new_pending();
        chain.push_newest(commit, value);
        Ok(chain)
    }

    pub(crate) fn new_pending() -> Self {
        Self {
            versions: VecDeque::new(),
        }
    }

    pub(crate) fn push_newest(&mut self, commit: Arc<CommitRecord>, value: T) {
        self.versions.push_front(Version { commit, value });
    }

    pub(crate) fn read_visible(&self, snapshot: CommitTimestamp) -> Option<&T> {
        self.versions
            .iter()
            .find_map(|version| match version.commit.state() {
                CommitState::Committed(timestamp) if timestamp <= snapshot => Some(&version.value),
                CommitState::Pending | CommitState::Aborted | CommitState::Committed(_) => None,
            })
    }

    pub(crate) fn newest_committed_timestamp(&self) -> Option<CommitTimestamp> {
        self.versions
            .iter()
            .find_map(|version| match version.commit.state() {
                CommitState::Committed(timestamp) => Some(timestamp),
                CommitState::Pending | CommitState::Aborted => None,
            })
    }

    pub(crate) fn committed_predecessor(&self) -> Option<&T> {
        self.versions
            .iter()
            .find_map(|version| match version.commit.state() {
                CommitState::Committed(_) => Some(&version.value),
                CommitState::Pending | CommitState::Aborted => None,
            })
    }

    pub(crate) fn remove_newest_for_commit(&mut self, commit: &Arc<CommitRecord>) -> bool {
        if self
            .versions
            .front()
            .is_some_and(|version| Arc::ptr_eq(&version.commit, commit))
        {
            self.versions.pop_front();
            true
        } else {
            false
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.versions.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn newest_commit_record(&self) -> Option<&Arc<CommitRecord>> {
        self.versions.front().map(|version| &version.commit)
    }

    #[cfg(test)]
    pub(crate) fn version_count(&self) -> usize {
        self.versions.len()
    }

    pub(crate) fn prune_for_oldest_snapshot(&mut self, oldest_snapshot: Option<CommitTimestamp>) {
        let mut retained_predecessor = false;
        self.versions
            .retain(|version| match version.commit.state() {
                CommitState::Aborted => false,
                CommitState::Pending => true,
                CommitState::Committed(timestamp) => match oldest_snapshot {
                    Some(snapshot) if timestamp > snapshot => true,
                    Some(_) | None if !retained_predecessor => {
                        retained_predecessor = true;
                        true
                    }
                    Some(_) | None => false,
                },
            });
    }

    #[cfg(test)]
    fn committed_timestamps(&self) -> Vec<CommitTimestamp> {
        self.versions
            .iter()
            .filter_map(|version| match version.commit.state() {
                CommitState::Committed(timestamp) => Some(timestamp),
                CommitState::Pending | CommitState::Aborted => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;

    #[test]
    fn pending_and_aborted_versions_are_invisible() {
        let baseline = Arc::new(CommitRecord::committed_for_baseline(0).unwrap());
        let pending = Arc::new(CommitRecord::pending());
        let aborted = Arc::new(CommitRecord::pending());
        let mut chain = VersionChain::new_baseline(baseline, 10_u64).unwrap();
        chain.push_newest(pending, 20);
        chain.push_newest(aborted.clone(), 30);
        aborted.abort().unwrap();

        assert_eq!(chain.read_visible(100), Some(&10));
    }

    #[test]
    fn baseline_requires_a_committed_record() {
        let pending = Arc::new(CommitRecord::pending());
        let aborted = Arc::new(CommitRecord::pending());
        aborted.abort().unwrap();

        assert!(VersionChain::new_baseline(pending, 1_u64).is_err());
        assert!(VersionChain::new_baseline(aborted, 2_u64).is_err());
    }

    #[test]
    fn one_commit_record_exposes_multiple_chains_atomically() {
        let baseline = Arc::new(CommitRecord::committed_for_baseline(0).unwrap());
        let commit = Arc::new(CommitRecord::pending());
        let mut left = VersionChain::new_baseline(baseline.clone(), 1_u64).unwrap();
        let mut right = VersionChain::new_baseline(baseline, 2_u64).unwrap();
        left.push_newest(commit.clone(), 11);
        right.push_newest(commit.clone(), 22);

        assert_eq!(left.read_visible(1), Some(&1));
        assert_eq!(right.read_visible(1), Some(&2));
        commit.commit(1).unwrap();
        assert_eq!(left.read_visible(1), Some(&11));
        assert_eq!(right.read_visible(1), Some(&22));
    }

    #[test]
    fn horizon_pruning_keeps_one_predecessor_and_all_newer_commits() {
        let mut chain = VersionChain::new_baseline(
            Arc::new(CommitRecord::committed_for_baseline(0).unwrap()),
            0_u64,
        )
        .unwrap();
        for timestamp in 1..=5 {
            chain.push_newest(
                Arc::new(CommitRecord::committed_for_baseline(timestamp).unwrap()),
                timestamp,
            );
        }

        chain.prune_for_oldest_snapshot(Some(3));
        assert_eq!(chain.committed_timestamps(), vec![5, 4, 3]);
        assert_eq!(chain.read_visible(3), Some(&3));
    }

    #[test]
    fn commit_records_allow_only_one_valid_terminal_transition() {
        let record = CommitRecord::pending();

        assert_eq!(record.state(), CommitState::Pending);
        assert!(record.commit(0).is_err());
        assert!(record.commit(u64::MAX).is_err());
        record.commit(7).unwrap();
        assert_eq!(record.state(), CommitState::Committed(7));
        assert!(
            record
                .commit(0)
                .unwrap_err()
                .to_string()
                .contains("commit record is no longer pending")
        );
        assert!(
            record
                .abort()
                .unwrap_err()
                .to_string()
                .contains("commit record is no longer pending")
        );
    }

    #[test]
    fn pruning_removes_aborted_versions_but_retains_pending_versions() {
        let mut chain = VersionChain::new_pending();
        let pending = Arc::new(CommitRecord::pending());
        let aborted = Arc::new(CommitRecord::pending());
        chain.push_newest(pending, 1_u64);
        chain.push_newest(aborted.clone(), 2_u64);
        aborted.abort().unwrap();

        chain.prune_for_oldest_snapshot(None);
        assert_eq!(chain.read_visible(100), None);
        assert_eq!(chain.versions.len(), 1);
        assert_eq!(chain.versions[0].commit.state(), CommitState::Pending);
    }

    #[test]
    fn committed_accessors_skip_pending_and_aborted_versions() {
        let baseline = Arc::new(CommitRecord::committed_for_baseline(1).unwrap());
        let pending = Arc::new(CommitRecord::pending());
        let aborted = Arc::new(CommitRecord::pending());
        let mut chain = VersionChain::new_baseline(baseline, 10_u64).unwrap();
        chain.push_newest(aborted.clone(), 20);
        chain.push_newest(pending.clone(), 30);
        aborted.abort().unwrap();

        assert_eq!(chain.newest_committed_timestamp(), Some(1));
        assert_eq!(chain.committed_predecessor(), Some(&10));
        pending.commit(2).unwrap();
        assert_eq!(chain.newest_committed_timestamp(), Some(2));
        assert_eq!(chain.committed_predecessor(), Some(&30));
    }

    #[test]
    fn pruning_without_snapshots_keeps_the_newest_committed_version() {
        let mut chain = VersionChain::new_baseline(
            Arc::new(CommitRecord::committed_for_baseline(0).unwrap()),
            0_u64,
        )
        .unwrap();
        for timestamp in 1..=2 {
            chain.push_newest(
                Arc::new(CommitRecord::committed_for_baseline(timestamp).unwrap()),
                timestamp,
            );
        }

        chain.prune_for_oldest_snapshot(None);
        assert_eq!(chain.committed_timestamps(), vec![2]);
        assert_eq!(chain.read_visible(2), Some(&2));
    }
}
