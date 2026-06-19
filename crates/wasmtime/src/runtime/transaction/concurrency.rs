use crate::prelude::*;
use alloc::collections::BTreeMap;

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
}

#[derive(Debug)]
pub(crate) enum ConcurrencyControlState {
    LockBased(LockBased),
    NoWaitAbort(NoWaitAbort),
    WoundWait(WoundWait),
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
            ConcurrencyControl::WoundWait => Self::WoundWait(WoundWait::default()),
        }
    }

    fn policy(&self) -> &dyn TransactionConcurrencyControl {
        match self {
            Self::LockBased(policy) => policy,
            Self::NoWaitAbort(policy) => policy,
            Self::WoundWait(policy) => policy,
        }
    }

    fn policy_mut(&mut self) -> &mut dyn TransactionConcurrencyControl {
        match self {
            Self::LockBased(policy) => policy,
            Self::NoWaitAbort(policy) => policy,
            Self::WoundWait(policy) => policy,
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

    #[cfg(test)]
    pub(crate) fn remove_owner_for_granule_for_test(
        &mut self,
        granule: GranuleId,
    ) -> Option<TransactionId> {
        match self {
            Self::LockBased(policy) => policy.owners.remove(&granule),
            Self::NoWaitAbort(policy) => policy.owners.remove(&granule),
            Self::WoundWait(policy) => policy.owners.remove(&granule),
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
}
