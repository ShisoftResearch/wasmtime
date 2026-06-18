use crate::prelude::*;
use alloc::collections::{BTreeMap, BTreeSet};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::ThreadId;

use super::concurrency::LockBasedConflictKind;
use super::{
    GranuleId, LockBased, TransactionId, TxDurableLog, granule_uses_transaction_state_version,
};

const FIRST_DURABLE_LOG_SEGMENT_STREAM_ID: u32 = 0x2000_0000;
const MAX_DURABLE_LOG_SEGMENT_STREAM_ID: u32 = 0x3fff_ffff;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DurableLogSegment {
    stream_id: u32,
}

impl DurableLogSegment {
    pub(crate) fn stream_id(self) -> u32 {
        self.stream_id
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TransactionRegionRuntime(Arc<Mutex<TransactionRegionRuntimeInner>>);

#[derive(Debug)]
pub(crate) struct TransactionRegionRuntimeInner {
    pub(crate) next_transaction_id: u64,
    pub(crate) next_log_segment_stream_id: u32,
    pub(crate) thread_log_segments: HashMap<ThreadId, DurableLogSegment>,
    pub(crate) locks: LockBased,
    pub(crate) conflict_aborted_transactions: BTreeSet<TransactionId>,
    pub(crate) terminal_commits: BTreeSet<TransactionId>,
    pub(crate) granule_versions: BTreeMap<GranuleId, u64>,
    pub(crate) durable_log: TxDurableLog,
}

impl Default for TransactionRegionRuntimeInner {
    fn default() -> Self {
        Self {
            next_transaction_id: 10_001,
            next_log_segment_stream_id: FIRST_DURABLE_LOG_SEGMENT_STREAM_ID,
            thread_log_segments: HashMap::new(),
            locks: LockBased::default(),
            conflict_aborted_transactions: BTreeSet::new(),
            terminal_commits: BTreeSet::new(),
            granule_versions: BTreeMap::new(),
            durable_log: TxDurableLog::default(),
        }
    }
}

impl Default for TransactionRegionRuntime {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(
            TransactionRegionRuntimeInner::default(),
        )))
    }
}

impl TransactionRegionRuntime {
    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, TransactionRegionRuntimeInner>> {
        self.0
            .lock()
            .map_err(|_| crate::format_err!("transaction region runtime lock poisoned"))
    }

    pub(crate) fn allocate_transaction_id(&self) -> Result<TransactionId> {
        let mut runtime = self.lock()?;
        let id = TransactionId::from_raw(runtime.next_transaction_id);
        runtime.next_transaction_id = runtime
            .next_transaction_id
            .checked_add(1)
            .context("transaction id overflow")?;
        Ok(id)
    }

    pub(crate) fn current_thread_log_segment(&self) -> Result<DurableLogSegment> {
        let thread_id = std::thread::current().id();
        let mut runtime = self.lock()?;
        if let Some(segment) = runtime.thread_log_segments.get(&thread_id).copied() {
            return Ok(segment);
        }

        ensure!(
            runtime.next_log_segment_stream_id <= MAX_DURABLE_LOG_SEGMENT_STREAM_ID,
            "durable log segment stream id overflow"
        );
        let segment = DurableLogSegment {
            stream_id: runtime.next_log_segment_stream_id,
        };
        runtime.next_log_segment_stream_id = runtime
            .next_log_segment_stream_id
            .checked_add(1)
            .context("durable log segment stream id overflow")?;
        runtime.thread_log_segments.insert(thread_id, segment);
        Ok(segment)
    }

