use crate::prelude::*;
use alloc::collections::BTreeMap;
#[cfg(test)]
use alloc::collections::BTreeSet;

use super::super::{GranuleId, TransactionId};
use super::{
    TransactionConcurrencyControl, TransactionConflictAction, action_from_aborted_transaction,
};

#[derive(Debug, Default)]
pub(crate) struct LockBased {
    // SHISOFT-TWASM-MOCK: not every non-object granule has durable version
    // metadata yet, so those granules still feed version `0` through the lock
    // manager.
    pub(crate) owners: BTreeMap<GranuleId, TransactionId>,
    pub(crate) read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
    #[cfg(test)]
    pub(crate) committed_transactions_for_test: BTreeSet<TransactionId>,
    #[cfg(test)]
    pub(crate) fail_commit_transaction_once_for_test: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LockBasedConflictKind {
    ReadOwnedByOther,
    ReadVersionMismatch,
    WriteOwnedByOther,
    WriteVersionMismatch,
}

impl LockBasedConflictKind {
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
            Self::WriteVersionMismatch => {
                "transaction write conflict: optimistic read version changed"
            }
        }
    }
}

#[cfg(test)]
pub(crate) use self::LockBasedConflictKind as LockBasedConflictKindForTest;

impl LockBased {
    pub(crate) fn record_read(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> Result<Option<TransactionId>> {
        Self::map_conflict_result(self.record_read_typed(transaction, granule, version))
    }

    pub(crate) fn acquire_write(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<Option<TransactionId>> {
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

    pub(crate) fn validate_transaction_reads<F>(
        &self,
        transaction: TransactionId,
        mut current_version_fn: F,
    ) -> Result<()>
    where
        F: FnMut(GranuleId) -> Result<u64>,
    {
        for ((reader, granule), _) in self.read_versions.iter() {
            if *reader != transaction {
                continue;
            }
            let current_version = current_version_fn(*granule)?;
            self.validate_read(transaction, *granule, current_version)?;
        }

        Ok(())
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
        self.owners.retain(|_, owner| *owner != transaction);
        self.read_versions
            .retain(|(reader, _), _| *reader != transaction);
    }

    pub(crate) fn release_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        self.release_transaction(transaction);
        Ok(())
    }

    pub(crate) fn owner_for_granule(&self, granule: GranuleId) -> Option<TransactionId> {
        self.owners.get(&granule).copied()
    }

    fn map_conflict_result<T>(result: core::result::Result<T, LockBasedConflictKind>) -> Result<T> {
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
    ) -> core::result::Result<Option<TransactionId>, LockBasedConflictKind> {
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|recorded| *recorded != version)
        {
            return Err(LockBasedConflictKind::ReadVersionMismatch);
        }
        let aborted = self.resolve_writer_conflict_typed(transaction, granule, false)?;

        self.read_versions
            .entry((transaction, granule))
            .or_insert(version);

        Ok(aborted)
    }

    fn acquire_write_typed(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<Option<TransactionId>, LockBasedConflictKind> {
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(LockBasedConflictKind::WriteVersionMismatch);
        }
        let aborted = self.resolve_writer_conflict_typed(transaction, granule, true)?;

        self.owners.insert(granule, transaction);
        Ok(aborted)
    }

    fn validate_read_typed(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), LockBasedConflictKind> {
        if self
            .owners
            .get(&granule)
            .is_some_and(|owner| *owner != transaction)
        {
            return Err(LockBasedConflictKind::ReadOwnedByOther);
        }
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(LockBasedConflictKind::ReadVersionMismatch);
        }

        Ok(())
    }

    fn validate_write_typed(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
    ) -> core::result::Result<(), LockBasedConflictKind> {
        if self.owners.get(&granule).copied() != Some(transaction) {
            return Err(LockBasedConflictKind::WriteOwnedByOther);
        }
        Ok(())
    }

    fn resolve_writer_conflict_typed(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        is_write: bool,
    ) -> core::result::Result<Option<TransactionId>, LockBasedConflictKind> {
        let Some(owner) = self.owners.get(&granule).copied() else {
            return Ok(None);
        };
        if owner == transaction {
            return Ok(None);
        }
        if transaction < owner {
            self.release_transaction(owner);
            return Ok(Some(owner));
        }
        Err(if is_write {
            LockBasedConflictKind::WriteOwnedByOther
        } else {
            LockBasedConflictKind::ReadOwnedByOther
        })
    }
}

#[cfg(test)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct LockBasedSnapshotForTest {
    pub(crate) owners: BTreeMap<GranuleId, TransactionId>,
    pub(crate) read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
}

