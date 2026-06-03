#![allow(dead_code)]

use crate::prelude::*;
use alloc::collections::{BTreeMap, BTreeSet};

/// Storage backend selected for transactional memories.
///
/// Milestone 1 only implements `VMemory`. The other variants are represented
/// now so transaction configuration has the right shape for later persistence
/// experiments without changing ordinary Wasmtime memories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TMemoryBackend {
    VMemory,
    FileBackedMemory,
    NVMemory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConcurrencyControl {
    LockBased,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DurabilityPolicy {
    VolatileRollbackOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConflictPolicy {
    AbortOrWizardDefault,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TransactionConfig {
    tmemory_backend: TMemoryBackend,
    concurrency_control: ConcurrencyControl,
    durability_policy: DurabilityPolicy,
    conflict_policy: ConflictPolicy,
}

impl Default for TransactionConfig {
    fn default() -> Self {
        Self {
            tmemory_backend: TMemoryBackend::VMemory,
            concurrency_control: ConcurrencyControl::LockBased,
            durability_policy: DurabilityPolicy::VolatileRollbackOnly,
            conflict_policy: ConflictPolicy::AbortOrWizardDefault,
        }
    }
}

impl TransactionConfig {
    pub(crate) fn with_tmemory_backend(tmemory_backend: TMemoryBackend) -> Result<Self> {
        let mut config = Self::default();
        config.set_tmemory_backend(tmemory_backend)?;
        Ok(config)
    }

    pub(crate) fn tmemory_backend(&self) -> TMemoryBackend {
        self.tmemory_backend
    }

    pub(crate) fn concurrency_control(&self) -> ConcurrencyControl {
        self.concurrency_control
    }

    pub(crate) fn durability_policy(&self) -> DurabilityPolicy {
        self.durability_policy
    }

    pub(crate) fn conflict_policy(&self) -> ConflictPolicy {
        self.conflict_policy
    }

    pub(crate) fn is_vmemory_only(&self) -> bool {
        self.tmemory_backend == TMemoryBackend::VMemory
    }

    fn set_tmemory_backend(&mut self, tmemory_backend: TMemoryBackend) -> Result<()> {
        match tmemory_backend {
            TMemoryBackend::VMemory => {
                self.tmemory_backend = tmemory_backend;
                Ok(())
            }
            TMemoryBackend::FileBackedMemory | TMemoryBackend::NVMemory => {
                bail!("tmemory backend is not implemented: {tmemory_backend:?}")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct TransactionId(u64);

#[derive(Debug)]
pub(crate) struct TransactionState {
    active: Option<TransactionId>,
    next_id: u64,
    undo_log: Vec<UndoRecord>,
    globals: BTreeSet<u32>,
    memory_read_granules: BTreeSet<MemoryGranuleKey>,
    memory_write_granules: BTreeSet<MemoryGranuleKey>,
}

impl Default for TransactionState {
    fn default() -> Self {
        Self {
            active: None,
            next_id: 1,
            undo_log: Vec::new(),
            globals: BTreeSet::new(),
            memory_read_granules: BTreeSet::new(),
            memory_write_granules: BTreeSet::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum UndoRecord {
    Global {
        global_index: u32,
        value: GlobalSnapshot,
    },
    MemoryGranule {
        memory_index: u32,
        granule_index: u64,
        bytes: Vec<u8>,
    },
    MemorySize {
        memory_index: u32,
        old_pages: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GlobalSnapshot {
    I32(i32),
    I64(i64),
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct MemoryGranuleKey {
    memory_index: u32,
    granule_index: u64,
}

pub(crate) trait TransactionConcurrencyControl {
    fn acquire_memory_granule_read(
        &mut self,
        transaction: TransactionId,
        memory_index: u32,
        granule_index: u64,
    ) -> Result<()>;

    fn acquire_memory_granule_write(
        &mut self,
        transaction: TransactionId,
        memory_index: u32,
        granule_index: u64,
    ) -> Result<()>;

    fn release_transaction(&mut self, transaction: TransactionId);
}

#[derive(Debug, Default)]
pub(crate) struct LockBased {
    memory_granules: BTreeMap<MemoryGranuleKey, MemoryGranuleOwnership>,
}

#[derive(Debug, Default)]
struct MemoryGranuleOwnership {
    readers: BTreeSet<TransactionId>,
    writer: Option<TransactionId>,
}

impl TransactionConcurrencyControl for LockBased {
    fn acquire_memory_granule_read(
        &mut self,
        transaction: TransactionId,
        memory_index: u32,
        granule_index: u64,
    ) -> Result<()> {
        let key = MemoryGranuleKey {
            memory_index,
            granule_index,
        };
        let ownership = self.memory_granules.entry(key).or_default();
        if ownership.writer.is_some_and(|writer| writer != transaction) {
            bail!("tmemory granule conflict: write owner blocks read");
        }
        ownership.readers.insert(transaction);
        Ok(())
    }

    fn acquire_memory_granule_write(
        &mut self,
        transaction: TransactionId,
        memory_index: u32,
        granule_index: u64,
    ) -> Result<()> {
        let key = MemoryGranuleKey {
            memory_index,
            granule_index,
        };
        let ownership = self.memory_granules.entry(key).or_default();
        if ownership.writer.is_some_and(|writer| writer != transaction) {
            bail!("tmemory granule conflict: write owner blocks write");
        }
        if ownership
            .readers
            .iter()
            .any(|reader| *reader != transaction)
        {
            bail!("tmemory granule conflict: read owner blocks write");
        }
        ownership.readers.insert(transaction);
        ownership.writer = Some(transaction);
        Ok(())
    }

    fn release_transaction(&mut self, transaction: TransactionId) {
        self.memory_granules.retain(|_, ownership| {
            ownership.readers.remove(&transaction);
            if ownership.writer == Some(transaction) {
                ownership.writer = None;
            }
            !ownership.readers.is_empty() || ownership.writer.is_some()
        });
    }
}

impl TransactionState {
    pub(crate) fn begin(&mut self) -> Result<TransactionId> {
        ensure!(
            self.active.is_none(),
            "transaction is already active in this store"
        );
        let id = TransactionId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("transaction id overflow")?;
        self.active = Some(id);
        Ok(id)
    }

    pub(crate) fn active_transaction(&self) -> Option<TransactionId> {
        self.active
    }

    pub(crate) fn commit(&mut self) -> Result<()> {
        self.ensure_active()?;
        self.clear_active();
        Ok(())
    }

    pub(crate) fn abort_with<F>(&mut self, mut restore: F) -> Result<()>
    where
        F: FnMut(&UndoRecord) -> Result<()>,
    {
        self.ensure_active()?;
        for record in self.undo_log.iter().rev() {
            restore(record)?;
        }
        self.clear_active();
        Ok(())
    }

    pub(crate) fn fail_with<F>(&mut self, restore: F) -> Result<()>
    where
        F: FnMut(&UndoRecord) -> Result<()>,
    {
        self.abort_with(restore)
    }

    pub(crate) fn record_memory_size(&mut self, memory_index: u32, old_pages: u64) -> Result<()> {
        self.ensure_active()?;
        self.undo_log.push(UndoRecord::MemorySize {
            memory_index,
            old_pages,
        });
        Ok(())
    }

    pub(crate) fn record_global_restore(
        &mut self,
        global_index: u32,
        value: GlobalSnapshot,
    ) -> Result<bool> {
        self.ensure_active()?;
        if !self.globals.insert(global_index) {
            return Ok(false);
        }
        self.undo_log.push(UndoRecord::Global {
            global_index,
            value,
        });
        Ok(true)
    }

    pub(crate) fn record_memory_granule(
        &mut self,
        memory_index: u32,
        granule_index: u64,
        bytes: Vec<u8>,
    ) -> Result<bool> {
        self.ensure_active()?;
        let key = MemoryGranuleKey {
            memory_index,
            granule_index,
        };
        if !self.memory_write_granules.insert(key) {
            return Ok(false);
        }
        self.memory_read_granules.insert(key);

        self.undo_log.push(UndoRecord::MemoryGranule {
            memory_index,
            granule_index,
            bytes,
        });
        Ok(true)
    }

    pub(crate) fn acquire_memory_granule_read(
        &mut self,
        memory_index: u32,
        granule_index: u64,
    ) -> Result<bool> {
        self.ensure_active()?;
        Ok(self.memory_read_granules.insert(MemoryGranuleKey {
            memory_index,
            granule_index,
        }))
    }

    pub(crate) fn acquire_memory_granule_write(
        &mut self,
        memory_index: u32,
        granule_index: u64,
        bytes: Vec<u8>,
    ) -> Result<bool> {
        self.record_memory_granule(memory_index, granule_index, bytes)
    }

    pub(crate) fn owns_memory_granule_read(&self, memory_index: u32, granule_index: u64) -> bool {
        self.memory_read_granules.contains(&MemoryGranuleKey {
            memory_index,
            granule_index,
        })
    }

    pub(crate) fn owns_memory_granule_write(&self, memory_index: u32, granule_index: u64) -> bool {
        self.memory_write_granules.contains(&MemoryGranuleKey {
            memory_index,
            granule_index,
        })
    }

    fn ensure_active(&self) -> Result<()> {
        ensure!(self.active.is_some(), "no active transaction in this store");
        Ok(())
    }

    fn clear_active(&mut self) {
        self.active = None;
        self.undo_log.clear();
        self.globals.clear();
        self.memory_read_granules.clear();
        self.memory_write_granules.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_transaction_config_uses_minimal_vmemory_runtime() {
        let config = TransactionConfig::default();

        assert_eq!(config.tmemory_backend(), TMemoryBackend::VMemory);
        assert_eq!(config.concurrency_control(), ConcurrencyControl::LockBased);
        assert_eq!(
            config.durability_policy(),
            DurabilityPolicy::VolatileRollbackOnly
        );
        assert_eq!(
            config.conflict_policy(),
            ConflictPolicy::AbortOrWizardDefault
        );
    }

    #[test]
    fn transaction_config_accepts_only_vmemory_backend_for_milestone_1() {
        assert!(
            TransactionConfig::with_tmemory_backend(TMemoryBackend::VMemory)
                .unwrap()
                .is_vmemory_only()
        );

        for backend in [TMemoryBackend::FileBackedMemory, TMemoryBackend::NVMemory] {
            let error = TransactionConfig::with_tmemory_backend(backend).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("tmemory backend is not implemented")
            );
        }
    }

    #[test]
    fn begin_commit_and_abort_clear_active_transaction() {
        let mut state = TransactionState::default();

        let first = state.begin().unwrap();
        assert_eq!(state.active_transaction(), Some(first));
        state.commit().unwrap();
        assert_eq!(state.active_transaction(), None);

        let second = state.begin().unwrap();
        assert_eq!(state.active_transaction(), Some(second));
        state.abort_with(|_| Ok(())).unwrap();
        assert_eq!(state.active_transaction(), None);
    }

    #[test]
    fn abort_runs_undo_records_in_reverse_order() {
        let mut state = TransactionState::default();
        state.begin().unwrap();
        state
            .record_memory_size(0, 1)
            .expect("memory size undo record");
        state
            .record_memory_size(0, 2)
            .expect("memory size undo record");

        let mut pages = Vec::new();
        state
            .abort_with(|record| {
                if let UndoRecord::MemorySize { old_pages, .. } = record {
                    pages.push(*old_pages);
                }
                Ok(())
            })
            .unwrap();

        assert_eq!(pages, [2, 1]);
    }

    #[test]
    fn duplicate_granule_write_records_one_snapshot() {
        let mut state = TransactionState::default();
        state.begin().unwrap();

        assert!(state.record_memory_granule(0, 7, vec![0x11; 256]).unwrap());
        assert!(!state.record_memory_granule(0, 7, vec![0x22; 256]).unwrap());

        let mut snapshots = Vec::new();
        state
            .abort_with(|record| {
                if let UndoRecord::MemoryGranule { bytes, .. } = record {
                    snapshots.push(bytes[0]);
                }
                Ok(())
            })
            .unwrap();

        assert_eq!(snapshots, [0x11]);
    }

    #[test]
    fn memory_granule_acquisition_requires_active_transaction() {
        let mut state = TransactionState::default();

        assert!(state.acquire_memory_granule_read(0, 7).is_err());
        assert!(
            state
                .acquire_memory_granule_write(0, 7, vec![0x11; 256])
                .is_err()
        );
    }

    #[test]
    fn read_and_write_granule_acquisition_tracks_owned_sets() {
        let mut state = TransactionState::default();
        state.begin().unwrap();

        assert!(state.acquire_memory_granule_read(0, 7).unwrap());
        assert!(!state.acquire_memory_granule_read(0, 7).unwrap());
        assert!(state.owns_memory_granule_read(0, 7));
        assert!(!state.owns_memory_granule_write(0, 7));

        assert!(
            state
                .acquire_memory_granule_write(0, 7, vec![0x11; 256])
                .unwrap()
        );
        assert!(
            !state
                .acquire_memory_granule_write(0, 7, vec![0x22; 256])
                .unwrap()
        );
        assert!(state.owns_memory_granule_read(0, 7));
        assert!(state.owns_memory_granule_write(0, 7));

        let mut snapshots = Vec::new();
        state
            .abort_with(|record| {
                if let UndoRecord::MemoryGranule { bytes, .. } = record {
                    snapshots.push(bytes[0]);
                }
                Ok(())
            })
            .unwrap();

        assert_eq!(snapshots, [0x11]);
    }

    #[test]
    fn fail_aborts_and_runs_undo_records() {
        let mut state = TransactionState::default();
        state.begin().unwrap();
        state
            .record_memory_size(0, 9)
            .expect("memory size undo record");

        let mut restored = Vec::new();
        state
            .fail_with(|record| {
                if let UndoRecord::MemorySize { old_pages, .. } = record {
                    restored.push(*old_pages);
                }
                Ok(())
            })
            .unwrap();

        assert_eq!(restored, [9]);
        assert_eq!(state.active_transaction(), None);
    }

    #[test]
    fn duplicate_global_write_records_one_restore_snapshot() {
        let mut state = TransactionState::default();
        state.begin().unwrap();

        assert!(
            state
                .record_global_restore(3, GlobalSnapshot::I64(11))
                .unwrap()
        );
        assert!(
            !state
                .record_global_restore(3, GlobalSnapshot::I64(22))
                .unwrap()
        );

        let mut restored = Vec::new();
        state
            .abort_with(|record| {
                if let UndoRecord::Global {
                    global_index,
                    value,
                } = record
                {
                    restored.push((*global_index, *value));
                }
                Ok(())
            })
            .unwrap();

        assert_eq!(restored, [(3, GlobalSnapshot::I64(11))]);
    }

    #[test]
    fn lock_based_allows_shared_reads_and_rejects_conflicting_write() {
        let mut locks = LockBased::default();
        let first = TransactionId(1);
        let second = TransactionId(2);

        locks.acquire_memory_granule_read(first, 0, 7).unwrap();
        locks.acquire_memory_granule_read(second, 0, 7).unwrap();

        let error = locks
            .acquire_memory_granule_write(second, 0, 7)
            .unwrap_err();
        assert!(error.to_string().contains("tmemory granule conflict"));
    }

    #[test]
    fn lock_based_writer_excludes_other_transactions() {
        let mut locks = LockBased::default();
        let first = TransactionId(1);
        let second = TransactionId(2);

        locks.acquire_memory_granule_write(first, 0, 7).unwrap();

        let read_error = locks.acquire_memory_granule_read(second, 0, 7).unwrap_err();
        assert!(read_error.to_string().contains("tmemory granule conflict"));

        let write_error = locks
            .acquire_memory_granule_write(second, 0, 7)
            .unwrap_err();
        assert!(write_error.to_string().contains("tmemory granule conflict"));
    }

    #[test]
    fn lock_based_supports_upgrade_and_release() {
        let mut locks = LockBased::default();
        let first = TransactionId(1);
        let second = TransactionId(2);

        locks.acquire_memory_granule_read(first, 0, 7).unwrap();
        locks.acquire_memory_granule_write(first, 0, 7).unwrap();
        locks.release_transaction(first);

        locks.acquire_memory_granule_write(second, 0, 7).unwrap();
    }
}