    pub(crate) fn acquire_granule_read(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<Option<TransactionId>> {
        let mut runtime = self.lock()?;
        Self::check_terminal_owner_conflict(&runtime, transaction, granule, false)?;
        let aborted = runtime
            .locks
            .record_read(transaction, granule, current_version)?;
        if let Some(aborted) = aborted {
            runtime.conflict_aborted_transactions.insert(aborted);
            return Ok(Some(aborted));
        }
        Ok(None)
    }

    pub(crate) fn acquire_granule_write(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<Option<TransactionId>> {
        let mut runtime = self.lock()?;
        Self::check_terminal_owner_conflict(&runtime, transaction, granule, true)?;
        let aborted = runtime
            .locks
            .acquire_write(transaction, granule, current_version)?;
        if let Some(aborted) = aborted {
            runtime.conflict_aborted_transactions.insert(aborted);
            return Ok(Some(aborted));
        }
        Ok(None)
    }

    pub(crate) fn validate_granule_read(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.lock()?
            .locks
            .validate_read(transaction, granule, current_version)
    }

    pub(crate) fn refresh_read_version(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.lock()?
            .locks
            .refresh_read_version(transaction, granule, current_version);
        Ok(())
    }

    pub(crate) fn release_transaction(&self, transaction: TransactionId) -> Result<()> {
        let mut runtime = self.lock()?;
        runtime.locks.release_transaction(transaction);
        runtime.conflict_aborted_transactions.remove(&transaction);
        runtime.terminal_commits.remove(&transaction);
        Ok(())
    }

    pub(crate) fn take_conflict_aborted_transaction(
        &self,
        transaction: TransactionId,
    ) -> Result<bool> {
        Ok(self
            .lock()?
            .conflict_aborted_transactions
            .remove(&transaction))
    }

    pub(crate) fn begin_terminal_commit(&self, transaction: TransactionId) -> Result<()> {
        let mut runtime = self.lock()?;
        ensure!(
            !runtime.conflict_aborted_transactions.remove(&transaction),
            "transaction was conflict-aborted by another transaction"
        );
        runtime.terminal_commits.insert(transaction);
        Ok(())
    }

    pub(crate) fn end_terminal_commit(&self, transaction: TransactionId) -> Result<()> {
        self.lock()?.terminal_commits.remove(&transaction);
        Ok(())
    }

    pub(crate) fn versioned_granule_version(&self, granule: GranuleId) -> Result<u64> {
        Ok(self
            .lock()?
            .granule_versions
            .get(&granule)
            .copied()
            .unwrap_or(0))
    }

    pub(crate) fn bump_versioned_granules<I>(&self, granules: I) -> Result<()>
    where
        I: IntoIterator<Item = GranuleId>,
    {
        let mut runtime = self.lock()?;
        for granule in granules {
            if !granule_uses_transaction_state_version(granule) {
                continue;
            }
            let version = runtime.granule_versions.entry(granule).or_insert(0);
            *version = version
                .checked_add(1)
                .context("transaction granule version overflow")?;
        }
        Ok(())
    }

    fn check_terminal_owner_conflict(
        runtime: &TransactionRegionRuntimeInner,
        transaction: TransactionId,
        granule: GranuleId,
        is_write: bool,
    ) -> Result<()> {
        let Some(owner) = runtime.locks.owner_for_granule(granule) else {
            return Ok(());
        };
        if owner == transaction || transaction > owner || !runtime.terminal_commits.contains(&owner)
        {
            return Ok(());
        }
        let kind = if is_write {
            LockBasedConflictKind::WriteOwnedByOther
        } else {
            LockBasedConflictKind::ReadOwnedByOther
        };
        bail!(kind.message())
    }

    #[cfg(test)]
    pub(crate) fn new_for_test() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn allocate_transaction_id_for_test(&self) -> Result<TransactionId> {
        self.allocate_transaction_id()
    }

    #[cfg(test)]
    pub(crate) fn acquire_granule_read_for_test(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<Option<TransactionId>> {
        self.acquire_granule_read(transaction, granule, current_version)
    }

    #[cfg(test)]
    pub(crate) fn acquire_granule_write_for_test(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<Option<TransactionId>> {
        self.acquire_granule_write(transaction, granule, current_version)
    }

    #[cfg(test)]
    pub(crate) fn release_transaction_for_test(&self, transaction: TransactionId) -> Result<()> {
        self.release_transaction(transaction)
    }

    #[cfg(test)]
    pub(crate) fn begin_terminal_commit_for_test(&self, transaction: TransactionId) -> Result<()> {
        self.begin_terminal_commit(transaction)
    }

    #[cfg(test)]
    pub(crate) fn end_terminal_commit_for_test(&self, transaction: TransactionId) -> Result<()> {
        self.end_terminal_commit(transaction)
    }

    #[cfg(test)]
    pub(crate) fn take_conflict_aborted_transaction_for_test(
        &self,
        transaction: TransactionId,
    ) -> Result<bool> {
        self.take_conflict_aborted_transaction(transaction)
    }

    #[cfg(test)]
    pub(crate) fn transaction_is_terminal_commit_for_test(
        &self,
        transaction: TransactionId,
    ) -> Result<bool> {
        Ok(self.lock()?.terminal_commits.contains(&transaction))
    }

    #[cfg(test)]
    pub(crate) fn ptr_eq_for_test(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    #[cfg(test)]
    pub(crate) fn current_thread_log_segment_for_test(&self) -> Result<DurableLogSegment> {
        self.current_thread_log_segment()
    }
}
