use crate::prelude::*;
use std::path::PathBuf;

#[cfg(all(
    feature = "transaction",
    feature = "transaction-cc-lockbased",
    feature = "transaction-cc-nowait-abort"
))]
compile_error!(
    "select exactly one transaction concurrency-control feature: \
     transaction-cc-lockbased or transaction-cc-nowait-abort"
);

#[cfg(all(
    feature = "transaction",
    not(any(
        feature = "transaction-cc-lockbased",
        feature = "transaction-cc-nowait-abort"
    ))
))]
compile_error!(
    "transaction requires one transaction concurrency-control feature: \
     transaction-cc-lockbased or transaction-cc-nowait-abort"
);

// Milestone runtime core for proposal WAST progress. The current runtime uses
// store-local transaction state, `VMemory` and configurable `NVMemory`
// transactional memory storage, and real `tmemory` sidecars. Remaining
// `SHISOFT-TWASM-MOCK` tags in this file identify policy selection and
// object-table gaps.

/// Storage backend selected for transactional memories.
///
/// SHISOFT-TWASM-MOCK: backend selection still lives behind transaction
/// research configuration rather than Wasmtime's public embedding API.
///
/// Milestone runtime support currently implements `VMemory` and research
/// `NVMemory` by default, with hardware-PMEM mode selectable for experiments.
/// `FileBackedMemory` is an opt-in mmap-backed durable test/research backend;
/// ordinary Wasmtime memories still use their existing storage path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TMemoryBackend {
    VMemory,
    FileBackedMemory,
    NVMemory,
}

/// Persistence behavior for `NVMemory` block regions.
///
/// The research mode is the default so normal tests do not require PMEM
/// hardware. `RequireHardwarePmem` routes NVMemory through the CLWB/SFENCE
/// persistence engine and fails construction when the host cannot provide it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TMemoryPersistenceMode {
    ResearchPretendPmem,
    RequireHardwarePmem,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TMemoryFileBacking {
    Temp,
    Path(PathBuf),
    ExistingPath(PathBuf),
}

/// SHISOFT-TWASM-MOCK: selectable concurrency policy shape. Policy selection is
/// currently compile-time for branch experiments rather than public API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConcurrencyControl {
    LockBased,
    NoWaitAbort,
}

impl ConcurrencyControl {
    pub(crate) const fn default_for_build() -> Self {
        #[cfg(feature = "transaction-cc-nowait-abort")]
        {
            return Self::NoWaitAbort;
        }

        #[cfg(feature = "transaction-cc-lockbased")]
        {
            return Self::LockBased;
        }

        #[cfg(not(any(
            feature = "transaction-cc-nowait-abort",
            feature = "transaction-cc-lockbased"
        )))]
        {
            return Self::LockBased;
        }
    }
}

/// SHISOFT-TWASM-MOCK: durability policy selection is not yet public API.
/// File-backed transaction storage has restart recovery for current
/// tmemory/object-log tests, while broader policy selection and hardware-PMEM
/// validation remain research work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DurabilityPolicy {
    VolatileRollbackOnly,
}

