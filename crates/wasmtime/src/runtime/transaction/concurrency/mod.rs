use crate::prelude::*;
use alloc::collections::BTreeSet;
#[cfg(any(
    not(feature = "transaction-cc-strict-2pl"),
    feature = "transaction-mvcc"
))]
use alloc::sync::Arc;

use super::{ConcurrencyControl, GranuleId, TransactionId};

#[cfg(any(feature = "transaction-cc-lockbased", not(feature = "transaction")))]
mod lock_based;
#[cfg(feature = "transaction-cc-nowait-abort")]
mod no_wait_abort;
#[cfg(any(
    not(feature = "transaction-cc-strict-2pl"),
    feature = "transaction-mvcc"
))]
mod optimistic_certification;
#[cfg(feature = "transaction-cc-optimistic-validation")]
mod optimistic_validation;
#[cfg(feature = "transaction-cc-strict-2pl")]
mod strict_2pl;
#[cfg(feature = "transaction-cc-timestamp-ordering")]
mod timestamp_ordering;
#[cfg(feature = "transaction-cc-wait-die")]
mod wait_die;
#[cfg(feature = "transaction-cc-wound-wait")]
mod wound_wait;

#[cfg(all(test, feature = "transaction-cc-lockbased"))]
pub(crate) use self::lock_based::LockBased;
#[cfg(all(test, feature = "transaction-cc-lockbased"))]
pub(crate) use self::lock_based::{LockBasedConflictKindForTest, LockBasedSnapshotForTest};
#[cfg(all(
    test,
    any(
        not(feature = "transaction-cc-strict-2pl"),
        feature = "transaction-mvcc"
    )
))]
pub(crate) use self::optimistic_certification::CertificationMode;
#[cfg(any(
    not(feature = "transaction-cc-strict-2pl"),
    feature = "transaction-mvcc"
))]
pub(crate) use self::optimistic_certification::{
    OptimisticCertificationAuthority, OptimisticCertificationPermit,
};
#[cfg(feature = "transaction-mvcc")]
pub(crate) type MvccCertificationAuthority = OptimisticCertificationAuthority;
#[cfg(feature = "transaction-mvcc")]
pub(crate) type MvccCertificationPermit = OptimisticCertificationPermit;
#[cfg(feature = "transaction-cc-nowait-abort")]
pub(crate) use self::no_wait_abort::NoWaitAbort;
#[cfg(all(test, feature = "transaction-cc-nowait-abort"))]
pub(crate) use self::no_wait_abort::NoWaitAbortConflictKindForTest;
#[cfg(feature = "transaction-cc-optimistic-validation")]
pub(crate) use self::optimistic_validation::OptimisticValidation;
#[cfg(all(test, feature = "transaction-cc-optimistic-validation"))]
pub(crate) use self::optimistic_validation::OptimisticValidationConflictKindForTest;
#[cfg(feature = "transaction-cc-strict-2pl")]
pub(crate) use self::strict_2pl::StrictTwoPhaseLocking;
#[cfg(all(test, feature = "transaction-cc-strict-2pl"))]
pub(crate) use self::strict_2pl::StrictTwoPhaseLockingConflictKindForTest;
#[cfg(feature = "transaction-cc-timestamp-ordering")]
pub(crate) use self::timestamp_ordering::TimestampOrdering;
#[cfg(all(test, feature = "transaction-cc-timestamp-ordering"))]
pub(crate) use self::timestamp_ordering::TimestampOrderingConflictKindForTest;
#[cfg(feature = "transaction-cc-wait-die")]
pub(crate) use self::wait_die::WaitDie;
#[cfg(all(test, feature = "transaction-cc-wait-die"))]
pub(crate) use self::wait_die::WaitDieConflictKindForTest;
#[cfg(feature = "transaction-cc-wound-wait")]
pub(crate) use self::wound_wait::WoundWait;
#[cfg(all(test, feature = "transaction-cc-wound-wait"))]
pub(crate) use self::wound_wait::WoundWaitConflictKindForTest;

#[cfg(any(feature = "transaction-cc-lockbased", not(feature = "transaction")))]
type SelectedConcurrencyControl = lock_based::LockBased;
#[cfg(feature = "transaction-cc-nowait-abort")]
type SelectedConcurrencyControl = no_wait_abort::NoWaitAbort;
#[cfg(feature = "transaction-cc-optimistic-validation")]
type SelectedConcurrencyControl = optimistic_validation::OptimisticValidation;
#[cfg(feature = "transaction-cc-strict-2pl")]
type SelectedConcurrencyControl = strict_2pl::StrictTwoPhaseLocking;
#[cfg(feature = "transaction-cc-timestamp-ordering")]
type SelectedConcurrencyControl = timestamp_ordering::TimestampOrdering;
#[cfg(feature = "transaction-cc-wait-die")]
type SelectedConcurrencyControl = wait_die::WaitDie;
#[cfg(feature = "transaction-cc-wound-wait")]
type SelectedConcurrencyControl = wound_wait::WoundWait;

