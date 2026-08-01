use crate::prelude::*;
use alloc::collections::BTreeMap;

use super::super::{GranuleId, TransactionId};
use super::{
    TransactionConcurrencyControl, TransactionConflictAction, TransactionTimestamp, is_older,
    timestamp_for_transaction,
};

#[derive(Debug, Default)]
pub(crate) struct TimestampOrdering {
    pub(crate) read_timestamps: BTreeMap<GranuleId, TransactionTimestamp>,
    pub(crate) write_timestamps: BTreeMap<GranuleId, TransactionTimestamp>,
    pub(crate) read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
    pub(crate) write_versions: BTreeMap<(TransactionId, GranuleId), u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TimestampOrderingConflictKind {
    ReadWriteTimestampTooNew,
    ReadVersionMismatch,
    WriteReadTimestampTooNew,
    WriteWriteTimestampTooNew,
    WriteVersionMismatch,
}

impl TimestampOrderingConflictKind {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::ReadWriteTimestampTooNew => {
                "transaction read conflict: timestamp ordering write timestamp too new"
            }
            Self::ReadVersionMismatch => {
                "transaction read conflict: timestamp ordering read version changed"
            }
            Self::WriteReadTimestampTooNew => {
                "transaction write conflict: timestamp ordering read timestamp too new"
            }
            Self::WriteWriteTimestampTooNew => {
                "transaction write conflict: timestamp ordering write timestamp too new"
            }
            Self::WriteVersionMismatch => {
                "transaction write conflict: timestamp ordering write version changed"
            }
        }
    }
}

#[cfg(test)]
pub(crate) use self::TimestampOrderingConflictKind as TimestampOrderingConflictKindForTest;

impl TimestampOrdering {
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

    pub(crate) fn owner_for_granule(&self, _granule: GranuleId) -> Option<TransactionId> {
        None
    }

    pub(crate) fn commit_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        let timestamp = timestamp_for_transaction(transaction);
        let granules: Vec<_> = self
            .write_versions
            .keys()
            .filter_map(|(writer, granule)| (*writer == transaction).then_some(*granule))
            .collect();
        for granule in granules {
            match self.write_timestamps.entry(granule) {
                alloc::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(timestamp);
                }
                alloc::collections::btree_map::Entry::Occupied(mut entry) => {
                    if *entry.get() < timestamp {
                        entry.insert(timestamp);
                    }
                }
            }
        }
        Ok(())
    }

    fn map_conflict_result<T>(
        result: core::result::Result<T, TimestampOrderingConflictKind>,
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
    ) -> core::result::Result<TransactionConflictAction, TimestampOrderingConflictKind> {
        if self
            .write_timestamps
            .get(&granule)
            .is_some_and(|write_timestamp| is_older(transaction, *write_timestamp))
        {
            return Err(TimestampOrderingConflictKind::ReadWriteTimestampTooNew);
        }

        let transaction_timestamp = timestamp_for_transaction(transaction);
        match self.read_timestamps.entry(granule) {
            alloc::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(transaction_timestamp);
            }
            alloc::collections::btree_map::Entry::Occupied(mut entry) => {
                if *entry.get() < transaction_timestamp {
                    entry.insert(transaction_timestamp);
                }
            }
        }

        match self.read_versions.entry((transaction, granule)) {
            alloc::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(version);
            }
            alloc::collections::btree_map::Entry::Occupied(entry) => {
                if *entry.get() != version {
                    return Err(TimestampOrderingConflictKind::ReadVersionMismatch);
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
    ) -> core::result::Result<TransactionConflictAction, TimestampOrderingConflictKind> {
        if self
            .read_timestamps
            .get(&granule)
            .is_some_and(|read_timestamp| is_older(transaction, *read_timestamp))
        {
            return Err(TimestampOrderingConflictKind::WriteReadTimestampTooNew);
        }
        if self
            .write_timestamps
            .get(&granule)
            .is_some_and(|write_timestamp| is_older(transaction, *write_timestamp))
        {
            return Err(TimestampOrderingConflictKind::WriteWriteTimestampTooNew);
        }

        match self.write_versions.entry((transaction, granule)) {
            alloc::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(current_version);
            }
            alloc::collections::btree_map::Entry::Occupied(entry) => {
                if *entry.get() != current_version {
                    return Err(TimestampOrderingConflictKind::WriteVersionMismatch);
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
    ) -> core::result::Result<(), TimestampOrderingConflictKind> {
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(TimestampOrderingConflictKind::ReadVersionMismatch);
        }

        Ok(())
    }

    fn validate_write_typed(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), TimestampOrderingConflictKind> {
        if self.write_versions.get(&(transaction, granule)).copied() != Some(current_version) {
            return Err(TimestampOrderingConflictKind::WriteVersionMismatch);
        }

        Ok(())
    }
}

#[cfg(test)]
impl TimestampOrdering {
    pub(crate) fn record_read_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> Result<()> {
        match self.record_read(transaction, granule, version)? {
            TransactionConflictAction::Continue => Ok(()),
            TransactionConflictAction::AbortOther(owner) => bail!(
                "timestamp-ordering should not abort another transaction: {:?}",
                owner
            ),
            TransactionConflictAction::WouldWait(owner) => {
                bail!("timestamp-ordering should not wait on owner {:?}", owner)
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
                "timestamp-ordering should not abort another transaction: {:?}",
                owner
            ),
            TransactionConflictAction::WouldWait(owner) => {
                bail!("timestamp-ordering should not wait on owner {:?}", owner)
            }
        }
    }

    pub(crate) fn acquire_write_result_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<TransactionConflictAction, TimestampOrderingConflictKindForTest> {
        self.acquire_write_typed(transaction, granule, current_version)
    }

    pub(crate) fn write_timestamp_for_test(
        &self,
        granule: GranuleId,
    ) -> Option<TransactionTimestamp> {
        self.write_timestamps.get(&granule).copied()
    }
}

impl TransactionConcurrencyControl for TimestampOrdering {
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
        TimestampOrdering::validate_read(self, transaction, granule, current_version)
    }

    fn validate_write(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        TimestampOrdering::validate_write(self, transaction, granule, current_version)
    }

    fn refresh_read_version(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) {
        TimestampOrdering::refresh_read_version(self, transaction, granule, current_version);
    }

    fn release_transaction(&mut self, transaction: TransactionId) {
        TimestampOrdering::release_transaction(self, transaction);
    }

    fn owner_for_granule(&self, granule: GranuleId) -> Option<TransactionId> {
        TimestampOrdering::owner_for_granule(self, granule)
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

    fn commit_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        TimestampOrdering::commit_transaction_result(self, transaction)
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