/// SHISOFT-TWASM-MOCK: conflict behavior is the current abort/default scaffold;
/// richer policy selection is deferred to the real concurrency-control layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConflictPolicy {
    AbortOrWizardDefault,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObjectIndexPersistencePolicy {
    RebuildOnRecovery,
    PersistentIndex,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TransactionConfig {
    tmemory_backend: TMemoryBackend,
    tmemory_persistence_mode: TMemoryPersistenceMode,
    tmemory_file_backing: Option<TMemoryFileBacking>,
    concurrency_control: ConcurrencyControl,
    durability_policy: DurabilityPolicy,
    conflict_policy: ConflictPolicy,
    object_index_persistence_policy: ObjectIndexPersistencePolicy,
}

impl Default for TransactionConfig {
    fn default() -> Self {
        Self {
            tmemory_backend: TMemoryBackend::VMemory,
            tmemory_persistence_mode: TMemoryPersistenceMode::ResearchPretendPmem,
            tmemory_file_backing: None,
            concurrency_control: ConcurrencyControl::default_for_build(),
            durability_policy: DurabilityPolicy::VolatileRollbackOnly,
            conflict_policy: ConflictPolicy::AbortOrWizardDefault,
            object_index_persistence_policy: ObjectIndexPersistencePolicy::RebuildOnRecovery,
        }
    }
}

impl TransactionConfig {
    pub(crate) fn with_tmemory_backend(tmemory_backend: TMemoryBackend) -> Result<Self> {
        let mut config = Self::default();
        config.set_tmemory_backend(tmemory_backend)?;
        Ok(config)
    }

    pub(crate) fn with_nvmemory_persistence_mode(
        tmemory_persistence_mode: TMemoryPersistenceMode,
    ) -> Result<Self> {
        let mut config = Self::default();
        config.set_nvmemory_persistence_mode(tmemory_persistence_mode)?;
        Ok(config)
    }

    pub(crate) fn with_file_backed_tmemory_temp() -> Result<Self> {
        let mut config = Self::default();
        config.set_file_backed_tmemory(TMemoryFileBacking::Temp)?;
        Ok(config)
    }

    pub(crate) fn with_file_backed_tmemory_path(path: PathBuf) -> Result<Self> {
        ensure!(
            !path.as_os_str().is_empty(),
            "file-backed tmemory path cannot be empty"
        );
        let mut config = Self::default();
        config.set_file_backed_tmemory(TMemoryFileBacking::Path(path))?;
        Ok(config)
    }

    pub(crate) fn with_file_backed_tmemory_existing_path(path: PathBuf) -> Result<Self> {
        ensure!(
            !path.as_os_str().is_empty(),
            "file-backed tmemory path cannot be empty"
        );
        let mut config = Self::default();
        config.set_file_backed_tmemory(TMemoryFileBacking::ExistingPath(path))?;
        Ok(config)
    }

    pub(crate) fn with_object_index_persistence_policy(
        object_index_persistence_policy: ObjectIndexPersistencePolicy,
    ) -> Result<Self> {
        let mut config = Self::default();
        config.set_object_index_persistence_policy(object_index_persistence_policy)?;
        Ok(config)
    }

    pub(crate) fn with_concurrency_control(
        concurrency_control: ConcurrencyControl,
    ) -> Result<Self> {
        let mut config = Self::default();
        config.set_concurrency_control(concurrency_control)?;
        Ok(config)
    }

    pub(crate) fn tmemory_backend(&self) -> TMemoryBackend {
        self.tmemory_backend
    }

    pub(crate) fn tmemory_persistence_mode(&self) -> TMemoryPersistenceMode {
        self.tmemory_persistence_mode
    }

    pub(crate) fn tmemory_file_backing(&self) -> Option<TMemoryFileBacking> {
        self.tmemory_file_backing.clone()
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

    pub(crate) fn object_index_persistence_policy(&self) -> ObjectIndexPersistencePolicy {
        self.object_index_persistence_policy
    }

    pub(crate) fn is_vmemory_only(&self) -> bool {
        self.tmemory_backend == TMemoryBackend::VMemory
    }

    fn set_tmemory_backend(&mut self, tmemory_backend: TMemoryBackend) -> Result<()> {
        match tmemory_backend {
            TMemoryBackend::VMemory | TMemoryBackend::NVMemory => {
                self.tmemory_backend = tmemory_backend;
                self.tmemory_file_backing = None;
                Ok(())
            }
            TMemoryBackend::FileBackedMemory => {
                bail!("FileBackedMemory requires explicit file backing configuration")
            }
        }
    }

    fn set_nvmemory_persistence_mode(
        &mut self,
        tmemory_persistence_mode: TMemoryPersistenceMode,
    ) -> Result<()> {
        self.tmemory_backend = TMemoryBackend::NVMemory;
        self.tmemory_persistence_mode = tmemory_persistence_mode;
        self.tmemory_file_backing = None;
        Ok(())
    }

    fn set_file_backed_tmemory(&mut self, file_backing: TMemoryFileBacking) -> Result<()> {
        self.tmemory_backend = TMemoryBackend::FileBackedMemory;
        self.tmemory_file_backing = Some(file_backing);
        Ok(())
    }

    fn set_concurrency_control(&mut self, concurrency_control: ConcurrencyControl) -> Result<()> {
        self.concurrency_control = concurrency_control;
        Ok(())
    }

    fn set_object_index_persistence_policy(
        &mut self,
        object_index_persistence_policy: ObjectIndexPersistencePolicy,
    ) -> Result<()> {
        match object_index_persistence_policy {
            ObjectIndexPersistencePolicy::RebuildOnRecovery => {
                self.object_index_persistence_policy = object_index_persistence_policy;
                Ok(())
            }
            ObjectIndexPersistencePolicy::PersistentIndex => {
                bail!("PersistentIndex object-table policy is not implemented yet")
            }
        }
    }
}
