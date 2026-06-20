use crate::prelude::*;
use alloc::collections::{BTreeMap, BTreeSet};

use super::super::{GranuleId, TransactionId};
use super::{TransactionConcurrencyControl, TransactionConflictAction};

#[derive(Debug, Default)]
pub(crate) struct StrictTwoPhaseLocking {
    pub(crate) readers: BTreeMap<GranuleId, BTreeSet<TransactionId>>,
    pub(crate) writers: BTreeMap<GranuleId, TransactionId>,
    pub(crate) read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StrictTwoPhaseLockingConflictKind {
    ReadOwnedByOther,
    ReadVersionMismatch,
    WriteOwnedByOther,
    WriteReadLockedByOther,
    WriteVersionMismatch,
}

impl StrictTwoPhaseLockingConflictKind {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::ReadOwnedByOther => {
                "transaction read conflict: granule is owned by another transaction"
            }
            Self::ReadVersionMismatch => {
                "transaction read conflict: optimistic read version changed"
            }
            Self::WriteOwnedByOther => {
                "transaction write conflict: granule is owned by another transaction"
            }
            Self::WriteReadLockedByOther => {
                "transaction write conflict: granule is read-locked by another transaction"
            }
            Self::WriteVersionMismatch => {
                "transaction write conflict: optimistic read version changed"
            }
        }
    }
}

#[cfg(test)]
pub(crate) use self::StrictTwoPhaseLockingConflictKind as StrictTwoPhaseLockingConflictKindForTest;

impl StrictTwoPhaseLocking {
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
        _current_version: u64,
    ) -> Result<()> {
        Self::map_conflict_result(self.validate_write_typed(transaction, granule))
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
        self.writers.retain(|_, owner| *owner != transaction);
        self.readers.retain(|_, readers| {
            readers.remove(&transaction);
            !readers.is_empty()
        });
        self.read_versions
            .retain(|(reader, _), _| *reader != transaction);
    }

    pub(crate) fn release_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        self.release_transaction(transaction);
        Ok(())
    }

    pub(crate) fn owner_for_granule(&self, granule: GranuleId) -> Option<TransactionId> {
        self.writers.get(&granule).copied()
    }

    fn map_conflict_result<T>(
        result: core::result::Result<T, StrictTwoPhaseLockingConflictKind>,
    ) -> Result<T> {
        match result {
            Ok(value) => Ok(value),
            Err(kind) => bail!(kind.message()),
        }
    }

    fn has_other_reader(&self, transaction: TransactionId, granule: GranuleId) -> bool {
        self.readers
            .get(&granule)
            .is_some_and(|readers| readers.iter().any(|reader| *reader != transaction))
    }

    fn record_read_typed(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> core::result::Result<TransactionConflictAction, StrictTwoPhaseLockingConflictKind> {
        if self
            .writers
            .get(&granule)
            .is_some_and(|writer| *writer != transaction)
        {
            return Err(StrictTwoPhaseLockingConflictKind::ReadOwnedByOther);
        }
        match self.read_versions.entry((transaction, granule)) {
            alloc::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(version);
            }
            alloc::collections::btree_map::Entry::Occupied(entry) => {
                if *entry.get() != version {
                    return Err(StrictTwoPhaseLockingConflictKind::ReadVersionMismatch);
                }
            }
        }
        self.readers.entry(granule).or_default().insert(transaction);
        Ok(TransactionConflictAction::Continue)
    }

    fn acquire_write_typed(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<TransactionConflictAction, StrictTwoPhaseLockingConflictKind> {
        if self
            .writers
            .get(&granule)
            .is_some_and(|writer| *writer != transaction)
        {
            return Err(StrictTwoPhaseLockingConflictKind::WriteOwnedByOther);
        }
        if self.has_other_reader(transaction, granule) {
            return Err(StrictTwoPhaseLockingConflictKind::WriteReadLockedByOther);
        }
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(StrictTwoPhaseLockingConflictKind::WriteVersionMismatch);
        }

        self.writers.insert(granule, transaction);
        self.readers.entry(granule).or_default().insert(transaction);
        self.read_versions
            .entry((transaction, granule))
            .or_insert(current_version);
        Ok(TransactionConflictAction::Continue)
    }

    fn validate_read_typed(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), StrictTwoPhaseLockingConflictKind> {
        if self
            .writers
            .get(&granule)
            .is_some_and(|writer| *writer != transaction)
        {
            return Err(StrictTwoPhaseLockingConflictKind::ReadOwnedByOther);
        }
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(StrictTwoPhaseLockingConflictKind::ReadVersionMismatch);
        }

        Ok(())
    }

    fn validate_write_typed(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
    ) -> core::result::Result<(), StrictTwoPhaseLockingConflictKind> {
        if self.writers.get(&granule).copied() != Some(transaction) {
            return Err(StrictTwoPhaseLockingConflictKind::WriteOwnedByOther);
        }
        Ok(())
    }
}