#[cfg(test)]
impl LockBased {
    pub(crate) fn clone_for_test(&self) -> Self {
        Self {
            owners: self.owners.clone(),
            read_versions: self.read_versions.clone(),
            committed_transactions_for_test: self.committed_transactions_for_test.clone(),
            fail_commit_transaction_once_for_test: self.fail_commit_transaction_once_for_test,
        }
    }

    pub(crate) fn snapshot_for_test(&self) -> LockBasedSnapshotForTest {
        LockBasedSnapshotForTest {
            owners: self.owners.clone(),
            read_versions: self.read_versions.clone(),
        }
    }

    pub(crate) fn record_read_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> Result<()> {
        self.record_read(transaction, granule, version)?;
        Ok(())
    }

    pub(crate) fn acquire_write_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.acquire_write(transaction, granule, current_version)?;
        Ok(())
    }

    pub(crate) fn acquire_memory_granule_read(
        &mut self,
        transaction: TransactionId,
        memory_index: u32,
        granule_index: u64,
    ) -> Result<()> {
        self.acquire_granule_read(
            transaction,
            GranuleId::TMemory {
                instance: None,
                memory_index,
                granule_index,
            },
            0,
        )?;
        Ok(())
    }

    pub(crate) fn acquire_memory_granule_write(
        &mut self,
        transaction: TransactionId,
        memory_index: u32,
        granule_index: u64,
    ) -> Result<()> {
        self.acquire_granule_write(
            transaction,
            GranuleId::TMemory {
                instance: None,
                memory_index,
                granule_index,
            },
            0,
        )?;
        Ok(())
    }

    pub(crate) fn validate_read_for_test(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.validate_read(transaction, granule, current_version)
    }

    pub(crate) fn record_read_result_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> core::result::Result<(), LockBasedConflictKindForTest> {
        self.record_read_typed(transaction, granule, version)
            .map(|_| ())
    }

    pub(crate) fn acquire_write_result_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), LockBasedConflictKindForTest> {
        self.acquire_write_typed(transaction, granule, current_version)
            .map(|_| ())
    }

    pub(crate) fn validate_read_result_for_test(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), LockBasedConflictKindForTest> {
        self.validate_read_typed(transaction, granule, current_version)
    }

    pub(crate) fn abort_for_test(&mut self, transaction: TransactionId) {
        self.release_transaction(transaction);
    }

    pub(crate) fn owner_for_test(&self, granule: GranuleId) -> Option<TransactionId> {
        self.owners.get(&granule).copied()
    }

    pub(crate) fn committed_transactions_for_test(&self) -> BTreeSet<TransactionId> {
        self.committed_transactions_for_test.clone()
    }
}

impl TransactionConcurrencyControl for LockBased {
    fn acquire_granule_read(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction> {
        self.record_read(transaction, granule, current_version)
            .map(action_from_aborted_transaction)
    }

    fn acquire_granule_write(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction> {
        self.acquire_write(transaction, granule, current_version)
            .map(action_from_aborted_transaction)
    }

    fn validate_read(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        LockBased::validate_read(self, transaction, granule, current_version)
    }

    fn validate_write(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        LockBased::validate_write(self, transaction, granule, current_version)
    }

    fn refresh_read_version(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) {
        LockBased::refresh_read_version(self, transaction, granule, current_version);
    }

    fn release_transaction(&mut self, transaction: TransactionId) {
        LockBased::release_transaction(self, transaction);
    }

    fn release_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        LockBased::release_transaction_result(self, transaction)
    }

    fn owner_for_granule(&self, granule: GranuleId) -> Option<TransactionId> {
        LockBased::owner_for_granule(self, granule)
    }

    fn read_granules_for_transaction(&self, transaction: TransactionId) -> Vec<GranuleId> {
        self.read_versions
            .keys()
            .filter_map(|(reader, granule)| (*reader == transaction).then_some(*granule))
            .collect()
    }

    fn write_granules_for_transaction(&self, transaction: TransactionId) -> Vec<GranuleId> {
        self.owners
            .iter()
            .filter_map(|(granule, owner)| (*owner == transaction).then_some(*granule))
            .collect()
    }

    fn commit_transaction_result(&mut self, _transaction: TransactionId) -> Result<()> {
        #[cfg(test)]
        {
            self.committed_transactions_for_test.insert(_transaction);
            if core::mem::take(&mut self.fail_commit_transaction_once_for_test) {
                bail!("injected commit transaction failure");
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn remove_owner_for_granule_for_test(&mut self, granule: GranuleId) -> Option<TransactionId> {
        self.owners.remove(&granule)
    }

    #[cfg(test)]
    fn committed_transactions_for_test(&self) -> Option<BTreeSet<TransactionId>> {
        Some(self.committed_transactions_for_test())
    }

    #[cfg(test)]
    fn fail_commit_transaction_once_for_test(&mut self) {
        self.fail_commit_transaction_once_for_test = true;
    }
}
