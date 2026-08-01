use crate::prelude::*;
use crate::runtime::vm::{
    RegionAddressWindow, RegionBackendKind, RegionDescriptor, RegionId, RegionSetDescriptor,
};
use std::path::PathBuf;

#[cfg(all(
    feature = "transaction",
    any(
        all(
            feature = "transaction-cc-lockbased",
            feature = "transaction-cc-nowait-abort"
        ),
        all(
            feature = "transaction-cc-lockbased",
            feature = "transaction-cc-wound-wait"
        ),
        all(
            feature = "transaction-cc-lockbased",
            feature = "transaction-cc-wait-die"
        ),
        all(
            feature = "transaction-cc-lockbased",
            feature = "transaction-cc-strict-2pl"
        ),
        all(
            feature = "transaction-cc-lockbased",
            feature = "transaction-cc-optimistic-validation"
        ),
        all(
            feature = "transaction-cc-lockbased",
            feature = "transaction-cc-timestamp-ordering"
        ),
        all(
            feature = "transaction-cc-nowait-abort",
            feature = "transaction-cc-wound-wait"
        ),
        all(
            feature = "transaction-cc-nowait-abort",
            feature = "transaction-cc-wait-die"
        ),
        all(
            feature = "transaction-cc-nowait-abort",
            feature = "transaction-cc-strict-2pl"
        ),
        all(
            feature = "transaction-cc-nowait-abort",
            feature = "transaction-cc-optimistic-validation"
        ),
        all(
            feature = "transaction-cc-nowait-abort",
            feature = "transaction-cc-timestamp-ordering"
        ),
        all(
            feature = "transaction-cc-wound-wait",
            feature = "transaction-cc-wait-die"
        ),
        all(
            feature = "transaction-cc-wound-wait",
            feature = "transaction-cc-strict-2pl"
        ),
        all(
            feature = "transaction-cc-wound-wait",
            feature = "transaction-cc-optimistic-validation"
        ),
        all(
            feature = "transaction-cc-wound-wait",
            feature = "transaction-cc-timestamp-ordering"
        ),
        all(
            feature = "transaction-cc-wait-die",
            feature = "transaction-cc-strict-2pl"
        ),
        all(
            feature = "transaction-cc-wait-die",
            feature = "transaction-cc-optimistic-validation"
        ),
        all(
            feature = "transaction-cc-wait-die",
            feature = "transaction-cc-timestamp-ordering"
        ),
        all(
            feature = "transaction-cc-strict-2pl",
            feature = "transaction-cc-optimistic-validation"
        ),
        all(
            feature = "transaction-cc-strict-2pl",
            feature = "transaction-cc-timestamp-ordering"
        ),
        all(
            feature = "transaction-cc-optimistic-validation",
            feature = "transaction-cc-timestamp-ordering"
        )
    )
))]
compile_error!(
    "select exactly one transaction concurrency-control feature: \
     transaction-cc-lockbased, transaction-cc-nowait-abort, \
     transaction-cc-wound-wait, transaction-cc-wait-die, \
     transaction-cc-strict-2pl, or \
     transaction-cc-optimistic-validation, or \
     transaction-cc-timestamp-ordering"
);

#[cfg(all(
    feature = "transaction",
    not(any(
        feature = "transaction-cc-lockbased",
        feature = "transaction-cc-nowait-abort",
        feature = "transaction-cc-wound-wait",
        feature = "transaction-cc-wait-die",
        feature = "transaction-cc-strict-2pl",
        feature = "transaction-cc-optimistic-validation",
        feature = "transaction-cc-timestamp-ordering"
    ))
))]
compile_error!(
    "transaction requires one transaction concurrency-control feature: \
     transaction-cc-lockbased, transaction-cc-nowait-abort, \
     transaction-cc-wound-wait, transaction-cc-wait-die, \
     transaction-cc-strict-2pl, or \
     transaction-cc-optimistic-validation, or \
     transaction-cc-timestamp-ordering"
);