#[cfg(test)]
impl StrictTwoPhaseLocking {
    pub(crate) fn record_read_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> Result<()> {
        match self.record_read(transaction, granule, version)? {
            TransactionConflictAction::Continue => Ok(()),
            TransactionConflictAction::AbortOther(owner) => {
                bail!(
                    "strict-2pl should not abort another transaction: {:?}",
                    owner
                )
            }
            TransactionConflictAction::WouldWait(owner) => {
                bail!("strict-2pl should not wait on owner {:?}", owner)
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
            TransactionConflictAction::AbortOther(owner) => {
                bail!(
                    "strict-2pl should not abort another transaction: {:?}",
                    owner
                )
            }
            TransactionConflictAction::WouldWait(owner) => {
                bail!("strict-2pl should not wait on owner {:?}", owner)
            }
        }
    }

    pub(crate) fn record_read_result_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> core::result::Result<TransactionConflictAction, StrictTwoPhaseLockingConflictKindForTest>
    {
        self.record_read_typed(transaction, granule, version)
    }

    pub(crate) fn acquire_write_result_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<TransactionConflictAction, StrictTwoPhaseLockingConflictKindForTest>
    {
        self.acquire_write_typed(transaction, granule, current_version)
    }

    pub(crate) fn validate_read_result_for_test(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), StrictTwoPhaseLockingConflictKindForTest> {
        self.validate_read_typed(transaction, granule, current_version)
    }

    pub(crate) fn abort_for_test(&mut self, transaction: TransactionId) {
        self.release_transaction(transaction);
    }

    pub(crate) fn owner_for_test(&self, granule: GranuleId) -> Option<TransactionId> {
        self.writers.get(&granule).copied()
    }
}

impl TransactionConcurrencyControl for StrictTwoPhaseLocking {
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
        StrictTwoPhaseLocking::validate_read(self, transaction, granule, current_version)
    }

    fn validate_write(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        StrictTwoPhaseLocking::validate_write(self, transaction, granule, current_version)
    }

    fn refresh_read_version(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) {
        StrictTwoPhaseLocking::refresh_read_version(self, transaction, granule, current_version);
    }

    fn release_transaction(&mut self, transaction: TransactionId) {
        StrictTwoPhaseLocking::release_transaction(self, transaction);
    }

    fn release_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        StrictTwoPhaseLocking::release_transaction_result(self, transaction)
    }

    fn owner_for_granule(&self, granule: GranuleId) -> Option<TransactionId> {
        StrictTwoPhaseLocking::owner_for_granule(self, granule)
    }

    fn read_granules_for_transaction(&self, transaction: TransactionId) -> Vec<GranuleId> {
        self.read_versions
            .keys()
            .filter_map(|(reader, granule)| (*reader == transaction).then_some(*granule))
            .collect()
    }

    fn write_granules_for_transaction(&self, transaction: TransactionId) -> Vec<GranuleId> {
        self.writers
            .iter()
            .filter_map(|(granule, owner)| (*owner == transaction).then_some(*granule))
            .collect()
    }

    #[cfg(test)]
    fn remove_owner_for_granule_for_test(&mut self, granule: GranuleId) -> Option<TransactionId> {
        self.writers.remove(&granule)
    }
}
