use crate::prelude::*;
use alloc::collections::{BTreeMap, BTreeSet};

use super::{ConcurrencyControl, GranuleId, TransactionId};

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
}

#[derive(Debug)]
pub(crate) enum ConcurrencyControlState {
    LockBased(LockBased),
    NoWaitAbort(NoWaitAbort),
    StrictTwoPhaseLocking(StrictTwoPhaseLocking),
    WoundWait(WoundWait),
    WaitDie(WaitDie),
    OptimisticValidation(OptimisticValidation),
}

impl Default for ConcurrencyControlState {
    fn default() -> Self {
        Self::for_config(ConcurrencyControl::default_for_build())
    }
}

impl ConcurrencyControlState {
    pub(crate) fn for_config(policy: ConcurrencyControl) -> Self {
        match policy {
            ConcurrencyControl::LockBased => Self::LockBased(LockBased::default()),
            ConcurrencyControl::NoWaitAbort => Self::NoWaitAbort(NoWaitAbort::default()),
            ConcurrencyControl::StrictTwoPhaseLocking => {
                Self::StrictTwoPhaseLocking(StrictTwoPhaseLocking::default())
            }
            ConcurrencyControl::WoundWait => Self::WoundWait(WoundWait::default()),
            ConcurrencyControl::WaitDie => Self::WaitDie(WaitDie::default()),
            ConcurrencyControl::OptimisticValidation => {
                Self::OptimisticValidation(OptimisticValidation::default())
            }
        }
    }

    fn policy(&self) -> &dyn TransactionConcurrencyControl {
        match self {
            Self::LockBased(policy) => policy,
            Self::NoWaitAbort(policy) => policy,
            Self::StrictTwoPhaseLocking(policy) => policy,
            Self::WoundWait(policy) => policy,
            Self::WaitDie(policy) => policy,
            Self::OptimisticValidation(policy) => policy,
        }
    }

    fn policy_mut(&mut self) -> &mut dyn TransactionConcurrencyControl {
        match self {
            Self::LockBased(policy) => policy,
            Self::NoWaitAbort(policy) => policy,
            Self::StrictTwoPhaseLocking(policy) => policy,
            Self::WoundWait(policy) => policy,
            Self::WaitDie(policy) => policy,
            Self::OptimisticValidation(policy) => policy,
        }
    }

    pub(crate) fn acquire_granule_read(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction> {
        self.policy_mut()
            .acquire_granule_read(transaction, granule, current_version)
    }

    pub(crate) fn acquire_granule_write(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction> {
        self.policy_mut()
            .acquire_granule_write(transaction, granule, current_version)
    }

    pub(crate) fn validate_read(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.policy()
            .validate_read(transaction, granule, current_version)
    }

    pub(crate) fn validate_write(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.policy()
            .validate_write(transaction, granule, current_version)
    }

    pub(crate) fn refresh_read_version(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) {
        self.policy_mut()
            .refresh_read_version(transaction, granule, current_version);
    }

    pub(crate) fn release_transaction(&mut self, transaction: TransactionId) {
        self.policy_mut().release_transaction(transaction);
    }

    pub(crate) fn release_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        self.policy_mut().release_transaction_result(transaction)
    }

    pub(crate) fn owner_for_granule(&self, granule: GranuleId) -> Option<TransactionId> {
        self.policy().owner_for_granule(granule)
    }

    pub(crate) fn read_granules_for_transaction(
        &self,
        transaction: TransactionId,
    ) -> Vec<GranuleId> {
        self.policy().read_granules_for_transaction(transaction)
    }

    pub(crate) fn write_granules_for_transaction(
        &self,
        transaction: TransactionId,
    ) -> Vec<GranuleId> {
        self.policy().write_granules_for_transaction(transaction)
    }

    pub(crate) fn commit_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        self.policy_mut().commit_transaction_result(transaction)
    }