// Milestone runtime core for proposal WAST progress. The current runtime uses
// store-local transaction state, `VMemory` and configurable durable tmemory
// transactional memory storage, and real `tmemory` sidecars. Remaining
// `SHISOFT-TWASM-MOCK` tags in this file identify policy selection and
// object-table gaps.

/// Storage backend selected for transactional memories.
///
/// SHISOFT-TWASM-MOCK: backend selection still lives behind transaction
/// research configuration rather than Wasmtime's public embedding API.
///
/// Milestone runtime support currently implements `VMemory`, `FileBackedMemory`,
/// and research/configurable DAX PMEM backends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TMemoryBackend {
    VMemory,
    FileBackedMemory,
    DaxPmem,
}

/// Persistence behavior for durable tmemory block regions.
///
/// The research mode is the default so normal tests do not require PMEM
/// hardware.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TMemoryPersistenceMode {
    ResearchPretendDaxPmem,
    RequireDaxPmem,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TMemoryRegionConfig {
    pub(crate) path: PathBuf,
    pub(crate) address_base: usize,
    pub(crate) reserved_len: usize,
    pub(crate) numa_node: Option<i32>,
}

impl TMemoryRegionConfig {
    #[cfg(test)]
    pub(crate) fn new_for_test(
        path: impl Into<PathBuf>,
        address_base: usize,
        reserved_len: usize,
    ) -> Self {
        Self {
            path: path.into(),
            address_base,
            reserved_len,
            numa_node: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TMemoryDaxPmemBacking {
    ResearchTemp,
    FsDaxPath(PathBuf),
    ExistingFsDaxPath(PathBuf),
    FsDaxRegions(Vec<TMemoryRegionConfig>),
    ExistingFsDaxRegions(Vec<TMemoryRegionConfig>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TMemoryFileBacking {
    Temp,
    Path(PathBuf),
    ExistingPath(PathBuf),
    Regions(Vec<TMemoryRegionConfig>),
    ExistingRegions(Vec<TMemoryRegionConfig>),
}

fn validate_tmemory_regions(
    regions: &[TMemoryRegionConfig],
    empty_path_message: &'static str,
    backend: RegionBackendKind,
) -> Result<()> {
    ensure!(
        !regions.is_empty(),
        "multi-region tmemory configuration requires at least one region"
    );

    let descriptors = regions
        .iter()
        .enumerate()
        .map(|(index, region)| {
            let id = u32::try_from(index).context("too many regions in tmemory configuration")?;

            ensure!(!region.path.as_os_str().is_empty(), "{empty_path_message}");
            Ok(RegionDescriptor {
                id: RegionId(id),
                backend,
                path: region.path.clone(),
                window: RegionAddressWindow {
                    base: region.address_base,
                    reserved_len: region.reserved_len,
                    mapped_len: region.reserved_len,
                },
                numa_node: region.numa_node,
                cpu_set: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let _ = RegionSetDescriptor::new(descriptors)?;

    Ok(())
}

/// SHISOFT-TWASM-MOCK: selectable concurrency policy shape. Policy selection is
/// currently compile-time for branch experiments rather than public API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConcurrencyControl {
    LockBased,
    NoWaitAbort,
    StrictTwoPhaseLocking,
    WoundWait,
    WaitDie,
    OptimisticValidation,
    TimestampOrdering,
}

impl ConcurrencyControl {
    pub(crate) const fn default_for_build() -> Self {
        #[cfg(feature = "transaction-cc-timestamp-ordering")]
        {
            return Self::TimestampOrdering;
        }

        #[cfg(feature = "transaction-cc-optimistic-validation")]
        {
            return Self::OptimisticValidation;
        }

        #[cfg(feature = "transaction-cc-wait-die")]
        {
            return Self::WaitDie;
        }

        #[cfg(feature = "transaction-cc-wound-wait")]
        {
            return Self::WoundWait;
        }

        #[cfg(feature = "transaction-cc-strict-2pl")]
        {
            return Self::StrictTwoPhaseLocking;
        }

        #[cfg(feature = "transaction-cc-nowait-abort")]
        {
            return Self::NoWaitAbort;
        }

        #[cfg(feature = "transaction-cc-lockbased")]
        {
            return Self::LockBased;
        }

        #[cfg(not(any(
            feature = "transaction-cc-wait-die",
            feature = "transaction-cc-wound-wait",
            feature = "transaction-cc-strict-2pl",
            feature = "transaction-cc-nowait-abort",
            feature = "transaction-cc-lockbased",
            feature = "transaction-cc-optimistic-validation",
            feature = "transaction-cc-timestamp-ordering"
        )))]
        {
            return Self::LockBased;
        }
    }

    pub(crate) fn ensure_compiled_for_build(self) -> Result<()> {
        let selected = Self::default_for_build();
        ensure!(
            self == selected,
            "transaction concurrency-control policy {self:?} is not compiled into this build; selected policy is {selected:?}"
        );
        Ok(())
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
    tmemory_dax_pmem_backing: Option<TMemoryDaxPmemBacking>,
    concurrency_control: ConcurrencyControl,
    durability_policy: DurabilityPolicy,
    conflict_policy: ConflictPolicy,
    object_index_persistence_policy: ObjectIndexPersistencePolicy,
}

impl Default for TransactionConfig {
    fn default() -> Self {
        Self {
            tmemory_backend: TMemoryBackend::VMemory,
            tmemory_persistence_mode: TMemoryPersistenceMode::ResearchPretendDaxPmem,
            tmemory_file_backing: None,
            tmemory_dax_pmem_backing: None,
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

    pub(crate) fn with_file_backed_tmemory_temp() -> Result<Self> {
        let mut config = Self::default();
        config.set_file_backed_tmemory(TMemoryFileBacking::Temp)?;
        Ok(config)
    }

    pub(crate) fn with_dax_pmem_fsdax_path(path: PathBuf) -> Result<Self> {
        ensure!(
            !path.as_os_str().is_empty(),
            "DAX PMEM fsdax path cannot be empty"
        );
        let mut config = Self::default();
        config.set_dax_pmem_backing(TMemoryDaxPmemBacking::FsDaxPath(path))?;
        Ok(config)
    }

    pub(crate) fn with_dax_pmem_existing_fsdax_path(path: PathBuf) -> Result<Self> {
        ensure!(
            !path.as_os_str().is_empty(),
            "DAX PMEM fsdax path cannot be empty"
        );
        let mut config = Self::default();
        config.set_dax_pmem_backing(TMemoryDaxPmemBacking::ExistingFsDaxPath(path))?;
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

    pub(crate) fn with_dax_pmem_fsdax_regions(regions: Vec<TMemoryRegionConfig>) -> Result<Self> {
        validate_tmemory_regions(
            &regions,
            "DAX PMEM fsdax path cannot be empty",
            RegionBackendKind::DaxPmem,
        )?;
        let mut config = Self::default();
        config.set_dax_pmem_backing(TMemoryDaxPmemBacking::FsDaxRegions(regions))?;
        Ok(config)
    }

    pub(crate) fn with_dax_pmem_existing_fsdax_regions(
        regions: Vec<TMemoryRegionConfig>,
    ) -> Result<Self> {
        validate_tmemory_regions(
            &regions,
            "DAX PMEM fsdax path cannot be empty",
            RegionBackendKind::DaxPmem,
        )?;
        let mut config = Self::default();
        config.set_dax_pmem_backing(TMemoryDaxPmemBacking::ExistingFsDaxRegions(regions))?;
        Ok(config)
    }

    pub(crate) fn with_file_backed_tmemory_regions(
        regions: Vec<TMemoryRegionConfig>,
    ) -> Result<Self> {
        validate_tmemory_regions(
            &regions,
            "file-backed tmemory path cannot be empty",
            RegionBackendKind::FileBacked,
        )?;
        let mut config = Self::default();
        config.set_file_backed_tmemory(TMemoryFileBacking::Regions(regions))?;
        Ok(config)
    }

    pub(crate) fn with_file_backed_tmemory_existing_regions(
        regions: Vec<TMemoryRegionConfig>,
    ) -> Result<Self> {
        validate_tmemory_regions(
            &regions,
            "file-backed tmemory path cannot be empty",
            RegionBackendKind::FileBacked,
        )?;
        let mut config = Self::default();
        config.set_file_backed_tmemory(TMemoryFileBacking::ExistingRegions(regions))?;
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

    pub(crate) fn tmemory_dax_pmem_backing(&self) -> Option<TMemoryDaxPmemBacking> {
        self.tmemory_dax_pmem_backing.clone()
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

    pub(crate) fn has_path_backed_dax_pmem_tmemory(&self) -> bool {
        self.tmemory_backend == TMemoryBackend::DaxPmem
            && matches!(
                self.tmemory_dax_pmem_backing.as_ref(),
                Some(TMemoryDaxPmemBacking::FsDaxPath(_))
                    | Some(TMemoryDaxPmemBacking::ExistingFsDaxPath(_))
                    | Some(TMemoryDaxPmemBacking::FsDaxRegions(_))
                    | Some(TMemoryDaxPmemBacking::ExistingFsDaxRegions(_))
            )
    }

    pub(crate) fn has_existing_dax_pmem_tmemory(&self) -> bool {
        self.tmemory_backend == TMemoryBackend::DaxPmem
            && matches!(
                self.tmemory_dax_pmem_backing.as_ref(),
                Some(TMemoryDaxPmemBacking::ExistingFsDaxPath(_))
                    | Some(TMemoryDaxPmemBacking::ExistingFsDaxRegions(_))
            )
    }

    fn set_tmemory_backend(&mut self, tmemory_backend: TMemoryBackend) -> Result<()> {
        match tmemory_backend {
            TMemoryBackend::VMemory => {
                self.tmemory_backend = tmemory_backend;
                self.tmemory_persistence_mode = TMemoryPersistenceMode::ResearchPretendDaxPmem;
                self.tmemory_file_backing = None;
                self.tmemory_dax_pmem_backing = None;
                Ok(())
            }
            TMemoryBackend::DaxPmem => {
                self.set_dax_pmem_backing(TMemoryDaxPmemBacking::ResearchTemp)
            }
            TMemoryBackend::FileBackedMemory => {
                bail!("FileBackedMemory requires explicit file backing configuration")
            }
        }
    }

    fn set_file_backed_tmemory(&mut self, file_backing: TMemoryFileBacking) -> Result<()> {
        self.tmemory_backend = TMemoryBackend::FileBackedMemory;
        self.tmemory_file_backing = Some(file_backing);
        self.tmemory_dax_pmem_backing = None;
        Ok(())
    }

    fn set_dax_pmem_backing(&mut self, backing: TMemoryDaxPmemBacking) -> Result<()> {
        self.tmemory_backend = TMemoryBackend::DaxPmem;
        self.tmemory_persistence_mode = match &backing {
            TMemoryDaxPmemBacking::ResearchTemp => TMemoryPersistenceMode::ResearchPretendDaxPmem,
            TMemoryDaxPmemBacking::FsDaxPath(_)
            | TMemoryDaxPmemBacking::ExistingFsDaxPath(_)
            | TMemoryDaxPmemBacking::FsDaxRegions(_)
            | TMemoryDaxPmemBacking::ExistingFsDaxRegions(_) => {
                TMemoryPersistenceMode::RequireDaxPmem
            }
        };
        self.tmemory_file_backing = None;
        self.tmemory_dax_pmem_backing = Some(backing);
        Ok(())
    }

    fn set_concurrency_control(&mut self, concurrency_control: ConcurrencyControl) -> Result<()> {
        concurrency_control.ensure_compiled_for_build()?;
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
