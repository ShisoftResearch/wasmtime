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
    staged_globals: BTreeMap<u32, GlobalSnapshot>,
    staged_memory_granules: BTreeMap<MemoryGranuleKey, Vec<u8>>,
    staged_memory_sizes: BTreeMap<u32, u64>,
    memory_read_granules: BTreeSet<MemoryGranuleKey>,
    memory_write_granules: BTreeSet<MemoryGranuleKey>,
}

impl Default for TransactionState {
    fn default() -> Self {
        Self {
            active: None,
            next_id: 1,
            staged_globals: BTreeMap::new(),
            staged_memory_granules: BTreeMap::new(),
            staged_memory_sizes: BTreeMap::new(),
            memory_read_granules: BTreeSet::new(),
            memory_write_granules: BTreeSet::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StagedRecord {
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
        new_pages: u64,
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
        self.commit_with(|_| Ok(()))
    }

    pub(crate) fn commit_with<F>(&mut self, mut apply: F) -> Result<()>
    where
        F: FnMut(&StagedRecord) -> Result<()>,
    {
        self.ensure_active()?;
        for (&global_index, &value) in &self.staged_globals {
            apply(&StagedRecord::Global {
                global_index,
                value,
            })?;
        }
        for (key, bytes) in &self.staged_memory_granules {
            apply(&StagedRecord::MemoryGranule {
                memory_index: key.memory_index,
                granule_index: key.granule_index,
                bytes: bytes.clone(),
            })?;
        }
        for (&memory_index, &new_pages) in &self.staged_memory_sizes {
            apply(&StagedRecord::MemorySize {
                memory_index,
                new_pages,
            })?;
        }
        self.clear_active();
        Ok(())
    }

    pub(crate) fn abort(&mut self) -> Result<()> {
        self.ensure_active()?;
        self.clear_active();
        Ok(())
    }

    pub(crate) fn fail(&mut self) -> Result<()> {
        self.abort()
    }

    pub(crate) fn stage_memory_size(&mut self, memory_index: u32, new_pages: u64) -> Result<bool> {
        self.ensure_active()?;
        Ok(self
            .staged_memory_sizes
            .insert(memory_index, new_pages)
            .is_none())
    }

    pub(crate) fn stage_global(
        &mut self,
        global_index: u32,
        value: GlobalSnapshot,
    ) -> Result<bool> {
        self.ensure_active()?;
        Ok(self.staged_globals.insert(global_index, value).is_none())
    }

    pub(crate) fn stage_memory_granule(
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
        self.memory_read_granules.insert(key);
        self.memory_write_granules.insert(key);
        Ok(self.staged_memory_granules.insert(key, bytes).is_none())
    }

    pub(crate) fn staged_memory_granule(
        &self,
        memory_index: u32,
        granule_index: u64,
    ) -> Option<&[u8]> {
        self.staged_memory_granules
            .get(&MemoryGranuleKey {
                memory_index,
                granule_index,
            })
            .map(Vec::as_slice)
    }

    pub(crate) fn staged_memory_granule_mut(
        &mut self,
        memory_index: u32,
        granule_index: u64,
    ) -> Option<&mut [u8]> {
        self.staged_memory_granules
            .get_mut(&MemoryGranuleKey {
                memory_index,
                granule_index,
            })
            .map(Vec::as_mut_slice)
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
        self.stage_memory_granule(memory_index, granule_index, bytes)
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
        self.staged_globals.clear();
        self.staged_memory_granules.clear();
        self.staged_memory_sizes.clear();
        self.memory_read_granules.clear();
        self.memory_write_granules.clear();
    }
}

pub(crate) fn execute_research_transaction_fixture(
    state: &mut TransactionState,
    operators: &[wasmtime_environ::ResearchTransactionModuleOperator],
) -> Result<()> {
    for operator in operators {
        match operator.operator {
            wasmtime_environ::TransactionOperator::TTry => {
                state.begin()?;
            }
            wasmtime_environ::TransactionOperator::TFail => {
                state.fail()?;
            }
            other => {
                bail!(
                    "research transaction fixture executor does not handle {other:?} at function {} offset {}",
                    operator.function_index,
                    operator.body_offset
                );
            }
        }
    }
    if state.active_transaction().is_some() {
        state.commit()?;
    }
    Ok(())
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
        state.abort().unwrap();
        assert_eq!(state.active_transaction(), None);
    }

    #[test]
    fn commit_applies_only_latest_staged_records() {
        let mut state = TransactionState::default();
        state.begin().unwrap();
        state.stage_memory_size(0, 1).unwrap();
        state.stage_memory_size(0, 2).unwrap();
        state.stage_global(3, GlobalSnapshot::I64(11)).unwrap();
        state.stage_global(3, GlobalSnapshot::I64(22)).unwrap();
        state.stage_memory_granule(0, 7, vec![0x11; 256]).unwrap();
        state.stage_memory_granule(0, 7, vec![0x22; 256]).unwrap();

        let mut applied = Vec::new();
        state
            .commit_with(|record| {
                applied.push(record.clone());
                Ok(())
            })
            .unwrap();

        assert_eq!(
            applied,
            [
                StagedRecord::Global {
                    global_index: 3,
                    value: GlobalSnapshot::I64(22)
                },
                StagedRecord::MemoryGranule {
                    memory_index: 0,
                    granule_index: 7,
                    bytes: vec![0x22; 256]
                },
                StagedRecord::MemorySize {
                    memory_index: 0,
                    new_pages: 2
                },
            ]
        );
    }

    #[test]
    fn abort_drops_staged_records_without_apply() {
        let mut state = TransactionState::default();
        state.begin().unwrap();
        state.stage_memory_size(0, 9).unwrap();
        state.stage_global(3, GlobalSnapshot::I32(4)).unwrap();
        state.stage_memory_granule(0, 7, vec![0x11; 256]).unwrap();

        state.abort().unwrap();
        assert_eq!(state.active_transaction(), None);

        state.begin().unwrap();
        let mut applied = Vec::new();
        state
            .commit_with(|record| {
                applied.push(record.clone());
                Ok(())
            })
            .unwrap();

        assert!(applied.is_empty());
    }

    #[test]
    fn duplicate_granule_write_updates_one_staged_buffer() {
        let mut state = TransactionState::default();
        state.begin().unwrap();

        assert!(state.stage_memory_granule(0, 7, vec![0x11; 256]).unwrap());
        assert!(!state.stage_memory_granule(0, 7, vec![0x22; 256]).unwrap());

        let staged = state.staged_memory_granule(0, 7).unwrap();
        assert_eq!(staged, &[0x22; 256]);
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

        assert_eq!(state.staged_memory_granule(0, 7).unwrap(), &[0x22; 256]);
    }

    #[test]
    fn fail_drops_staged_records() {
        let mut state = TransactionState::default();
        state.begin().unwrap();
        state.stage_memory_size(0, 9).unwrap();

        state.fail().unwrap();

        assert_eq!(state.active_transaction(), None);
    }

    #[test]
    fn duplicate_global_write_updates_latest_staged_value() {
        let mut state = TransactionState::default();
        state.begin().unwrap();

        assert!(state.stage_global(3, GlobalSnapshot::I64(11)).unwrap());
        assert!(!state.stage_global(3, GlobalSnapshot::I64(22)).unwrap());

        let mut applied = Vec::new();
        state
            .commit_with(|record| {
                applied.push(record.clone());
                Ok(())
            })
            .unwrap();

        assert_eq!(
            applied,
            [StagedRecord::Global {
                global_index: 3,
                value: GlobalSnapshot::I64(22)
            }]
        );
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

    #[test]
    fn fixture_executor_begins_and_fails_transaction() {
        let mut state = TransactionState::default();
        let operators = [
            wasmtime_environ::ResearchTransactionModuleOperator {
                function_index: 0,
                body_offset: 0,
                operator: wasmtime_environ::TransactionOperator::TTry,
                bytes_read: 2,
            },
            wasmtime_environ::ResearchTransactionModuleOperator {
                function_index: 0,
                body_offset: 2,
                operator: wasmtime_environ::TransactionOperator::TFail,
                bytes_read: 2,
            },
        ];

        execute_research_transaction_fixture(&mut state, &operators).unwrap();

        assert_eq!(state.active_transaction(), None);
    }

    #[test]
    fn fixture_executor_commits_open_transaction_at_end() {
        let mut state = TransactionState::default();
        let operators = [wasmtime_environ::ResearchTransactionModuleOperator {
            function_index: 0,
            body_offset: 0,
            operator: wasmtime_environ::TransactionOperator::TTry,
            bytes_read: 2,
        }];

        execute_research_transaction_fixture(&mut state, &operators).unwrap();

        assert_eq!(state.active_transaction(), None);
    }

    #[test]
    fn module_compilation_accepts_lifecycle_transaction_opcodes() {
        let engine = crate::Engine::default();
        let wasm = [
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x0a, 0x08, 0x01, 0x06, 0x00, // code section/function body
            0xfa, 0x04, // ttry
            0xfa, 0x0f, // tfail
            0x0b, // end
        ];

        crate::Module::new(&engine, wasm).unwrap();
    }

    #[test]
    fn module_compilation_accepts_transaction_data_helper_lowering() {
        let engine = crate::Engine::default();
        let wasm = [
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x05, 0x03, 0x01, 0x00, 0x01, // memory section
            0x0a, 0x0a, 0x01, 0x08, 0x00, // code section/function body
            0x41, 0x00, // i32.const 0
            0xfa, 0x28, 0x02, 0x00, // i32.tload align=2 offset=0
            0x0b, // end
        ];

        crate::Module::new(&engine, wasm).unwrap();
    }

    #[test]
    fn module_compilation_accepts_transaction_store_helper_lowering() {
        let engine = crate::Engine::default();
        let wasm = [
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x05, 0x03, 0x01, 0x00, 0x01, // memory section
            0x0a, 0x0c, 0x01, 0x0a, 0x00, // code section/function body
            0x41, 0x00, // i32.const 0
            0x41, 0x2a, // i32.const 42
            0xfa, 0x36, 0x02, 0x00, // i32.tstore align=2 offset=0
            0x0b, // end
        ];

        crate::Module::new(&engine, wasm).unwrap();
    }

    #[test]
    fn module_compilation_accepts_transaction_global_get_helper_lowering() {
        let engine = crate::Engine::default();
        let wasm = [
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x06, 0x06, 0x01, 0x7f, 0x00, 0x41, 0x00, 0x0b, // global section
            0x0a, 0x07, 0x01, 0x05, 0x00, // code section/function body
            0xfa, 0x23, 0x00, // tglobal.get 0
            0x0b, // end
        ];

        crate::Module::new(&engine, wasm).unwrap();
    }

    #[test]
    fn module_compilation_accepts_transaction_global_set_helper_lowering() {
        let engine = crate::Engine::default();
        let wasm = [
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x06, 0x06, 0x01, 0x7f, 0x01, 0x41, 0x00, 0x0b, // global section
            0x0a, 0x09, 0x01, 0x07, 0x00, // code section/function body
            0x41, 0x2a, // i32.const 42
            0xfa, 0x24, 0x00, // tglobal.set 0
            0x0b, // end
        ];

        crate::Module::new(&engine, wasm).unwrap();
    }

    #[test]
    fn module_compilation_accepts_transaction_memory_size_helper_lowering() {
        let engine = crate::Engine::default();
        let wasm = [
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x05, 0x03, 0x01, 0x00, 0x01, // memory section
            0x0a, 0x07, 0x01, 0x05, 0x00, // code section/function body
            0xfa, 0x3f, 0x00, // tmemory.size 0
            0x0b, // end
        ];

        crate::Module::new(&engine, wasm).unwrap();
    }

    #[test]
    fn module_compilation_accepts_transaction_memory_grow_helper_lowering() {
        let engine = crate::Engine::default();
        let wasm = [
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x05, 0x03, 0x01, 0x00, 0x01, // memory section
            0x0a, 0x09, 0x01, 0x07, 0x00, // code section/function body
            0x41, 0x01, // i32.const 1
            0xfa, 0x40, 0x00, // tmemory.grow 0
            0x0b, // end
        ];

        crate::Module::new(&engine, wasm).unwrap();
    }
}
