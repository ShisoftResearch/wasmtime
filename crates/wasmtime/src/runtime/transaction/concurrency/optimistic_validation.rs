use crate::prelude::*;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;

use super::super::{GranuleId, TransactionId};
use super::{OptimisticCertificationAuthority, OptimisticCertificationPermit};
use super::{TransactionConcurrencyControl, TransactionConflictAction};

#[derive(Debug, Default)]
pub(crate) struct OptimisticValidation {
    pub(crate) read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
    pub(crate) write_versions: BTreeMap<(TransactionId, GranuleId), u64>,
    commit_certification: Arc<OptimisticCertificationAuthority>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OptimisticValidationConflictKind {
    ReadVersionMismatch,
    WriteVersionMismatch,
}

impl OptimisticValidationConflictKind {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::ReadVersionMismatch => {
                "transaction read conflict: optimistic read version changed"
            }
            Self::WriteVersionMismatch => {
                "transaction write conflict: optimistic write version changed"
            }
        }
    }
}

#[cfg(test)]
pub(crate) use self::OptimisticValidationConflictKind as OptimisticValidationConflictKindForTest;

impl OptimisticValidation {
    pub(crate) fn acquire_commit_certification(
        &self,
        transaction: TransactionId,
        reads: &BTreeSet<GranuleId>,
        writes: &BTreeSet<GranuleId>,
    ) -> Result<OptimisticCertificationPermit> {
        self.commit_certification
            .acquire(transaction, reads, writes)
    }

    pub(crate) fn commit_certification_authority(&self) -> Arc<OptimisticCertificationAuthority> {
        self.commit_certification.clone()
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    pub(crate) fn acquire_mvcc_certification(
        &self,
        transaction: TransactionId,
        reads: &BTreeSet<GranuleId>,
        writes: &BTreeSet<GranuleId>,
    ) -> Result<OptimisticCertificationPermit> {
        self.acquire_commit_certification(transaction, reads, writes)
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    pub(crate) fn mvcc_certification_authority(&self) -> Arc<OptimisticCertificationAuthority> {
        self.commit_certification_authority()
    }

    pub(crate) fn record_read(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> Result<TransactionConflictAction> {
        Self::map_conflict_result(self.record_read_typed(transaction, granule, version))
    }

    pub(crate) fn acquire_write(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction> {
        Self::map_conflict_result(self.acquire_write_typed(transaction, granule, current_version))
    }

    pub(crate) fn validate_read(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        Self::map_conflict_result(self.validate_read_typed(transaction, granule, current_version))
    }

    pub(crate) fn validate_write(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        Self::map_conflict_result(self.validate_write_typed(transaction, granule, current_version))
    }

    pub(crate) fn refresh_read_version(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) {
        if let Some(version) = self.read_versions.get_mut(&(transaction, granule)) {
            *version = current_version;
        }
    }

    pub(crate) fn release_transaction(&mut self, transaction: TransactionId) {
        self.read_versions
            .retain(|(reader, _), _| *reader != transaction);
        self.write_versions
            .retain(|(writer, _), _| *writer != transaction);
    }

    pub(crate) fn release_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        self.release_transaction(transaction);
        Ok(())
    }

    pub(crate) fn owner_for_granule(&self, _granule: GranuleId) -> Option<TransactionId> {
        None
    }

    fn map_conflict_result<T>(
        result: core::result::Result<T, OptimisticValidationConflictKind>,
    ) -> Result<T> {
        match result {
            Ok(value) => Ok(value),
            Err(kind) => bail!(kind.message()),
        }
    }

    fn record_read_typed(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> core::result::Result<TransactionConflictAction, OptimisticValidationConflictKind> {
        match self.read_versions.entry((transaction, granule)) {
            alloc::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(version);
            }
            alloc::collections::btree_map::Entry::Occupied(entry) => {
                if *entry.get() != version {
                    return Err(OptimisticValidationConflictKind::ReadVersionMismatch);
                }
            }
        }

        Ok(TransactionConflictAction::Continue)
    }

    fn acquire_write_typed(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<TransactionConflictAction, OptimisticValidationConflictKind> {
        match self.write_versions.entry((transaction, granule)) {
            alloc::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(current_version);
            }
            alloc::collections::btree_map::Entry::Occupied(entry) => {
                if *entry.get() != current_version {
                    return Err(OptimisticValidationConflictKind::WriteVersionMismatch);
                }
            }
        }

        Ok(TransactionConflictAction::Continue)
    }

    fn validate_read_typed(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), OptimisticValidationConflictKind> {
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(OptimisticValidationConflictKind::ReadVersionMismatch);
        }

        Ok(())
    }

    fn validate_write_typed(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), OptimisticValidationConflictKind> {
        if self.write_versions.get(&(transaction, granule)).copied() != Some(current_version) {
            return Err(OptimisticValidationConflictKind::WriteVersionMismatch);
        }

        Ok(())
    }
}

#[cfg(test)]
impl OptimisticValidation {
    pub(crate) fn record_read_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> Result<()> {
        match self.record_read(transaction, granule, version)? {
            TransactionConflictAction::Continue => Ok(()),
            TransactionConflictAction::AbortOther(owner) => bail!(
                "optimistic-validation should not abort another transaction: {:?}",
                owner
            ),
            TransactionConflictAction::WouldWait(owner) => {
                bail!("optimistic-validation should not wait on owner {:?}", owner)
            }
        }
    }

    pub(crate) fn acquire_write_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        match self.acquire_write(transaction, granule, current_version)? {
            TransactionConflictAction::Continue => Ok(()),
            TransactionConflictAction::AbortOther(owner) => bail!(
                "optimistic-validation should not abort another transaction: {:?}",
                owner
            ),
            TransactionConflictAction::WouldWait(owner) => {
                bail!("optimistic-validation should not wait on owner {:?}", owner)
            }
        }
    }

    pub(crate) fn validate_write_result_for_test(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), OptimisticValidationConflictKindForTest> {
        self.validate_write_typed(transaction, granule, current_version)
    }
}

impl TransactionConcurrencyControl for OptimisticValidation {
    fn acquire_granule_read(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction> {
        self.record_read(transaction, granule, current_version)
    }

    fn acquire_granule_write(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction> {
        self.acquire_write(transaction, granule, current_version)
    }

    fn validate_read(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        OptimisticValidation::validate_read(self, transaction, granule, current_version)
    }

    fn validate_write(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        OptimisticValidation::validate_write(self, transaction, granule, current_version)
    }

    fn refresh_read_version(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) {
        OptimisticValidation::refresh_read_version(self, transaction, granule, current_version);
    }

    fn release_transaction(&mut self, transaction: TransactionId) {
        OptimisticValidation::release_transaction(self, transaction);
    }

    fn release_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        OptimisticValidation::release_transaction_result(self, transaction)
    }

    fn owner_for_granule(&self, granule: GranuleId) -> Option<TransactionId> {
        OptimisticValidation::owner_for_granule(self, granule)
    }

    fn read_granules_for_transaction(&self, transaction: TransactionId) -> Vec<GranuleId> {
        self.read_versions
            .keys()
            .filter_map(|(reader, granule)| (*reader == transaction).then_some(*granule))
            .collect()
    }

    fn write_granules_for_transaction(&self, transaction: TransactionId) -> Vec<GranuleId> {
        self.write_versions
            .keys()
            .filter_map(|(writer, granule)| (*writer == transaction).then_some(*granule))
            .collect()
    }

    #[cfg(test)]
    fn remove_write_version_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
    ) -> bool {
        self.write_versions
            .remove(&(transaction, granule))
            .is_some()
    }
}