    #[cfg(test)]
    pub(crate) fn remove_owner_for_granule_for_test(
        &mut self,
        granule: GranuleId,
    ) -> Option<TransactionId> {
        match self {
            Self::LockBased(policy) => policy.owners.remove(&granule),
            Self::NoWaitAbort(policy) => policy.owners.remove(&granule),
            Self::StrictTwoPhaseLocking(policy) => policy.writers.remove(&granule),
            Self::WoundWait(policy) => policy.owners.remove(&granule),
            Self::WaitDie(policy) => policy.owners.remove(&granule),
            Self::OptimisticValidation(_) => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn committed_transactions_for_test(&self) -> Option<BTreeSet<TransactionId>> {
        match self {
            Self::LockBased(policy) => Some(policy.committed_transactions_for_test()),
            Self::NoWaitAbort(_)
            | Self::StrictTwoPhaseLocking(_)
            | Self::WoundWait(_)
            | Self::WaitDie(_)
            | Self::OptimisticValidation(_) => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn fail_commit_transaction_once_for_test(&mut self) {
        if let Self::LockBased(policy) = self {
            policy.fail_commit_transaction_once_for_test = true;
        }
    }
}

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
        let aborted = self.resolve_writer_conflict_typed(transaction, granule, false)?;

        match self.read_versions.entry((transaction, granule)) {
            alloc::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(version);
            }
            alloc::collections::btree_map::Entry::Occupied(entry) => {
                if *entry.get() != version {
                    return Err(LockBasedConflictKind::ReadVersionMismatch);
                }
            }
        }

        Ok(aborted)
    }

    fn acquire_write_typed(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<Option<TransactionId>, LockBasedConflictKind> {
        let aborted = self.resolve_writer_conflict_typed(transaction, granule, true)?;
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(LockBasedConflictKind::WriteVersionMismatch);
        }

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

    fn commit_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        #[cfg(test)]
        {
            self.committed_transactions_for_test.insert(transaction);
            if core::mem::take(&mut self.fail_commit_transaction_once_for_test) {
                bail!("injected commit transaction failure");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub(crate) struct NoWaitAbort {
    pub(crate) owners: BTreeMap<GranuleId, TransactionId>,
    pub(crate) read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NoWaitAbortConflictKind {
    ReadOwnedByOther,
    ReadVersionMismatch,
    WriteOwnedByOther,
    WriteVersionMismatch,
}

impl NoWaitAbortConflictKind {
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
pub(crate) use self::NoWaitAbortConflictKind as NoWaitAbortConflictKindForTest;

impl NoWaitAbort {
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

    fn map_conflict_result<T>(
        result: core::result::Result<T, NoWaitAbortConflictKind>,
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
    ) -> core::result::Result<Option<TransactionId>, NoWaitAbortConflictKind> {
        self.reject_other_writer(transaction, granule, false)?;

        match self.read_versions.entry((transaction, granule)) {
            alloc::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(version);
            }
            alloc::collections::btree_map::Entry::Occupied(entry) => {
                if *entry.get() != version {
                    return Err(NoWaitAbortConflictKind::ReadVersionMismatch);
                }
            }
        }

        Ok(None)
    }

    fn acquire_write_typed(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<Option<TransactionId>, NoWaitAbortConflictKind> {
        self.reject_other_writer(transaction, granule, true)?;
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(NoWaitAbortConflictKind::WriteVersionMismatch);
        }

        self.owners.insert(granule, transaction);
        Ok(None)
    }

    fn validate_read_typed(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), NoWaitAbortConflictKind> {
        if self
            .owners
            .get(&granule)
            .is_some_and(|owner| *owner != transaction)
        {
            return Err(NoWaitAbortConflictKind::ReadOwnedByOther);
        }
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(NoWaitAbortConflictKind::ReadVersionMismatch);
        }

        Ok(())
    }

    fn validate_write_typed(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
    ) -> core::result::Result<(), NoWaitAbortConflictKind> {
        if self.owners.get(&granule).copied() != Some(transaction) {
            return Err(NoWaitAbortConflictKind::WriteOwnedByOther);
        }
        Ok(())
    }

    fn reject_other_writer(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        is_write: bool,
    ) -> core::result::Result<(), NoWaitAbortConflictKind> {
        if self
            .owners
            .get(&granule)
            .is_some_and(|owner| *owner != transaction)
        {
            return Err(if is_write {
                NoWaitAbortConflictKind::WriteOwnedByOther
            } else {
                NoWaitAbortConflictKind::ReadOwnedByOther
            });
        }
        Ok(())
    }
}

#[cfg(test)]
impl NoWaitAbort {
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

    pub(crate) fn record_read_result_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> core::result::Result<(), NoWaitAbortConflictKindForTest> {
        self.record_read_typed(transaction, granule, version)
            .map(|_| ())
    }

    pub(crate) fn acquire_write_result_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), NoWaitAbortConflictKindForTest> {
        self.acquire_write_typed(transaction, granule, current_version)
            .map(|_| ())
    }

    pub(crate) fn validate_read_result_for_test(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), NoWaitAbortConflictKindForTest> {
        self.validate_read_typed(transaction, granule, current_version)
    }

    pub(crate) fn abort_for_test(&mut self, transaction: TransactionId) {
        self.release_transaction(transaction);
    }

    pub(crate) fn owner_for_test(&self, granule: GranuleId) -> Option<TransactionId> {
        self.owners.get(&granule).copied()
    }
}

impl TransactionConcurrencyControl for NoWaitAbort {
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
        NoWaitAbort::validate_read(self, transaction, granule, current_version)
    }

    fn validate_write(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        NoWaitAbort::validate_write(self, transaction, granule, current_version)
    }

    fn refresh_read_version(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) {
        NoWaitAbort::refresh_read_version(self, transaction, granule, current_version);
    }

    fn release_transaction(&mut self, transaction: TransactionId) {
        NoWaitAbort::release_transaction(self, transaction);
    }

    fn release_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        NoWaitAbort::release_transaction_result(self, transaction)
    }

    fn owner_for_granule(&self, granule: GranuleId) -> Option<TransactionId> {
        NoWaitAbort::owner_for_granule(self, granule)
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
}

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
}

#[derive(Debug, Default)]
pub(crate) struct WoundWait {
    pub(crate) owners: BTreeMap<GranuleId, TransactionId>,
    pub(crate) read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WoundWaitConflictKind {
    ReadOwnedByOther,
    ReadVersionMismatch,
    WriteOwnedByOther,
    WriteVersionMismatch,
}

impl WoundWaitConflictKind {
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
pub(crate) use self::WoundWaitConflictKind as WoundWaitConflictKindForTest;

impl WoundWait {
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

    fn map_conflict_result<T>(result: core::result::Result<T, WoundWaitConflictKind>) -> Result<T> {
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
    ) -> core::result::Result<Option<TransactionId>, WoundWaitConflictKind> {
        let wounded = self.resolve_writer_conflict_typed(transaction, granule, false)?;

        match self.read_versions.entry((transaction, granule)) {
            alloc::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(version);
            }
            alloc::collections::btree_map::Entry::Occupied(entry) => {
                if *entry.get() != version {
                    return Err(WoundWaitConflictKind::ReadVersionMismatch);
                }
            }
        }

        Ok(wounded)
    }

    fn acquire_write_typed(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<Option<TransactionId>, WoundWaitConflictKind> {
        let wounded = self.resolve_writer_conflict_typed(transaction, granule, true)?;
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(WoundWaitConflictKind::WriteVersionMismatch);
        }

        self.owners.insert(granule, transaction);
        Ok(wounded)
    }

    fn validate_read_typed(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), WoundWaitConflictKind> {
        if self
            .owners
            .get(&granule)
            .is_some_and(|owner| *owner != transaction)
        {
            return Err(WoundWaitConflictKind::ReadOwnedByOther);
        }
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(WoundWaitConflictKind::ReadVersionMismatch);
        }

        Ok(())
    }

    fn validate_write_typed(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
    ) -> core::result::Result<(), WoundWaitConflictKind> {
        if self.owners.get(&granule).copied() != Some(transaction) {
            return Err(WoundWaitConflictKind::WriteOwnedByOther);
        }
        Ok(())
    }

    fn resolve_writer_conflict_typed(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        is_write: bool,
    ) -> core::result::Result<Option<TransactionId>, WoundWaitConflictKind> {
        let Some(owner) = self.owners.get(&granule).copied() else {
            return Ok(None);
        };
        if owner == transaction {
            return Ok(None);
        }
        if is_older(transaction, owner) {
            self.release_transaction(owner);
            return Ok(Some(owner));
        }
        Err(if is_write {
            WoundWaitConflictKind::WriteOwnedByOther
        } else {
            WoundWaitConflictKind::ReadOwnedByOther
        })
    }
}

#[cfg(test)]
impl WoundWait {
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

    pub(crate) fn record_read_result_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> core::result::Result<(), WoundWaitConflictKindForTest> {
        self.record_read_typed(transaction, granule, version)
            .map(|_| ())
    }

    pub(crate) fn acquire_write_result_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), WoundWaitConflictKindForTest> {
        self.acquire_write_typed(transaction, granule, current_version)
            .map(|_| ())
    }

    pub(crate) fn validate_read_for_test(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.validate_read(transaction, granule, current_version)
    }

    pub(crate) fn validate_read_result_for_test(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), WoundWaitConflictKindForTest> {
        self.validate_read_typed(transaction, granule, current_version)
    }

    pub(crate) fn abort_for_test(&mut self, transaction: TransactionId) {
        self.release_transaction(transaction);
    }

    pub(crate) fn owner_for_test(&self, granule: GranuleId) -> Option<TransactionId> {
        self.owners.get(&granule).copied()
    }
}

impl TransactionConcurrencyControl for WoundWait {
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
        WoundWait::validate_read(self, transaction, granule, current_version)
    }

    fn validate_write(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        WoundWait::validate_write(self, transaction, granule, current_version)
    }

    fn refresh_read_version(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) {
        WoundWait::refresh_read_version(self, transaction, granule, current_version);
    }

    fn release_transaction(&mut self, transaction: TransactionId) {
        WoundWait::release_transaction(self, transaction);
    }

    fn release_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        WoundWait::release_transaction_result(self, transaction)
    }

    fn owner_for_granule(&self, granule: GranuleId) -> Option<TransactionId> {
        WoundWait::owner_for_granule(self, granule)
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
}

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
}

#[derive(Debug, Default)]
pub(crate) struct OptimisticValidation {
    pub(crate) read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
    pub(crate) write_versions: BTreeMap<(TransactionId, GranuleId), u64>,
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
}
