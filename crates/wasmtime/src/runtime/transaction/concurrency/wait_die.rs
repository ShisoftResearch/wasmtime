use crate::prelude::*;
use alloc::collections::BTreeMap;

use super::super::{GranuleId, TransactionId};
use super::{TransactionConcurrencyControl, TransactionConflictAction, is_older};

#[derive(Debug, Default)]
pub(crate) struct WaitDie {
    pub(crate) owners: BTreeMap<GranuleId, TransactionId>,
    pub(crate) read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WaitDieConflictKind {
    ReadOwnedByOther,
    ReadVersionMismatch,
    WriteOwnedByOther,
    WriteVersionMismatch,
}

impl WaitDieConflictKind {
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
pub(crate) use self::WaitDieConflictKind as WaitDieConflictKindForTest;

impl WaitDie {
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

    fn map_conflict_result<T>(result: core::result::Result<T, WaitDieConflictKind>) -> Result<T> {
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
    ) -> core::result::Result<TransactionConflictAction, WaitDieConflictKind> {
        let action = self.resolve_writer_conflict_typed(transaction, granule, false)?;
        if action.would_wait_owner().is_some() {
            return Ok(action);
        }

        match self.read_versions.entry((transaction, granule)) {
            alloc::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(version);
            }
            alloc::collections::btree_map::Entry::Occupied(entry) => {
                if *entry.get() != version {
                    return Err(WaitDieConflictKind::ReadVersionMismatch);
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
    ) -> core::result::Result<TransactionConflictAction, WaitDieConflictKind> {
        let action = self.resolve_writer_conflict_typed(transaction, granule, true)?;
        if action.would_wait_owner().is_some() {
            return Ok(action);
        }
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(WaitDieConflictKind::WriteVersionMismatch);
        }

        self.owners.insert(granule, transaction);
        Ok(TransactionConflictAction::Continue)
    }

    fn validate_read_typed(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), WaitDieConflictKind> {
        if self
            .owners
            .get(&granule)
            .is_some_and(|owner| *owner != transaction)
        {
            return Err(WaitDieConflictKind::ReadOwnedByOther);
        }
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(WaitDieConflictKind::ReadVersionMismatch);
        }

        Ok(())
    }

    fn validate_write_typed(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
    ) -> core::result::Result<(), WaitDieConflictKind> {
        if self.owners.get(&granule).copied() != Some(transaction) {
            return Err(WaitDieConflictKind::WriteOwnedByOther);
        }
        Ok(())
    }

    fn resolve_writer_conflict_typed(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        is_write: bool,
    ) -> core::result::Result<TransactionConflictAction, WaitDieConflictKind> {
        let Some(owner) = self.owners.get(&granule).copied() else {
            return Ok(TransactionConflictAction::Continue);
        };
        if owner == transaction {
            return Ok(TransactionConflictAction::Continue);
        }
        if is_older(transaction, owner) {
            return Ok(TransactionConflictAction::WouldWait(owner));
        }
        Err(if is_write {
            WaitDieConflictKind::WriteOwnedByOther
        } else {
            WaitDieConflictKind::ReadOwnedByOther
        })
    }
}

#[cfg(test)]
impl WaitDie {
    pub(crate) fn record_read_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> Result<()> {
        match self.record_read(transaction, granule, version)? {
            TransactionConflictAction::Continue => Ok(()),
            TransactionConflictAction::WouldWait(owner) => {
                bail!("wait-die test setup would wait on owner {:?}", owner)
            }
            TransactionConflictAction::AbortOther(owner) => {
                bail!("wait-die should not abort another transaction: {:?}", owner)
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
            TransactionConflictAction::WouldWait(owner) => {
                bail!("wait-die test setup would wait on owner {:?}", owner)
            }
            TransactionConflictAction::AbortOther(owner) => {
                bail!("wait-die should not abort another transaction: {:?}", owner)
            }
        }
    }

    pub(crate) fn record_read_result_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> core::result::Result<TransactionConflictAction, WaitDieConflictKindForTest> {
        self.record_read_typed(transaction, granule, version)
    }

    pub(crate) fn acquire_write_result_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<TransactionConflictAction, WaitDieConflictKindForTest> {
        self.acquire_write_typed(transaction, granule, current_version)
    }

    pub(crate) fn validate_read_result_for_test(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), WaitDieConflictKindForTest> {
        self.validate_read_typed(transaction, granule, current_version)
    }

    pub(crate) fn abort_for_test(&mut self, transaction: TransactionId) {
        self.release_transaction(transaction);
    }

    pub(crate) fn owner_for_test(&self, granule: GranuleId) -> Option<TransactionId> {
        self.owners.get(&granule).copied()
    }
}

impl TransactionConcurrencyControl for WaitDie {
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
        WaitDie::validate_read(self, transaction, granule, current_version)
    }

    fn validate_write(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        WaitDie::validate_write(self, transaction, granule, current_version)
    }

    fn refresh_read_version(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) {
        WaitDie::refresh_read_version(self, transaction, granule, current_version);
    }

    fn release_transaction(&mut self, transaction: TransactionId) {
        WaitDie::release_transaction(self, transaction);
    }

    fn release_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        WaitDie::release_transaction_result(self, transaction)
    }

    fn owner_for_granule(&self, granule: GranuleId) -> Option<TransactionId> {
        WaitDie::owner_for_granule(self, granule)
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

    #[cfg(test)]
    fn remove_owner_for_granule_for_test(&mut self, granule: GranuleId) -> Option<TransactionId> {
        self.owners.remove(&granule)
    }
}