type TransactionTimestamp = TransactionId;

fn timestamp_for_transaction(transaction: TransactionId) -> TransactionTimestamp {
    transaction
}

fn is_older(requester: TransactionId, owner: TransactionId) -> bool {
    timestamp_for_transaction(requester) < timestamp_for_transaction(owner)
}

fn is_younger(requester: TransactionId, owner: TransactionId) -> bool {
    timestamp_for_transaction(requester) > timestamp_for_transaction(owner)
}

#[cfg(test)]
pub(crate) fn timestamp_for_transaction_for_test(
    transaction: TransactionId,
) -> TransactionTimestamp {
    timestamp_for_transaction(transaction)
}

#[cfg(test)]
pub(crate) fn is_older_for_test(requester: TransactionId, owner: TransactionId) -> bool {
    is_older(requester, owner)
}

#[cfg(test)]
pub(crate) fn is_younger_for_test(requester: TransactionId, owner: TransactionId) -> bool {
    is_younger(requester, owner)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransactionConflictAction {
    Continue,
    AbortOther(TransactionId),
    WouldWait(TransactionId),
}

impl TransactionConflictAction {
    pub(crate) fn aborted_transaction(self) -> Option<TransactionId> {
        match self {
            Self::AbortOther(transaction) => Some(transaction),
            Self::Continue | Self::WouldWait(_) => None,
        }
    }

    pub(crate) fn would_wait_owner(self) -> Option<TransactionId> {
        match self {
            Self::WouldWait(owner) => Some(owner),
            Self::Continue | Self::AbortOther(_) => None,
        }
    }
}

fn action_from_aborted_transaction(aborted: Option<TransactionId>) -> TransactionConflictAction {
    match aborted {
        Some(transaction) => TransactionConflictAction::AbortOther(transaction),
        None => TransactionConflictAction::Continue,
    }
}

pub(crate) trait TransactionConcurrencyControl {
    fn acquire_granule_read(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction>;

    fn acquire_granule_write(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction>;

    fn validate_read(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()>;

    fn validate_write(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()>;

    fn refresh_read_version(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    );

    fn release_transaction(&mut self, transaction: TransactionId);

    fn release_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        self.release_transaction(transaction);
        Ok(())
    }

    fn owner_for_granule(&self, granule: GranuleId) -> Option<TransactionId>;

    fn read_granules_for_transaction(&self, transaction: TransactionId) -> Vec<GranuleId>;

    fn write_granules_for_transaction(&self, transaction: TransactionId) -> Vec<GranuleId>;

    fn commit_transaction_result(&mut self, _transaction: TransactionId) -> Result<()> {
        Ok(())
    }

    #[cfg(test)]
    fn remove_owner_for_granule_for_test(&mut self, _granule: GranuleId) -> Option<TransactionId> {
        None
    }

    #[cfg(test)]
    fn remove_write_version_for_test(
        &mut self,
        _transaction: TransactionId,
        _granule: GranuleId,
    ) -> bool {
        false
    }

    #[cfg(test)]
    fn committed_transactions_for_test(&self) -> Option<BTreeSet<TransactionId>> {
        None
    }

    #[cfg(test)]
    fn fail_commit_transaction_once_for_test(&mut self) {}
}

#[derive(Debug, Default)]
pub(crate) struct ConcurrencyControlState {
    policy: SelectedConcurrencyControl,
    #[cfg(any(
        not(feature = "transaction-cc-strict-2pl"),
        feature = "transaction-mvcc"
    ))]
    commit_certification: Arc<OptimisticCertificationAuthority>,
}

impl ConcurrencyControlState {
    pub(crate) fn for_config(policy: ConcurrencyControl) -> Result<Self> {
        policy.ensure_compiled_for_build()?;
        Ok(Self::default())
    }

    pub(crate) fn acquire_granule_read(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction> {
        self.policy
            .acquire_granule_read(transaction, granule, current_version)
    }

    pub(crate) fn acquire_granule_write(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction> {
        self.policy
            .acquire_granule_write(transaction, granule, current_version)
    }

    #[cfg(feature = "transaction-mvcc")]
    pub(crate) fn acquire_mvcc_granule_write(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction> {
        self.policy
            .acquire_granule_write(transaction, granule, current_version)
    }

    pub(crate) fn validate_read(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.policy
            .validate_read(transaction, granule, current_version)
    }

    pub(crate) fn validate_write(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.policy
            .validate_write(transaction, granule, current_version)
    }

    pub(crate) fn refresh_read_version(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) {
        self.policy
            .refresh_read_version(transaction, granule, current_version);
    }

    pub(crate) fn release_transaction(&mut self, transaction: TransactionId) {
        self.policy.release_transaction(transaction);
    }

    pub(crate) fn release_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        self.policy.release_transaction_result(transaction)
    }

    pub(crate) fn owner_for_granule(&self, granule: GranuleId) -> Option<TransactionId> {
        self.policy.owner_for_granule(granule)
    }

    pub(crate) fn read_granules_for_transaction(
        &self,
        transaction: TransactionId,
    ) -> Vec<GranuleId> {
        self.policy.read_granules_for_transaction(transaction)
    }

    pub(crate) fn write_granules_for_transaction(
        &self,
        transaction: TransactionId,
    ) -> Vec<GranuleId> {
        self.policy.write_granules_for_transaction(transaction)
    }

    pub(crate) fn commit_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        self.policy.commit_transaction_result(transaction)
    }

    #[cfg(not(feature = "transaction-cc-strict-2pl"))]
    pub(crate) fn acquire_optimistic_certification(
        &self,
        transaction: TransactionId,
        reads: &BTreeSet<GranuleId>,
        writes: &BTreeSet<GranuleId>,
    ) -> Result<OptimisticCertificationPermit> {
        self.commit_certification
            .acquire(transaction, reads, writes)
    }

    #[cfg(not(feature = "transaction-cc-strict-2pl"))]
    pub(super) fn optimistic_certification_authority(
        &self,
    ) -> Arc<OptimisticCertificationAuthority> {
        self.commit_certification.clone()
    }

    #[cfg(feature = "transaction-mvcc")]
    pub(crate) fn acquire_mvcc_certification(
        &self,
        transaction: TransactionId,
        reads: &BTreeSet<GranuleId>,
        writes: &BTreeSet<GranuleId>,
    ) -> Result<MvccCertificationPermit> {
        self.commit_certification
            .acquire(transaction, reads, writes)
    }

    #[cfg(feature = "transaction-mvcc")]
    pub(super) fn mvcc_certification_authority(&self) -> Arc<MvccCertificationAuthority> {
        self.commit_certification.clone()
    }

    #[cfg(test)]
    pub(crate) fn remove_owner_for_granule_for_test(
        &mut self,
        granule: GranuleId,
    ) -> Option<TransactionId> {
        self.policy.remove_owner_for_granule_for_test(granule)
    }

    #[cfg(test)]
    pub(crate) fn remove_write_version_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
    ) -> bool {
        self.policy
            .remove_write_version_for_test(transaction, granule)
    }

    #[cfg(test)]
    pub(crate) fn committed_transactions_for_test(&self) -> Option<BTreeSet<TransactionId>> {
        TransactionConcurrencyControl::committed_transactions_for_test(&self.policy)
    }

    #[cfg(test)]
    pub(crate) fn fail_commit_transaction_once_for_test(&mut self) {
        self.policy.fail_commit_transaction_once_for_test();
    }
}

impl TransactionConcurrencyControl for ConcurrencyControlState {
    fn acquire_granule_read(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction> {
        self.policy
            .acquire_granule_read(transaction, granule, current_version)
    }

    fn acquire_granule_write(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction> {
        self.policy
            .acquire_granule_write(transaction, granule, current_version)
    }

    fn validate_read(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.policy
            .validate_read(transaction, granule, current_version)
    }

    fn validate_write(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.policy
            .validate_write(transaction, granule, current_version)
    }

    fn refresh_read_version(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) {
        self.policy
            .refresh_read_version(transaction, granule, current_version);
    }

    fn release_transaction(&mut self, transaction: TransactionId) {
        self.policy.release_transaction(transaction);
    }

    fn release_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        self.policy.release_transaction_result(transaction)
    }

    fn owner_for_granule(&self, granule: GranuleId) -> Option<TransactionId> {
        self.policy.owner_for_granule(granule)
    }

    fn read_granules_for_transaction(&self, transaction: TransactionId) -> Vec<GranuleId> {
        self.policy.read_granules_for_transaction(transaction)
    }

    fn write_granules_for_transaction(&self, transaction: TransactionId) -> Vec<GranuleId> {
        self.policy.write_granules_for_transaction(transaction)
    }

    fn commit_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        self.policy.commit_transaction_result(transaction)
    }
}
