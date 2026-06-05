#![allow(dead_code)]

use crate::prelude::*;
use crate::runtime::store::InstanceId;
use crate::runtime::vm::TMemory;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::ops::Range;

// SHISOFT-TWASM-MOCK: milestone runtime scaffold for proposal WAST progress.
// The current runtime uses store-local transaction state, VMemory-only backend
// selection, and ordinary Wasmtime memory/global backing until the real
// transactional object table and tmemory allocation path are wired.

/// Storage backend selected for transactional memories.
///
/// SHISOFT-TWASM-MOCK: selectable backend shape is present, but only `VMemory`
/// is currently accepted by `TransactionConfig`.
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

/// SHISOFT-TWASM-MOCK: selectable concurrency policy shape. `LockBased` is
/// store-local today and must move behind the future transaction object table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConcurrencyControl {
    LockBased,
}

/// SHISOFT-TWASM-MOCK: durability is volatile rollback-only until tmemory can
/// select FileBackedMemory or NVMemory storage.
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

impl TransactionId {
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub(crate) fn as_raw(self) -> u64 {
        self.0
    }
}

#[derive(Debug)]
pub(crate) struct TransactionState {
    active: Option<TransactionId>,
    next_id: u64,
    locks: LockBased,
    staged_globals: BTreeMap<GranuleId, GlobalSnapshot>,
    staged_granules: BTreeMap<GranuleId, Vec<u8>>,
    staged_memory_sizes: BTreeMap<GranuleId, u64>,
    memory_read_granules: BTreeSet<GranuleId>,
    memory_write_granules: BTreeSet<GranuleId>,
    table_read_granules: BTreeSet<GranuleId>,
    table_write_granules: BTreeSet<GranuleId>,
    scratch: Vec<u8>,
    pending_memory_store: Option<PendingMemoryStore>,
}

impl Default for TransactionState {
    fn default() -> Self {
        Self {
            active: None,
            next_id: 1,
            locks: LockBased::default(),
            staged_globals: BTreeMap::new(),
            staged_granules: BTreeMap::new(),
            staged_memory_sizes: BTreeMap::new(),
            memory_read_granules: BTreeSet::new(),
            memory_write_granules: BTreeSet::new(),
            table_read_granules: BTreeSet::new(),
            table_write_granules: BTreeSet::new(),
            scratch: Vec::new(),
            pending_memory_store: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StagedRecord {
    Global {
        owner_instance: Option<InstanceId>,
        global_index: u32,
        value: GlobalSnapshot,
    },
    MemoryGranule {
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        granule_index: u64,
        bytes: Vec<u8>,
    },
    MemorySize {
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        new_pages: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GlobalSnapshot {
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
    V128([u8; 16]),
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ObjectId {
    pub(crate) object_index: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum GranuleId {
    TMemory {
        instance: Option<u32>,
        memory_index: u32,
        granule_index: u64,
    },
    TMemorySize {
        instance: Option<u32>,
        memory_index: u32,
    },
    TGlobal {
        instance: Option<u32>,
        global_index: u32,
    },
    TTable {
        instance: Option<u32>,
        table_index: u32,
        granule_index: u64,
    },
    TTableSize {
        instance: Option<u32>,
        table_index: u32,
    },
    // Future object-table identities; not wired into runtime paths yet.
    #[allow(dead_code)]
    TStruct { object_id: ObjectId },
    #[allow(dead_code)]
    TArray { object_id: ObjectId },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingMemoryStore {
    instance: InstanceId,
    memory_index: u32,
    addr: u64,
    len: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct TMemoryGranuleSnapshot {
    granule_index: usize,
    range: Range<usize>,
    version: u64,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub(crate) struct TMemoryAccessSnapshot {
    base: u64,
    bytes: Vec<u8>,
    byte_len: usize,
    granules: Vec<TMemoryGranuleSnapshot>,
}

const TMEMORY_GRANULE_SHIFT: usize = 8;
pub(crate) const TMEMORY_GRANULE_SIZE: usize = 1 << TMEMORY_GRANULE_SHIFT;
const TTABLE_GRANULE_SHIFT: u32 = 4;
pub(crate) const TTABLE_GRANULE_SIZE: u64 = 1 << TTABLE_GRANULE_SHIFT;

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
    // SHISOFT-TWASM-MOCK: the real tmemory path still feeds store-local
    // `instance: None`/version `0` through `TransactionConcurrencyControl`.
    owners: BTreeMap<GranuleId, TransactionId>,
    read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
}

impl LockBased {
    pub(crate) fn record_read(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> Result<()> {
        if self
            .owners
            .get(&granule)
            .is_some_and(|owner| *owner != transaction)
        {
            bail!("transaction read conflict: granule is owned by another transaction");
        }

        match self.read_versions.entry((transaction, granule)) {
            alloc::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(version);
            }
            alloc::collections::btree_map::Entry::Occupied(entry) => {
                ensure!(
                    *entry.get() == version,
                    "transaction read conflict: optimistic read version changed"
                );
            }
        }

        Ok(())
    }

    pub(crate) fn acquire_write(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        if self
            .owners
            .get(&granule)
            .is_some_and(|owner| *owner != transaction)
        {
            bail!("transaction write conflict: granule is owned by another transaction");
        }
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            bail!("transaction write conflict: optimistic read version changed");
        }

        self.owners.insert(granule, transaction);
        Ok(())
    }

    pub(crate) fn validate_read(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        if self
            .owners
            .get(&granule)
            .is_some_and(|owner| *owner != transaction)
        {
            bail!("transaction read conflict: granule is owned by another transaction");
        }
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            bail!("transaction read conflict: optimistic read version changed");
        }

        Ok(())
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

    pub(crate) fn release_transaction(&mut self, transaction: TransactionId) {
        self.owners.retain(|_, owner| *owner != transaction);
        self.read_versions
            .retain(|(reader, _), _| *reader != transaction);
    }
}

#[cfg(test)]
impl LockBased {
    fn record_read_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> Result<()> {
        self.record_read(transaction, granule, version)
    }

    fn acquire_write_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.acquire_write(transaction, granule, current_version)
    }

    fn validate_read_for_test(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.validate_read(transaction, granule, current_version)
    }

    fn abort_for_test(&mut self, transaction: TransactionId) {
        self.release_transaction(transaction);
    }

    fn owner_for_test(&self, granule: GranuleId) -> Option<TransactionId> {
        self.owners.get(&granule).copied()
    }
}

impl TransactionConcurrencyControl for LockBased {
    fn acquire_memory_granule_read(
        &mut self,
        transaction: TransactionId,
        memory_index: u32,
        granule_index: u64,
    ) -> Result<()> {
        self.record_read(
            transaction,
            GranuleId::TMemory {
                instance: None,
                memory_index,
                granule_index,
            },
            0,
        )
    }

    fn acquire_memory_granule_write(
        &mut self,
        transaction: TransactionId,
        memory_index: u32,
        granule_index: u64,
    ) -> Result<()> {
        self.acquire_write(
            transaction,
            GranuleId::TMemory {
                instance: None,
                memory_index,
                granule_index,
            },
            0,
        )
    }

    fn release_transaction(&mut self, transaction: TransactionId) {
        LockBased::release_transaction(self, transaction);
    }
}

impl TransactionState {
    pub(crate) fn begin(&mut self) -> Result<TransactionId> {
        ensure!(
            self.active.is_none(),
            "transaction is already active in this store"
        );
        let id = TransactionId::from_raw(self.next_id);
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
        self.commit_with_read_validation(|_| Ok(0), |record| apply(record))
    }

    pub(crate) fn commit_with_read_validation<V, F>(
        &mut self,
        current_version_fn: V,
        mut apply: F,
    ) -> Result<()>
    where
        V: FnMut(GranuleId) -> Result<u64>,
        F: FnMut(&StagedRecord) -> Result<()>,
    {
        self.ensure_active()?;
        self.validate_active_reads_with(current_version_fn)?;
        for record in self.staged_records()? {
            apply(&record)?;
        }
        self.complete_commit()
    }

    pub(crate) fn validate_active_reads_with<F>(&self, current_version_fn: F) -> Result<()>
    where
        F: FnMut(GranuleId) -> Result<u64>,
    {
        let transaction = self.active_transaction_required()?;
        self.locks
            .validate_transaction_reads(transaction, current_version_fn)
    }

    pub(crate) fn active_read_granules(&self) -> Result<Vec<GranuleId>> {
        let transaction = self.active_transaction_required()?;
        Ok(self
            .locks
            .read_versions
            .keys()
            .filter_map(|(reader, granule)| (*reader == transaction).then_some(*granule))
            .collect())
    }

    pub(crate) fn validate_active_read(
        &self,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        let transaction = self.active_transaction_required()?;
        self.locks
            .validate_read(transaction, granule, current_version)
    }

    pub(crate) fn complete_commit(&mut self) -> Result<()> {
        self.ensure_active()?;
        self.clear_active();
        Ok(())
    }

    pub(crate) fn staged_records(&self) -> Result<Vec<StagedRecord>> {
        self.ensure_active()?;
        let mut records = Vec::new();
        for (key, &value) in &self.staged_globals {
            let GranuleId::TGlobal {
                instance,
                global_index,
            } = *key
            else {
                bail!("staged global map contains non-global key");
            };
            records.push(StagedRecord::Global {
                owner_instance: granule_owner_instance(instance),
                global_index,
                value,
            });
        }
        for (key, bytes) in &self.staged_granules {
            let GranuleId::TMemory {
                instance,
                memory_index,
                granule_index,
            } = *key
            else {
                bail!("staged granule map contains non-memory key");
            };
            records.push(StagedRecord::MemoryGranule {
                owner_instance: granule_owner_instance(instance),
                memory_index,
                granule_index,
                bytes: bytes.clone(),
            });
        }
        for (key, &new_pages) in &self.staged_memory_sizes {
            let GranuleId::TMemorySize {
                instance,
                memory_index,
            } = *key
            else {
                bail!("staged memory size map contains non-size key");
            };
            records.push(StagedRecord::MemorySize {
                owner_instance: granule_owner_instance(instance),
                memory_index,
                new_pages,
            });
        }
        Ok(records)
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
        self.stage_memory_size_owned(None, memory_index, new_pages)
    }

    pub(crate) fn stage_memory_size_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        new_pages: u64,
    ) -> Result<bool> {
        self.ensure_active()?;
        Ok(self
            .staged_memory_sizes
            .insert(
                memory_size_granule_id(owner_instance, memory_index),
                new_pages,
            )
            .is_none())
    }

    pub(crate) fn stage_global(
        &mut self,
        global_index: u32,
        value: GlobalSnapshot,
    ) -> Result<bool> {
        self.stage_global_owned(None, global_index, value)
    }

    pub(crate) fn stage_global_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        global_index: u32,
        value: GlobalSnapshot,
    ) -> Result<bool> {
        self.ensure_active()?;
        Ok(self
            .staged_globals
            .insert(global_granule_id(owner_instance, global_index), value)
            .is_none())
    }

    pub(crate) fn staged_global_owned(
        &self,
        owner_instance: Option<InstanceId>,
        global_index: u32,
    ) -> Option<GlobalSnapshot> {
        self.staged_globals
            .get(&global_granule_id(owner_instance, global_index))
            .copied()
    }

    pub(crate) fn stage_memory_granule(
        &mut self,
        memory_index: u32,
        granule_index: u64,
        bytes: Vec<u8>,
    ) -> Result<bool> {
        self.ensure_active()?;
        self.lock_memory_granule_write(memory_index, granule_index)?;
        let key = memory_granule_id_from_u64(None, memory_index, granule_index);
        self.memory_read_granules.insert(key);
        self.memory_write_granules.insert(key);
        Ok(self.staged_granules.insert(key, bytes).is_none())
    }

    pub(crate) fn stage_memory_write(
        &mut self,
        memory_index: u32,
        addr: u64,
        bytes: &[u8],
        backing: &[u8],
    ) -> Result<()> {
        self.stage_memory_write_owned(None, memory_index, addr, bytes, backing)
    }

    pub(crate) fn stage_memory_write_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        bytes: &[u8],
        backing: &[u8],
    ) -> Result<()> {
        self.stage_memory_write_owned_from_backing(
            owner_instance,
            memory_index,
            addr,
            bytes,
            0,
            backing,
            backing.len(),
        )
    }

    pub(crate) fn stage_memory_write_owned_from_backing(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        bytes: &[u8],
        backing_base: u64,
        backing: &[u8],
        memory_len: usize,
    ) -> Result<()> {
        self.ensure_active()?;
        let range = checked_tmemory_range(addr, bytes.len(), memory_len)?;
        let backing_base = usize::try_from(backing_base)
            .context("tmemory backing base does not fit host usize")?;
        let backing_end = backing_base
            .checked_add(backing.len())
            .context("tmemory backing window overflow")?;
        let mut offset: usize = 0;
        let mut current = range.start;

        while current < range.end {
            let granule_index = current / TMEMORY_GRANULE_SIZE;
            let granule_range = tmemory_granule_backing_range(granule_index, memory_len)?;
            ensure!(
                granule_range.start >= backing_base && granule_range.end <= backing_end,
                "tmemory backing window does not cover touched granule"
            );
            let local_granule_range =
                (granule_range.start - backing_base)..(granule_range.end - backing_base);
            let key = memory_granule_key(owner_instance, memory_index, granule_index)?;
            let granule_index =
                u64::try_from(granule_index).context("tmemory granule index does not fit u64")?;
            let granule_offset = current - granule_range.start;
            let chunk_len = (range.end - current).min(granule_range.end - current);
            let granule_chunk_end = granule_offset
                .checked_add(chunk_len)
                .context("tmemory granule write offset overflow")?;
            let write_chunk_end = offset
                .checked_add(chunk_len)
                .context("tmemory write offset overflow")?;

            self.lock_memory_granule_write(memory_index, granule_index)?;
            {
                let staged = self
                    .staged_granules
                    .entry(key)
                    .or_insert_with(|| backing[local_granule_range.clone()].to_vec());
                ensure!(
                    granule_chunk_end <= staged.len(),
                    "staged tmemory granule length mismatch"
                );
                staged[granule_offset..granule_chunk_end]
                    .copy_from_slice(&bytes[offset..write_chunk_end]);
            }

            self.memory_read_granules.insert(key);
            self.memory_write_granules.insert(key);
            current += chunk_len;
            offset = write_chunk_end;
        }

        Ok(())
    }

    pub(crate) fn stage_tmemory_write_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        bytes: &[u8],
        tmemory: &TMemory,
    ) -> Result<()> {
        let snapshot = collect_tmemory_access_snapshot(tmemory, addr, bytes.len())?;
        self.stage_tmemory_write_owned_from_snapshot(
            owner_instance,
            memory_index,
            addr,
            bytes,
            &snapshot,
        )
    }

    pub(crate) fn stage_tmemory_write_owned_from_snapshot(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        bytes: &[u8],
        snapshot: &TMemoryAccessSnapshot,
    ) -> Result<()> {
        self.ensure_active()?;
        let range = checked_tmemory_range(addr, bytes.len(), snapshot.byte_len)?;
        let transaction = self.active_transaction_required()?;
        let mut offset: usize = 0;
        let mut current = range.start;

        for granule in &snapshot.granules {
            if current >= range.end {
                break;
            }

            let key = memory_granule_key(owner_instance, memory_index, granule.granule_index)?;
            let granule_offset = current - granule.range.start;
            let chunk_len = (range.end - current).min(granule.range.end - current);
            let granule_chunk_end = granule_offset
                .checked_add(chunk_len)
                .context("tmemory granule write offset overflow")?;
            let write_chunk_end = offset
                .checked_add(chunk_len)
                .context("tmemory write offset overflow")?;

            self.locks
                .acquire_write(transaction, key, granule.version)?;
            {
                let staged = self
                    .staged_granules
                    .entry(key)
                    .or_insert_with(|| granule.bytes.clone());
                ensure!(
                    granule_chunk_end <= staged.len(),
                    "staged tmemory granule length mismatch"
                );
                staged[granule_offset..granule_chunk_end]
                    .copy_from_slice(&bytes[offset..write_chunk_end]);
            }

            self.memory_read_granules.insert(key);
            self.memory_write_granules.insert(key);
            current += chunk_len;
            offset = write_chunk_end;
        }

        Ok(())
    }

    pub(crate) fn read_memory_overlay(
        &mut self,
        memory_index: u32,
        addr: u64,
        len: usize,
        backing: &[u8],
    ) -> Result<Vec<u8>> {
        self.read_memory_overlay_owned(None, memory_index, addr, len, backing)
    }

    pub(crate) fn read_memory_overlay_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        len: usize,
        backing: &[u8],
    ) -> Result<Vec<u8>> {
        self.read_memory_overlay_owned_from_backing(
            owner_instance,
            memory_index,
            addr,
            len,
            0,
            backing,
            backing.len(),
        )
    }

    pub(crate) fn read_memory_overlay_owned_from_backing(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        len: usize,
        backing_base: u64,
        backing: &[u8],
        memory_len: usize,
    ) -> Result<Vec<u8>> {
        self.ensure_active()?;
        let range = checked_tmemory_range(addr, len, memory_len)?;
        let bytes = self.merge_staged_tmemory_range(
            granule_instance(owner_instance),
            memory_index,
            addr,
            len,
            backing_base,
            backing,
            memory_len,
        )?;
        let mut current = range.start;

        while current < range.end {
            let granule_index = current / TMEMORY_GRANULE_SIZE;
            let granule_range = tmemory_granule_backing_range(granule_index, memory_len)?;
            let key = memory_granule_key(owner_instance, memory_index, granule_index)?;
            let granule_index =
                u64::try_from(granule_index).context("tmemory granule index does not fit u64")?;
            let chunk_len = (range.end - current).min(granule_range.end - current);

            self.lock_memory_granule_read(memory_index, granule_index)?;
            self.memory_read_granules.insert(key);
            current += chunk_len;
        }

        Ok(bytes)
    }

    pub(crate) fn read_tmemory_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        len: usize,
        tmemory: &TMemory,
    ) -> Result<Vec<u8>> {
        let snapshot = collect_tmemory_access_snapshot(tmemory, addr, len)?;
        self.read_tmemory_owned_from_snapshot(owner_instance, memory_index, addr, len, &snapshot)
    }

    pub(crate) fn read_tmemory_owned_from_snapshot(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        len: usize,
        snapshot: &TMemoryAccessSnapshot,
    ) -> Result<Vec<u8>> {
        self.ensure_active()?;
        let range = checked_tmemory_range(addr, len, snapshot.byte_len)?;
        let transaction = self.active_transaction_required()?;
        let bytes = self.merge_staged_tmemory_range(
            granule_instance(owner_instance),
            memory_index,
            addr,
            len,
            snapshot.base,
            &snapshot.bytes,
            snapshot.byte_len,
        )?;
        let mut current = range.start;

        for granule in &snapshot.granules {
            if current >= range.end {
                break;
            }

            let key = memory_granule_key(owner_instance, memory_index, granule.granule_index)?;
            let chunk_len = (range.end - current).min(granule.range.end - current);

            self.locks.record_read(transaction, key, granule.version)?;
            self.memory_read_granules.insert(key);
            current += chunk_len;
        }

        Ok(bytes)
    }

    pub(crate) fn set_scratch(&mut self, bytes: Vec<u8>) -> *mut u8 {
        debug_assert!(self.pending_memory_store.is_none());
        self.scratch = bytes;
        self.scratch.as_mut_ptr()
    }

    pub(crate) fn set_memory_store_scratch(
        &mut self,
        instance: InstanceId,
        memory_index: u32,
        addr: u64,
        bytes: Vec<u8>,
    ) -> Result<*mut u8> {
        self.ensure_active()?;
        let len = bytes.len();
        self.acquire_memory_write_ownership_range_owned(Some(instance), memory_index, addr, len)?;
        self.scratch = bytes;
        self.pending_memory_store = Some(PendingMemoryStore {
            instance,
            memory_index,
            addr,
            len,
        });
        Ok(self.scratch.as_mut_ptr())
    }

    pub(crate) fn set_tmemory_store_scratch(
        &mut self,
        instance: InstanceId,
        memory_index: u32,
        addr: u64,
        bytes: Vec<u8>,
    ) -> Result<*mut u8> {
        self.ensure_active()?;
        debug_assert!(self.pending_memory_store.is_none());
        self.scratch = bytes;
        self.pending_memory_store = Some(PendingMemoryStore {
            instance,
            memory_index,
            addr,
            len: self.scratch.len(),
        });
        Ok(self.scratch.as_mut_ptr())
    }

    pub(crate) fn pending_memory_store(&self) -> Option<(InstanceId, u32, u64, usize)> {
        self.pending_memory_store.map(|pending| {
            (
                pending.instance,
                pending.memory_index,
                pending.addr,
                pending.len,
            )
        })
    }

    pub(crate) fn flush_memory_store_scratch(&mut self, backing: &[u8]) -> Result<bool> {
        self.flush_memory_store_scratch_from_backing(0, backing, backing.len())
    }

    pub(crate) fn flush_memory_store_scratch_from_backing(
        &mut self,
        backing_base: u64,
        backing: &[u8],
        memory_len: usize,
    ) -> Result<bool> {
        self.ensure_active()?;
        let Some(pending) = self.pending_memory_store.take() else {
            return Ok(false);
        };
        ensure!(
            self.scratch.len() == pending.len,
            "pending tmemory store scratch length mismatch"
        );
        let bytes = self.scratch[..pending.len].to_vec();
        self.stage_memory_write_owned_from_backing(
            Some(pending.instance),
            pending.memory_index,
            pending.addr,
            &bytes,
            backing_base,
            backing,
            memory_len,
        )?;
        Ok(true)
    }

    pub(crate) fn flush_tmemory_store_scratch_from_snapshot(
        &mut self,
        snapshot: &TMemoryAccessSnapshot,
    ) -> Result<bool> {
        self.ensure_active()?;
        let Some(pending) = self.pending_memory_store.take() else {
            return Ok(false);
        };
        ensure!(
            self.scratch.len() == pending.len,
            "pending tmemory store scratch length mismatch"
        );
        let bytes = self.scratch[..pending.len].to_vec();
        self.stage_tmemory_write_owned_from_snapshot(
            Some(pending.instance),
            pending.memory_index,
            pending.addr,
            &bytes,
            snapshot,
        )?;
        Ok(true)
    }

    pub(crate) fn commit_tmemory_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        tmemory: &mut TMemory,
    ) -> Result<bool> {
        self.ensure_active()?;
        let staged = self.staged_tmemory_granules_owned(owner_instance, memory_index)?;
        if staged.is_empty() {
            return Ok(false);
        }

        for (_, granule_index, bytes) in &staged {
            let addr = granule_index
                .checked_mul(TMEMORY_GRANULE_SIZE)
                .context("tmemory granule byte offset overflow")?;
            tmemory.commit_range(addr, bytes)?;
        }

        self.remove_staged_tmemory_granules_owned(owner_instance, memory_index)?;
        Ok(true)
    }

    pub(crate) fn staged_memory_granule(
        &self,
        memory_index: u32,
        granule_index: u64,
    ) -> Option<&[u8]> {
        self.staged_granules
            .get(&memory_granule_id_from_u64(
                None,
                memory_index,
                granule_index,
            ))
            .map(Vec::as_slice)
    }

    pub(crate) fn staged_memory_granule_mut(
        &mut self,
        memory_index: u32,
        granule_index: u64,
    ) -> Option<&mut [u8]> {
        self.staged_granules
            .get_mut(&memory_granule_id_from_u64(
                None,
                memory_index,
                granule_index,
            ))
            .map(Vec::as_mut_slice)
    }

    pub(crate) fn acquire_memory_granule_read(
        &mut self,
        memory_index: u32,
        granule_index: u64,
    ) -> Result<bool> {
        self.acquire_memory_granule_read_owned(None, memory_index, granule_index)
    }

    pub(crate) fn acquire_memory_granule_read_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        granule_index: u64,
    ) -> Result<bool> {
        self.ensure_active()?;
        self.lock_memory_granule_read(memory_index, granule_index)?;
        Ok(self.memory_read_granules.insert(memory_granule_id_from_u64(
            owner_instance,
            memory_index,
            granule_index,
        )))
    }

    pub(crate) fn acquire_memory_granule_write(
        &mut self,
        memory_index: u32,
        granule_index: u64,
        bytes: Vec<u8>,
    ) -> Result<bool> {
        self.acquire_memory_granule_write_owned(None, memory_index, granule_index, bytes)
    }

    pub(crate) fn acquire_memory_granule_write_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        granule_index: u64,
        bytes: Vec<u8>,
    ) -> Result<bool> {
        self.ensure_active()?;
        self.lock_memory_granule_write(memory_index, granule_index)?;
        let key = memory_granule_id_from_u64(owner_instance, memory_index, granule_index);
        self.memory_read_granules.insert(key);
        self.memory_write_granules.insert(key);
        Ok(self.staged_granules.insert(key, bytes).is_none())
    }

    fn acquire_memory_write_ownership_range_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        len: usize,
    ) -> Result<()> {
        let start = usize::try_from(addr).context("tmemory address does not fit host usize")?;
        let end = start
            .checked_add(len)
            .context("tmemory pending store range overflow")?;
        let mut current = start;

        while current < end {
            let granule_index = current / TMEMORY_GRANULE_SIZE;
            let granule_end = granule_index
                .checked_add(1)
                .and_then(|index| index.checked_mul(TMEMORY_GRANULE_SIZE))
                .context("tmemory pending store granule overflow")?;
            let key = memory_granule_key(owner_instance, memory_index, granule_index)?;
            let granule_index =
                u64::try_from(granule_index).context("tmemory granule index does not fit u64")?;
            self.lock_memory_granule_write(memory_index, granule_index)?;
            self.memory_read_granules.insert(key);
            self.memory_write_granules.insert(key);
            current = end.min(granule_end);
        }

        Ok(())
    }

    fn lock_memory_granule_read(&mut self, memory_index: u32, granule_index: u64) -> Result<()> {
        let transaction = self.active_transaction_required()?;
        self.locks
            .acquire_memory_granule_read(transaction, memory_index, granule_index)
    }

    fn lock_memory_granule_write(&mut self, memory_index: u32, granule_index: u64) -> Result<()> {
        let transaction = self.active_transaction_required()?;
        self.locks
            .acquire_memory_granule_write(transaction, memory_index, granule_index)
    }

    pub(crate) fn owns_memory_granule_read(&self, memory_index: u32, granule_index: u64) -> bool {
        self.owns_memory_granule_read_owned(None, memory_index, granule_index)
    }

    pub(crate) fn owns_memory_granule_read_owned(
        &self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        granule_index: u64,
    ) -> bool {
        self.memory_read_granules
            .contains(&memory_granule_id_from_u64(
                owner_instance,
                memory_index,
                granule_index,
            ))
    }

    pub(crate) fn owns_memory_granule_write(&self, memory_index: u32, granule_index: u64) -> bool {
        self.owns_memory_granule_write_owned(None, memory_index, granule_index)
    }

    pub(crate) fn owns_memory_granule_write_owned(
        &self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        granule_index: u64,
    ) -> bool {
        self.memory_write_granules
            .contains(&memory_granule_id_from_u64(
                owner_instance,
                memory_index,
                granule_index,
            ))
    }

    pub(crate) fn acquire_table_granule_read_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        element_index: u64,
        current_version: u64,
    ) -> Result<bool> {
        self.ensure_active()?;
        let transaction = self.active_transaction_required()?;
        let key = table_granule_id(owner_instance, table_index, element_index);
        self.locks.record_read(transaction, key, current_version)?;
        Ok(self.table_read_granules.insert(key))
    }

    pub(crate) fn acquire_table_granule_write_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        element_index: u64,
        current_version: u64,
    ) -> Result<bool> {
        self.ensure_active()?;
        let transaction = self.active_transaction_required()?;
        let key = table_granule_id(owner_instance, table_index, element_index);
        self.locks
            .acquire_write(transaction, key, current_version)?;
        self.table_read_granules.insert(key);
        Ok(self.table_write_granules.insert(key))
    }

    pub(crate) fn acquire_table_granule_read_range_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        start_element: u64,
        len: u64,
        current_version: u64,
    ) -> Result<bool> {
        self.ensure_active()?;
        if len == 0 {
            return Ok(false);
        }
        let transaction = self.active_transaction_required()?;
        let last_element = start_element
            .checked_add(len - 1)
            .context("ttable read range overflow")?;
        let first_granule = start_element >> TTABLE_GRANULE_SHIFT;
        let last_granule = last_element >> TTABLE_GRANULE_SHIFT;
        let mut inserted = false;
        for granule_index in first_granule..=last_granule {
            let key = table_granule_id_from_u64(owner_instance, table_index, granule_index);
            self.locks.record_read(transaction, key, current_version)?;
            inserted |= self.table_read_granules.insert(key);
        }
        Ok(inserted)
    }

    pub(crate) fn acquire_table_granule_write_range_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        start_element: u64,
        len: u64,
        current_version: u64,
    ) -> Result<bool> {
        self.ensure_active()?;
        if len == 0 {
            return Ok(false);
        }
        let transaction = self.active_transaction_required()?;
        let last_element = start_element
            .checked_add(len - 1)
            .context("ttable write range overflow")?;
        let first_granule = start_element >> TTABLE_GRANULE_SHIFT;
        let last_granule = last_element >> TTABLE_GRANULE_SHIFT;
        let mut inserted = false;
        for granule_index in first_granule..=last_granule {
            let key = table_granule_id_from_u64(owner_instance, table_index, granule_index);
            self.locks
                .acquire_write(transaction, key, current_version)?;
            self.table_read_granules.insert(key);
            inserted |= self.table_write_granules.insert(key);
        }
        Ok(inserted)
    }

    pub(crate) fn acquire_table_size_read_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        current_version: u64,
    ) -> Result<bool> {
        self.ensure_active()?;
        let transaction = self.active_transaction_required()?;
        let key = table_size_granule_id(owner_instance, table_index);
        self.locks.record_read(transaction, key, current_version)?;
        Ok(self.table_read_granules.insert(key))
    }

    pub(crate) fn acquire_table_size_write_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        current_version: u64,
    ) -> Result<bool> {
        self.ensure_active()?;
        let transaction = self.active_transaction_required()?;
        let key = table_size_granule_id(owner_instance, table_index);
        self.locks
            .acquire_write(transaction, key, current_version)?;
        self.table_read_granules.insert(key);
        Ok(self.table_write_granules.insert(key))
    }

    pub(crate) fn owns_table_granule_read_owned(
        &self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        granule_index: u64,
    ) -> bool {
        self.table_read_granules
            .contains(&table_granule_id_from_u64(
                owner_instance,
                table_index,
                granule_index,
            ))
    }

    pub(crate) fn owns_table_granule_write_owned(
        &self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        granule_index: u64,
    ) -> bool {
        self.table_write_granules
            .contains(&table_granule_id_from_u64(
                owner_instance,
                table_index,
                granule_index,
            ))
    }

    pub(crate) fn owns_table_size_read_owned(
        &self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
    ) -> bool {
        self.table_read_granules
            .contains(&table_size_granule_id(owner_instance, table_index))
    }

    pub(crate) fn owns_table_size_write_owned(
        &self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
    ) -> bool {
        self.table_write_granules
            .contains(&table_size_granule_id(owner_instance, table_index))
    }

    fn merge_staged_tmemory_range(
        &self,
        instance: Option<u32>,
        memory_index: u32,
        addr: u64,
        len: usize,
        backing_base: u64,
        backing: &[u8],
        memory_len: usize,
    ) -> Result<Vec<u8>> {
        let range = checked_tmemory_range(addr, len, memory_len)?;
        let backing_base = usize::try_from(backing_base)
            .context("tmemory backing base does not fit host usize")?;
        let backing_end = backing_base
            .checked_add(backing.len())
            .context("tmemory backing window overflow")?;
        ensure!(
            range.start >= backing_base && range.end <= backing_end,
            "tmemory backing window does not cover read range"
        );

        let mut bytes = backing[range.start - backing_base..range.end - backing_base].to_vec();
        let mut current = range.start;

        while current < range.end {
            let granule_index = current / TMEMORY_GRANULE_SIZE;
            let granule_range = tmemory_granule_backing_range(granule_index, memory_len)?;
            let key = memory_granule_id_for_instance(instance, memory_index, granule_index)?;
            let granule_offset = current - granule_range.start;
            let chunk_len = (range.end - current).min(granule_range.end - current);
            let granule_chunk_end = granule_offset
                .checked_add(chunk_len)
                .context("tmemory granule read offset overflow")?;

            if let Some(staged) = self.staged_granules.get(&key) {
                ensure!(
                    granule_chunk_end <= staged.len(),
                    "staged tmemory granule length mismatch"
                );
                let read_offset = current - range.start;
                let read_chunk_end = read_offset
                    .checked_add(chunk_len)
                    .context("tmemory read offset overflow")?;
                bytes[read_offset..read_chunk_end]
                    .copy_from_slice(&staged[granule_offset..granule_chunk_end]);
            }

            current += chunk_len;
        }

        Ok(bytes)
    }

    pub(crate) fn staged_tmemory_granules_owned(
        &self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
    ) -> Result<Vec<(GranuleId, usize, Vec<u8>)>> {
        self.ensure_active()?;
        let owner = granule_instance(owner_instance);
        let mut staged = Vec::new();
        for (key, bytes) in &self.staged_granules {
            let GranuleId::TMemory {
                instance,
                memory_index: staged_memory_index,
                granule_index,
            } = *key
            else {
                continue;
            };
            if instance != owner || staged_memory_index != memory_index {
                continue;
            }
            staged.push((
                key.to_owned(),
                usize::try_from(granule_index)
                    .context("tmemory granule index does not fit host usize")?,
                bytes.clone(),
            ));
        }
        Ok(staged)
    }

    pub(crate) fn remove_staged_tmemory_granules_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
    ) -> Result<()> {
        self.ensure_active()?;
        let owner = granule_instance(owner_instance);
        let keys = self
            .staged_granules
            .keys()
            .filter_map(|key| match *key {
                GranuleId::TMemory {
                    instance,
                    memory_index: staged_memory_index,
                    ..
                } if instance == owner && staged_memory_index == memory_index => Some(*key),
                _ => None,
            })
            .collect::<Vec<_>>();
        for key in keys {
            self.staged_granules.remove(&key);
        }
        Ok(())
    }

    fn active_transaction_required(&self) -> Result<TransactionId> {
        self.active_transaction()
            .context("transaction operation requires an active transaction")
    }

    fn ensure_active(&self) -> Result<()> {
        ensure!(
            self.active.is_some(),
            "transaction operation requires an active transaction"
        );
        Ok(())
    }

    fn clear_active(&mut self) {
        if let Some(transaction) = self.active.take() {
            self.locks.release_transaction(transaction);
        }
        self.staged_globals.clear();
        self.staged_granules.clear();
        self.staged_memory_sizes.clear();
        self.memory_read_granules.clear();
        self.memory_write_granules.clear();
        self.table_read_granules.clear();
        self.table_write_granules.clear();
        self.scratch.clear();
        self.pending_memory_store = None;
    }
}

#[cfg(test)]
impl TransactionState {
    fn new_for_test(transaction: TransactionId) -> Self {
        Self {
            active: Some(transaction),
            next_id: transaction.as_raw().saturating_add(1),
            ..Self::default()
        }
    }

    fn stage_granule_for_test(&mut self, granule: GranuleId, bytes: Vec<u8>) {
        self.staged_granules.insert(granule, bytes);
    }

    fn read_tmemory_range_for_test(
        &self,
        instance: Option<u32>,
        memory_index: u32,
        addr: u64,
        len: usize,
        committed: &[u8],
    ) -> Result<Vec<u8>> {
        self.merge_staged_tmemory_range(
            instance,
            memory_index,
            addr,
            len,
            0,
            committed,
            committed.len(),
        )
    }

    fn stage_tmemory_write_for_test(
        &mut self,
        instance: u32,
        memory_index: u32,
        addr: u64,
        bytes: &[u8],
        tmemory: &TMemory,
    ) -> Result<()> {
        self.stage_tmemory_write_owned(
            Some(InstanceId::from_u32(instance)),
            memory_index,
            addr,
            bytes,
            tmemory,
        )
    }

    fn commit_tmemory_for_test(&mut self, tmemory: &mut TMemory) -> Result<bool> {
        self.commit_tmemory_owned(Some(InstanceId::from_u32(0)), 0, tmemory)
    }
}

pub(crate) fn collect_tmemory_access_snapshot(
    tmemory: &TMemory,
    addr: u64,
    len: usize,
) -> Result<TMemoryAccessSnapshot> {
    let range = checked_tmemory_range(addr, len, tmemory.byte_len())?;
    let bytes = tmemory.read_committed(range.clone())?;
    let mut granules = Vec::new();
    let mut current = range.start;

    while current < range.end {
        let granule_index = current / TMEMORY_GRANULE_SIZE;
        let granule_range = tmemory_granule_backing_range(granule_index, tmemory.byte_len())?;
        granules.push(TMemoryGranuleSnapshot {
            granule_index,
            range: granule_range.clone(),
            version: tmemory.granule_version(granule_index)?,
            bytes: tmemory.read_committed(granule_range)?,
        });
        current = range.end.min(granules.last().unwrap().range.end);
    }

    Ok(TMemoryAccessSnapshot {
        base: addr,
        bytes,
        byte_len: tmemory.byte_len(),
        granules,
    })
}

fn checked_tmemory_range(
    addr: u64,
    len: usize,
    backing_len: usize,
) -> Result<core::ops::Range<usize>> {
    let start = usize::try_from(addr).context("tmemory address does not fit host usize")?;
    let end = start.checked_add(len).context("tmemory address overflow")?;
    ensure!(
        end <= backing_len,
        "out of bounds tmemory access: range {start}..{end} exceeds backing length {backing_len}"
    );
    Ok(start..end)
}

fn granule_instance(owner_instance: Option<InstanceId>) -> Option<u32> {
    owner_instance.map(InstanceId::as_u32)
}

fn granule_owner_instance(instance: Option<u32>) -> Option<InstanceId> {
    instance.map(InstanceId::from_u32)
}

fn global_granule_id(owner_instance: Option<InstanceId>, global_index: u32) -> GranuleId {
    GranuleId::TGlobal {
        instance: granule_instance(owner_instance),
        global_index,
    }
}

fn memory_size_granule_id(owner_instance: Option<InstanceId>, memory_index: u32) -> GranuleId {
    GranuleId::TMemorySize {
        instance: granule_instance(owner_instance),
        memory_index,
    }
}

fn table_granule_id(
    owner_instance: Option<InstanceId>,
    table_index: u32,
    element_index: u64,
) -> GranuleId {
    table_granule_id_from_u64(
        owner_instance,
        table_index,
        element_index >> TTABLE_GRANULE_SHIFT,
    )
}

fn table_granule_id_from_u64(
    owner_instance: Option<InstanceId>,
    table_index: u32,
    granule_index: u64,
) -> GranuleId {
    GranuleId::TTable {
        instance: granule_instance(owner_instance),
        table_index,
        granule_index,
    }
}

fn table_size_granule_id(owner_instance: Option<InstanceId>, table_index: u32) -> GranuleId {
    GranuleId::TTableSize {
        instance: granule_instance(owner_instance),
        table_index,
    }
}

fn memory_granule_id_from_u64(
    owner_instance: Option<InstanceId>,
    memory_index: u32,
    granule_index: u64,
) -> GranuleId {
    GranuleId::TMemory {
        instance: granule_instance(owner_instance),
        memory_index,
        granule_index,
    }
}

fn memory_granule_id_for_instance(
    instance: Option<u32>,
    memory_index: u32,
    granule_index: usize,
) -> Result<GranuleId> {
    Ok(GranuleId::TMemory {
        instance,
        memory_index,
        granule_index: u64::try_from(granule_index)
            .context("tmemory granule index does not fit u64")?,
    })
}

fn memory_granule_key(
    owner_instance: Option<InstanceId>,
    memory_index: u32,
    granule_index: usize,
) -> Result<GranuleId> {
    memory_granule_id_for_instance(
        granule_instance(owner_instance),
        memory_index,
        granule_index,
    )
}

fn tmemory_granule_backing_range(
    granule_index: usize,
    backing_len: usize,
) -> Result<core::ops::Range<usize>> {
    let start = granule_index
        .checked_mul(TMEMORY_GRANULE_SIZE)
        .context("tmemory granule byte offset overflow")?;
    let end = start.saturating_add(TMEMORY_GRANULE_SIZE).min(backing_len);
    Ok(start..end)
}

pub(crate) fn execute_research_transaction_fixture(
    state: &mut TransactionState,
    operators: &[wasmtime_environ::ResearchTransactionModuleOperator],
) -> Result<()> {
    // SHISOFT-TWASM-MOCK: research fixture executor for metadata-only tests.
    // It handles only `ttry` and `tfail`; executable transaction operators
    // should run through parser/lowering/libcalls instead of this bridge.
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

    fn with_transaction_memory_metadata(wasm: &[u8]) -> Vec<u8> {
        with_transaction_object_metadata(wasm, &[1, 1, 0, 0])
    }

    fn with_transaction_global_metadata(wasm: &[u8]) -> Vec<u8> {
        with_transaction_object_metadata(wasm, &[1, 0, 1, 0])
    }

    fn with_transaction_object_metadata(wasm: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut wasm = wasm.to_vec();
        wasm.extend_from_slice(&[
            0x00, 0x20, 0x1b, b's', b'h', b'i', b's', b'o', b'f', b't', b'.', b't', b'r', b'a',
            b'n', b's', b'a', b'c', b't', b'i', b'o', b'n', b'.', b'o', b'b', b'j', b'e', b'c',
            b't', b's',
        ]);
        wasm.extend_from_slice(payload);
        wasm
    }

    fn transaction_test_module(engine: &crate::Engine, wat: &str) -> crate::Module {
        crate::Module::new(engine, wat::parse_str(wat).unwrap()).unwrap()
    }

    #[test]
    fn mock_transaction_store_commits_to_tmemory() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (tfunc (export "write")
                (i32.tstore (i32.const 0) (i32.const 42)))
              (tfunc (export "read") (result i32)
                (i32.tload (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        write.call(&mut store, ()).unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 42);
    }

    #[test]
    fn mock_transaction_simd_store_commits_to_tmemory() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (tfunc (export "write")
                (i32.const 0)
                (v128.const i32x4 287454020 1432778632 16909060 84281096)
                (v128.tstore))
              (tfunc (export "read_lane0") (result i32)
                (i32.const 0)
                (v128.tload)
                (i32x4.extract_lane 0)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read_lane0 = instance
            .get_typed_func::<(), i32>(&mut store, "read_lane0")
            .unwrap();

        write.call(&mut store, ()).unwrap();

        assert_eq!(read_lane0.call(&mut store, ()).unwrap(), 287454020);
    }

    #[test]
    fn mock_transaction_simd_lane_store_commits_to_tmemory() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (tfunc (export "write_lane")
                (i32.const 0)
                (v128.const i32x4 287454020 1432778632 16909060 84281096)
                (v128.tstore32_lane 1))
              (tfunc (export "read") (result i32)
                (i32.tload (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write_lane = instance
            .get_typed_func::<(), ()>(&mut store, "write_lane")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        write_lane.call(&mut store, ()).unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 1432778632);
    }

    #[test]
    fn mock_transaction_tfunc_store_commits_on_return() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (tfunc (export "write")
                (i32.tstore (i32.const 0) (i32.const 42)))
              (tfunc (export "read") (result i32)
                (i32.tload (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        write.call(&mut store, ()).unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 42);
    }

    #[test]
    fn mock_transaction_nested_tfunc_reuses_active_transaction_until_outer_return() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (tfunc $inner
                (i32.tstore (i32.const 0) (i32.const 1)))
              (tfunc (export "outer_fail")
                (call $inner)
                (i32.tstore (i32.const 0) (i32.const 2))
                (tfail))
              (tfunc (export "read") (result i32)
                (i32.tload (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let outer_fail = instance
            .get_typed_func::<(), ()>(&mut store, "outer_fail")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        outer_fail.call(&mut store, ()).unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 0);
    }

    #[test]
    fn mock_transaction_fail_discards_memory_write() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (func (export "write_fail")
                (ttry)
                (i32.tstore (i32.const 0) (i32.const 42))
                (tfail))
              (func (export "read") (result i32)
                (i32.load (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write_fail = instance
            .get_typed_func::<(), ()>(&mut store, "write_fail")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        write_fail.call(&mut store, ()).unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 0);
    }

    #[test]
    fn mock_transaction_global_i32_set_commits_to_backing_global() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tglobal $g (mut i32) (i32.const 0))
              (func (export "write")
                (ttry)
                (tglobal.set $g (i32.const 42)))
              (func (export "read") (result i32)
                (global.get $g)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        write.call(&mut store, ()).unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 42);
    }

    #[test]
    fn mock_transaction_global_i64_fail_discards_staged_write() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tglobal $g (mut i64) (i64.const 7))
              (func (export "write_fail")
                (ttry)
                (tglobal.set $g (i64.const 99))
                (tfail))
              (func (export "read") (result i64)
                (global.get $g)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write_fail = instance
            .get_typed_func::<(), ()>(&mut store, "write_fail")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i64>(&mut store, "read")
            .unwrap();

        write_fail.call(&mut store, ()).unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 7);
    }

    #[test]
    fn mock_transaction_global_get_observes_staged_i32_write() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tglobal $g (mut i32) (i32.const 1))
              (func (export "write_read") (result i32)
                (ttry)
                (tglobal.set $g (i32.const 77))
                (tglobal.get $g)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write_read = instance
            .get_typed_func::<(), i32>(&mut store, "write_read")
            .unwrap();

        assert_eq!(write_read.call(&mut store, ()).unwrap(), 77);
    }

    #[test]
    fn mock_transaction_global_imported_tglobal_is_unsupported_without_backing_write() {
        let engine = crate::Engine::default();
        let provider = transaction_test_module(
            &engine,
            r#"
            (module
              (tglobal $g (export "g") (mut i32) (i32.const 5))
              (func (export "read") (result i32)
                (global.get $g)))
            "#,
        );
        let consumer = transaction_test_module(
            &engine,
            r#"
            (module
              (import "env" "g" (tglobal $g (mut i32)))
              (func (export "write")
                (ttry)
                (tglobal.set $g (i32.const 11))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let provider = crate::Instance::new(&mut store, &provider, &[]).unwrap();
        let global = provider.get_global(&mut store, "g").unwrap();
        let consumer = crate::Instance::new(&mut store, &consumer, &[global.into()]).unwrap();
        let write = consumer
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read = provider
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        let error = write.call(&mut store, ()).unwrap_err();

        assert!(
            format!("{error:?}")
                .contains("transactional imported globals are not implemented in the mock runtime")
        );
        assert_eq!(read.call(&mut store, ()).unwrap(), 5);
    }

    #[test]
    fn mock_transaction_global_float_sets_commit_bitwise() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tglobal $f32 (mut f32) (f32.const 0))
              (tglobal $f64 (mut f64) (f64.const 0))
              (func (export "write")
                (ttry)
                (tglobal.set $f32 (f32.const -13.5))
                (tglobal.set $f64 (f64.const 42.25)))
              (func (export "read_f32_bits") (result i32)
                (i32.reinterpret_f32 (global.get $f32)))
              (func (export "read_f64_bits") (result i64)
                (i64.reinterpret_f64 (global.get $f64))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read_f32_bits = instance
            .get_typed_func::<(), i32>(&mut store, "read_f32_bits")
            .unwrap();
        let read_f64_bits = instance
            .get_typed_func::<(), i64>(&mut store, "read_f64_bits")
            .unwrap();

        write.call(&mut store, ()).unwrap();

        assert_eq!(
            read_f32_bits.call(&mut store, ()).unwrap() as u32,
            (-13.5f32).to_bits()
        );
        assert_eq!(
            read_f64_bits.call(&mut store, ()).unwrap() as u64,
            42.25f64.to_bits()
        );
    }

    #[test]
    fn mock_transaction_global_v128_get_reads_defined_global() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tglobal $g (mut v128) (v128.const i32x4 287454020 1432778632 16909060 84281096))
              (tfunc (export "read_lane1") (result i32)
                (tglobal.get $g)
                (i32x4.extract_lane 1)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let read_lane1 = instance
            .get_typed_func::<(), i32>(&mut store, "read_lane1")
            .unwrap();

        assert_eq!(read_lane1.call(&mut store, ()).unwrap(), 1432778632);
    }

    #[test]
    fn mock_transaction_global_v128_set_commits_bitwise() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tglobal $g (mut v128) (v128.const i32x4 0 0 0 0))
              (tfunc (export "write")
                (tglobal.set $g (v128.const i32x4 287454020 1432778632 16909060 84281096)))
              (tfunc (export "read_lane2") (result i32)
                (tglobal.get $g)
                (i32x4.extract_lane 2)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read_lane2 = instance
            .get_typed_func::<(), i32>(&mut store, "read_lane2")
            .unwrap();

        write.call(&mut store, ()).unwrap();

        assert_eq!(read_lane2.call(&mut store, ()).unwrap(), 16909060);
    }

    #[test]
    fn mock_transaction_plain_func_memory_size_requires_active_transaction() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 2)
              (func (export "size") (result i32)
                (tmemory.size)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let size = instance
            .get_typed_func::<(), i32>(&mut store, "size")
            .unwrap();

        let error = size.call(&mut store, ()).unwrap_err();

        assert!(
            format!("{error:?}").contains("transaction operation requires an active transaction")
        );
    }

    #[test]
    fn mock_transaction_tfunc_size_and_grow_commit_against_tmemory() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 0 1)
              (tfunc (export "size") (result i32)
                (tmemory.size))
              (tfunc (export "grow") (param i32) (result i32)
                (tmemory.grow (local.get 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let size = instance
            .get_typed_func::<(), i32>(&mut store, "size")
            .unwrap();
        let grow = instance
            .get_typed_func::<i32, i32>(&mut store, "grow")
            .unwrap();

        assert_eq!(size.call(&mut store, ()).unwrap(), 0);
        assert_eq!(grow.call(&mut store, 1).unwrap(), 0);
        assert_eq!(size.call(&mut store, ()).unwrap(), 1);
    }

    #[test]
    fn mock_transaction_ttable_funcref_paths_hit_runtime_libcalls() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (ttable $t 1 funcref)
              (elem declare func $target)
              (func $target)
              (tfunc (export "size") (result i32)
                (ttable.size $t))
              (tfunc (export "grow") (result i32)
                (ref.null func)
                (i32.const 2)
                (ttable.grow $t))
              (tfunc (export "is_null") (result i32)
                (i32.const 0)
                (ttable.get $t)
                (ref.is_null))
              (tfunc (export "set")
                (i32.const 0)
                (ref.func $target)
                (ttable.set $t)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let size = instance
            .get_typed_func::<(), i32>(&mut store, "size")
            .unwrap();
        let grow = instance
            .get_typed_func::<(), i32>(&mut store, "grow")
            .unwrap();
        let is_null = instance
            .get_typed_func::<(), i32>(&mut store, "is_null")
            .unwrap();
        let set = instance
            .get_typed_func::<(), ()>(&mut store, "set")
            .unwrap();

        assert_eq!(size.call(&mut store, ()).unwrap(), 1);
        assert_eq!(is_null.call(&mut store, ()).unwrap(), 1);
        set.call(&mut store, ()).unwrap();
        assert_eq!(is_null.call(&mut store, ()).unwrap(), 0);
        assert_eq!(grow.call(&mut store, ()).unwrap(), 1);
        assert_eq!(size.call(&mut store, ()).unwrap(), 3);
    }

    #[test]
    fn mock_transaction_ttable_bulk_funcref_paths_hit_runtime_libcalls() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (table $t 4 tfuncref)
              (elem $e func $target)
              (func $target)
              (tfunc (export "is_null") (param i32) (result i32)
                (local.get 0)
                (ttable.get $t)
                (ref.is_null))
              (tfunc (export "fill")
                (i32.const 0)
                (ref.func $target)
                (i32.const 1)
                (ttable.fill $t))
              (tfunc (export "copy")
                (i32.const 1)
                (i32.const 0)
                (i32.const 1)
                (ttable.copy $t $t))
              (tfunc (export "init")
                (i32.const 2)
                (i32.const 0)
                (i32.const 1)
                (ttable.init $t $e)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let is_null = instance
            .get_typed_func::<i32, i32>(&mut store, "is_null")
            .unwrap();
        let fill = instance
            .get_typed_func::<(), ()>(&mut store, "fill")
            .unwrap();
        let copy = instance
            .get_typed_func::<(), ()>(&mut store, "copy")
            .unwrap();
        let init = instance
            .get_typed_func::<(), ()>(&mut store, "init")
            .unwrap();

        assert_eq!(is_null.call(&mut store, 0).unwrap(), 1);
        assert_eq!(is_null.call(&mut store, 1).unwrap(), 1);
        assert_eq!(is_null.call(&mut store, 2).unwrap(), 1);
        fill.call(&mut store, ()).unwrap();
        assert_eq!(is_null.call(&mut store, 0).unwrap(), 0);
        copy.call(&mut store, ()).unwrap();
        assert_eq!(is_null.call(&mut store, 1).unwrap(), 0);
        init.call(&mut store, ()).unwrap();
        assert_eq!(is_null.call(&mut store, 2).unwrap(), 0);
    }

    #[test]
    fn module_compilation_rejects_transaction_table_get_on_ordinary_table() {
        let engine = crate::Engine::default();
        let error = crate::Module::new(
            &engine,
            wat::parse_str(
                r#"
                (module
                  (table $t 1 funcref)
                  (func (result i32)
                    (i32.const 0)
                    (ttable.get $t)
                    (ref.is_null)))
                "#,
            )
            .unwrap(),
        )
        .unwrap_err();
        let error = format!("{error:?}");
        assert!(
            error.contains("transactional table operator requires ttable"),
            "{error}"
        );
    }

    #[test]
    fn mock_transaction_static_tdata_initializes_tmemory_sidecar() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (tdata (i32.const 0) "abcdefgh")
              (tfunc (export "read") (result i64)
                (i64.tload (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let read = instance
            .get_typed_func::<(), i64>(&mut store, "read")
            .unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 0x6867_6665_6463_6261);
    }

    #[test]
    fn mock_transaction_read_after_write_uses_pending_store_scratch() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (func (export "write_read") (result i32)
                (ttry)
                (i32.tstore (i32.const 0) (i32.const 77))
                (i32.tload (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write_read = instance
            .get_typed_func::<(), i32>(&mut store, "write_read")
            .unwrap();

        assert_eq!(write_read.call(&mut store, ()).unwrap(), 77);
    }

    #[test]
    fn mock_transaction_tmemory_trap_clears_active_transaction() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (tfunc (export "trap")
                (i32.tstore (i32.const 0) (i32.const 42))
                (drop (i32.tload (i32.const 65536))))
              (tfunc (export "write")
                (i32.tstore (i32.const 0) (i32.const 7)))
              (tfunc (export "read") (result i32)
                (i32.tload (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let trap = instance
            .get_typed_func::<(), ()>(&mut store, "trap")
            .unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        assert!(trap.call(&mut store, ()).is_err());
        assert_eq!(read.call(&mut store, ()).unwrap(), 0);

        write.call(&mut store, ()).unwrap();
        assert_eq!(read.call(&mut store, ()).unwrap(), 7);
    }

    #[test]
    fn mock_transaction_plain_wasm_trap_clears_active_transaction() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (type $sig (func))
              (tmemory 1)
              (table 0 funcref)
              (tfunc (export "trap_after_tload")
                (drop (i32.tload (i32.const 0)))
                (call_indirect (type $sig) (i32.const 0)))
              (tfunc (export "recover") (result i32)
                (i32.tload (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let trap_after_tload = instance
            .get_typed_func::<(), ()>(&mut store, "trap_after_tload")
            .unwrap();
        let recover = instance
            .get_typed_func::<(), i32>(&mut store, "recover")
            .unwrap();

        trap_after_tload.call(&mut store, ()).unwrap_err();

        assert_eq!(recover.call(&mut store, ()).unwrap(), 0);
    }

    #[test]
    fn mock_transaction_unexecuted_ttry_does_not_commit_caller_transaction() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (func $maybe_begin (param i32)
                (local.get 0)
                (if
                  (then
                    (ttry))))
              (func (export "write_then_fail")
                (ttry)
                (i32.tstore (i32.const 0) (i32.const 42))
                (call $maybe_begin (i32.const 0))
                (tfail))
              (func (export "read") (result i32)
                (i32.load (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write_then_fail = instance
            .get_typed_func::<(), ()>(&mut store, "write_then_fail")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        write_then_fail.call(&mut store, ()).unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 0);
    }

    #[test]
    fn mock_transaction_store_uses_defined_memory_index_after_import() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (import "env" "ordinary" (memory 1))
              (tmemory $tx 1)
              (tfunc (export "write")
                (i32.tstore $tx (i32.const 0) (i32.const 55)))
              (tfunc (export "read_tx") (result i32)
                (i32.tload $tx (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let ordinary = crate::Memory::new(&mut store, crate::MemoryType::new(1, None)).unwrap();
        let instance = crate::Instance::new(&mut store, &module, &[ordinary.into()]).unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read_tx = instance
            .get_typed_func::<(), i32>(&mut store, "read_tx")
            .unwrap();

        write.call(&mut store, ()).unwrap();

        assert_eq!(read_tx.call(&mut store, ()).unwrap(), 55);
    }

    #[test]
    fn mock_transaction_store_uses_imported_tmemory_vmctx() {
        let engine = crate::Engine::default();
        let provider = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory $tx 1)
              (export "tx" (memory $tx)))
            "#,
        );
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (import "env" "tx" (tmemory $tx 1))
              (tfunc (export "write")
                (i32.tstore $tx (i32.const 0) (i32.const 66)))
              (tfunc (export "read_tx") (result i32)
                (i32.tload $tx (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let provider = crate::Instance::new(&mut store, &provider, &[]).unwrap();
        let tx = provider.get_memory(&mut store, "tx").unwrap();
        let instance = crate::Instance::new(&mut store, &module, &[tx.into()]).unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read_tx = instance
            .get_typed_func::<(), i32>(&mut store, "read_tx")
            .unwrap();

        write.call(&mut store, ()).unwrap();

        assert_eq!(read_tx.call(&mut store, ()).unwrap(), 66);
    }

    #[test]
    fn mock_transaction_distinguishes_imported_and_local_tmemory_overlays() {
        let engine = crate::Engine::default();
        let provider = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory $tx 1)
              (export "tx" (memory $tx)))
            "#,
        );
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (import "env" "tx" (tmemory $imported 1))
              (tmemory $local 1)
              (tfunc (export "write_both")
                (i32.tstore $imported (i32.const 0) (i32.const 11))
                (i32.tstore $local (i32.const 0) (i32.const 22)))
              (tfunc (export "read_imported") (result i32)
                (i32.tload $imported (i32.const 0)))
              (tfunc (export "read_local") (result i32)
                (i32.tload $local (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let provider = crate::Instance::new(&mut store, &provider, &[]).unwrap();
        let tx = provider.get_memory(&mut store, "tx").unwrap();
        let instance = crate::Instance::new(&mut store, &module, &[tx.into()]).unwrap();
        let write_both = instance
            .get_typed_func::<(), ()>(&mut store, "write_both")
            .unwrap();
        let read_imported = instance
            .get_typed_func::<(), i32>(&mut store, "read_imported")
            .unwrap();
        let read_local = instance
            .get_typed_func::<(), i32>(&mut store, "read_local")
            .unwrap();

        write_both.call(&mut store, ()).unwrap();

        assert_eq!(read_imported.call(&mut store, ()).unwrap(), 11);
        assert_eq!(read_local.call(&mut store, ()).unwrap(), 22);
    }

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
                    owner_instance: None,
                    global_index: 3,
                    value: GlobalSnapshot::I64(22)
                },
                StagedRecord::MemoryGranule {
                    owner_instance: None,
                    memory_index: 0,
                    granule_index: 7,
                    bytes: vec![0x22; 256]
                },
                StagedRecord::MemorySize {
                    owner_instance: None,
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
    fn stage_memory_write_splits_cross_granule_write() {
        let mut backing = vec![0x11; TMEMORY_GRANULE_SIZE * 2];
        backing[TMEMORY_GRANULE_SIZE..].fill(0x22);
        let mut state = TransactionState::default();
        state.begin().unwrap();

        let addr = u64::try_from(TMEMORY_GRANULE_SIZE - 2).unwrap();
        state
            .stage_memory_write(0, addr, &[0xaa, 0xbb, 0xcc, 0xdd], &backing)
            .unwrap();

        let first = state.staged_memory_granule(0, 0).unwrap();
        let second = state.staged_memory_granule(0, 1).unwrap();
        assert_eq!(
            &first[..TMEMORY_GRANULE_SIZE - 2],
            &backing[..TMEMORY_GRANULE_SIZE - 2]
        );
        assert_eq!(&first[TMEMORY_GRANULE_SIZE - 2..], &[0xaa, 0xbb]);
        assert_eq!(&second[..2], &[0xcc, 0xdd]);
        assert_eq!(&second[2..], &backing[TMEMORY_GRANULE_SIZE + 2..]);
        assert!(state.owns_memory_granule_read(0, 0));
        assert!(state.owns_memory_granule_write(0, 0));
        assert!(state.owns_memory_granule_read(0, 1));
        assert!(state.owns_memory_granule_write(0, 1));
    }

    #[test]
    fn read_memory_overlay_preserves_unwritten_staged_bytes() {
        let backing = vec![0x10; TMEMORY_GRANULE_SIZE];
        let mut state = TransactionState::default();
        state.begin().unwrap();

        state
            .stage_memory_write(0, 10, &[0xaa, 0xbb], &backing)
            .unwrap();
        state.stage_memory_write(0, 12, &[0xcc], &backing).unwrap();

        let read = state.read_memory_overlay(0, 8, 6, &backing).unwrap();
        let mut expected_read = backing[8..14].to_vec();
        expected_read[2..5].copy_from_slice(&[0xaa, 0xbb, 0xcc]);
        assert_eq!(read, expected_read);

        let mut expected_granule = backing.clone();
        expected_granule[10..13].copy_from_slice(&[0xaa, 0xbb, 0xcc]);
        assert_eq!(
            state.staged_memory_granule(0, 0).unwrap(),
            expected_granule.as_slice()
        );
    }

    #[test]
    fn memory_overlay_helpers_reject_out_of_bounds_access() {
        let backing = vec![0; 8];
        let mut state = TransactionState::default();
        state.begin().unwrap();

        let write_error = state
            .stage_memory_write(0, 7, &[0xaa, 0xbb], &backing)
            .unwrap_err();
        assert!(
            write_error
                .to_string()
                .contains("out of bounds tmemory access")
        );

        let read_error = state.read_memory_overlay(0, 7, 2, &backing).unwrap_err();
        assert!(
            read_error
                .to_string()
                .contains("out of bounds tmemory access")
        );
    }

    #[test]
    fn read_memory_overlay_tracks_read_ownership_for_touched_granules() {
        let backing = vec![0; TMEMORY_GRANULE_SIZE * 2];
        let mut state = TransactionState::default();
        state.begin().unwrap();

        let addr = u64::try_from(TMEMORY_GRANULE_SIZE - 1).unwrap();
        assert_eq!(
            state.read_memory_overlay(0, addr, 2, &backing).unwrap(),
            vec![0, 0]
        );

        assert!(state.owns_memory_granule_read(0, 0));
        assert!(state.owns_memory_granule_read(0, 1));
        assert!(!state.owns_memory_granule_write(0, 0));
        assert!(!state.owns_memory_granule_write(0, 1));
    }

    #[test]
    fn pending_memory_store_scratch_acquires_write_ownership_for_touched_granules() {
        let mut state = TransactionState::default();
        state.begin().unwrap();
        let owner = InstanceId::from_u32(1);

        let addr = u64::try_from(TMEMORY_GRANULE_SIZE - 1).unwrap();
        state
            .set_memory_store_scratch(owner, 0, addr, vec![0xaa, 0xbb])
            .unwrap();

        assert!(state.owns_memory_granule_write_owned(Some(owner), 0, 0));
        assert!(state.owns_memory_granule_write_owned(Some(owner), 0, 1));
    }

    #[test]
    fn commit_releases_lock_based_ownership_for_next_transaction() {
        let mut state = TransactionState::default();
        state.begin().unwrap();
        state
            .acquire_memory_granule_write(0, 0, vec![0xaa; TMEMORY_GRANULE_SIZE])
            .unwrap();

        state.commit().unwrap();

        state.begin().unwrap();
        state.acquire_memory_granule_read(0, 0).unwrap();
    }

    #[test]
    fn commit_rejects_changed_optimistic_read_version() {
        let mut state = TransactionState::default();
        let transaction = state.begin().unwrap();
        let granule = GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 0,
        };
        state
            .locks
            .record_read_for_test(transaction, granule, 1)
            .unwrap();

        let error = state.commit().unwrap_err();

        assert!(error.to_string().contains("transaction read conflict"));
        assert_eq!(state.active_transaction(), Some(transaction));
    }

    #[test]
    fn commit_validates_optimistic_read_versions_before_clearing() {
        let mut state = TransactionState::default();
        let transaction = state.begin().unwrap();
        let granule = GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 0,
        };
        state
            .locks
            .record_read_for_test(transaction, granule, 1)
            .unwrap();

        let error = state
            .commit_with_read_validation(|_| Ok(2), |_| Ok(()))
            .unwrap_err();

        assert!(error.to_string().contains("transaction read conflict"));
        assert_eq!(state.active_transaction(), Some(transaction));
    }

    #[test]
    fn commit_validates_reads_before_apply_callback() {
        let mut state = TransactionState::default();
        let transaction = state.begin().unwrap();
        let granule = GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 0,
        };
        state.stage_global(3, GlobalSnapshot::I32(7)).unwrap();
        state
            .locks
            .record_read_for_test(transaction, granule, 1)
            .unwrap();
        let mut applied = false;

        let error = state
            .commit_with_read_validation(
                |_| Ok(2),
                |_| {
                    applied = true;
                    Ok(())
                },
            )
            .unwrap_err();

        assert!(error.to_string().contains("transaction read conflict"));
        assert!(!applied);
        assert_eq!(state.active_transaction(), Some(transaction));
    }

    #[test]
    fn abort_releases_lock_based_ownership_for_next_transaction() {
        let mut state = TransactionState::default();
        state.begin().unwrap();
        state
            .acquire_memory_granule_write(0, 0, vec![0xaa; TMEMORY_GRANULE_SIZE])
            .unwrap();

        state.abort().unwrap();

        state.begin().unwrap();
        state.acquire_memory_granule_read(0, 0).unwrap();
    }

    #[test]
    fn fail_releases_lock_based_ownership_for_next_transaction() {
        let mut state = TransactionState::default();
        state.begin().unwrap();
        state
            .acquire_memory_granule_write(0, 0, vec![0xaa; TMEMORY_GRANULE_SIZE])
            .unwrap();

        state.fail().unwrap();

        state.begin().unwrap();
        state.acquire_memory_granule_read(0, 0).unwrap();
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
                owner_instance: None,
                global_index: 3,
                value: GlobalSnapshot::I64(22)
            }]
        );
    }

    #[test]
    fn owner_instance_zero_does_not_alias_ownerless_globals() {
        let mut state = TransactionState::default();
        let owner0 = InstanceId::from_u32(0);
        state.begin().unwrap();

        assert!(
            state
                .stage_global_owned(None, 0, GlobalSnapshot::I32(1))
                .unwrap()
        );
        assert!(
            state
                .stage_global_owned(Some(owner0), 0, GlobalSnapshot::I32(2))
                .unwrap()
        );

        assert_eq!(
            state.staged_global_owned(None, 0),
            Some(GlobalSnapshot::I32(1))
        );
        assert_eq!(
            state.staged_global_owned(Some(owner0), 0),
            Some(GlobalSnapshot::I32(2))
        );

        let records = state.staged_records().unwrap();
        assert!(records.contains(&StagedRecord::Global {
            owner_instance: None,
            global_index: 0,
            value: GlobalSnapshot::I32(1),
        }));
        assert!(records.contains(&StagedRecord::Global {
            owner_instance: Some(owner0),
            global_index: 0,
            value: GlobalSnapshot::I32(2),
        }));
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(
                    record,
                    StagedRecord::Global {
                        global_index: 0,
                        ..
                    }
                ))
                .count(),
            2
        );
    }

    #[test]
    fn owner_instance_zero_does_not_alias_ownerless_memory_granules() {
        let mut state = TransactionState::default();
        let owner0 = InstanceId::from_u32(0);
        let ownerless_bytes = vec![0x11; TMEMORY_GRANULE_SIZE];
        let owner0_bytes = vec![0x22; TMEMORY_GRANULE_SIZE];
        state.begin().unwrap();

        assert!(
            state
                .stage_memory_granule(0, 0, ownerless_bytes.clone())
                .unwrap()
        );
        assert!(
            state
                .acquire_memory_granule_write_owned(Some(owner0), 0, 0, owner0_bytes.clone())
                .unwrap()
        );

        let records = state.staged_records().unwrap();
        assert!(records.contains(&StagedRecord::MemoryGranule {
            owner_instance: None,
            memory_index: 0,
            granule_index: 0,
            bytes: ownerless_bytes,
        }));
        assert!(records.contains(&StagedRecord::MemoryGranule {
            owner_instance: Some(owner0),
            memory_index: 0,
            granule_index: 0,
            bytes: owner0_bytes,
        }));
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(
                    record,
                    StagedRecord::MemoryGranule {
                        memory_index: 0,
                        granule_index: 0,
                        ..
                    }
                ))
                .count(),
            2
        );
    }

    #[test]
    fn owner_instance_zero_does_not_alias_ownerless_memory_sizes() {
        let mut state = TransactionState::default();
        let owner0 = InstanceId::from_u32(0);
        state.begin().unwrap();

        assert!(state.stage_memory_size_owned(None, 0, 1).unwrap());
        assert!(state.stage_memory_size_owned(Some(owner0), 0, 2).unwrap());

        let records = state.staged_records().unwrap();
        assert!(records.contains(&StagedRecord::MemorySize {
            owner_instance: None,
            memory_index: 0,
            new_pages: 1,
        }));
        assert!(records.contains(&StagedRecord::MemorySize {
            owner_instance: Some(owner0),
            memory_index: 0,
            new_pages: 2,
        }));
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(
                    record,
                    StagedRecord::MemorySize {
                        memory_index: 0,
                        ..
                    }
                ))
                .count(),
            2
        );
    }

    #[test]
    fn workspace_merges_staged_and_committed_granules_with_nonzero_backing_base() {
        let mut state = TransactionState::default();
        let owner0 = InstanceId::from_u32(0);
        let memory_index = 0;
        let backing_base = TMEMORY_GRANULE_SIZE as u64;
        let memory_len = TMEMORY_GRANULE_SIZE * 4;
        let committed: Vec<u8> = (0..TMEMORY_GRANULE_SIZE * 2)
            .map(|i| (i % 251) as u8)
            .collect();
        let addr = (TMEMORY_GRANULE_SIZE * 2 - 2) as u64;
        let staged = vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let read_addr = addr - 2;
        let read_len = 10;
        state.begin().unwrap();

        state
            .stage_memory_write_owned_from_backing(
                Some(owner0),
                memory_index,
                addr,
                &staged,
                backing_base,
                &committed,
                memory_len,
            )
            .unwrap();

        let merged = state
            .read_memory_overlay_owned_from_backing(
                Some(owner0),
                memory_index,
                read_addr,
                read_len,
                backing_base,
                &committed,
                memory_len,
            )
            .unwrap();

        let read_start = read_addr as usize - backing_base as usize;
        let mut expected = committed[read_start..read_start + read_len].to_vec();
        let staged_offset = (addr - read_addr) as usize;
        expected[staged_offset..staged_offset + staged.len()].copy_from_slice(&staged);

        assert_eq!(merged, expected);
    }

    #[test]
    fn granule_id_orders_by_object_space_and_index() {
        let mut ids = vec![
            GranuleId::TGlobal {
                instance: None,
                global_index: 2,
            },
            GranuleId::TMemorySize {
                instance: None,
                memory_index: 4,
            },
            GranuleId::TMemory {
                instance: Some(2),
                memory_index: 0,
                granule_index: 0,
            },
            GranuleId::TMemory {
                instance: Some(1),
                memory_index: 7,
                granule_index: 3,
            },
            GranuleId::TGlobal {
                instance: None,
                global_index: 1,
            },
            GranuleId::TMemorySize {
                instance: None,
                memory_index: 1,
            },
            GranuleId::TMemory {
                instance: Some(1),
                memory_index: 7,
                granule_index: 1,
            },
            GranuleId::TArray {
                object_id: ObjectId { object_index: 2 },
            },
            GranuleId::TStruct {
                object_id: ObjectId { object_index: 1 },
            },
            GranuleId::TTableSize {
                instance: None,
                table_index: 3,
            },
            GranuleId::TTable {
                instance: Some(1),
                table_index: 2,
                granule_index: 0,
            },
        ];

        ids.sort();

        assert_eq!(
            ids,
            vec![
                GranuleId::TMemory {
                    instance: Some(1),
                    memory_index: 7,
                    granule_index: 1,
                },
                GranuleId::TMemory {
                    instance: Some(1),
                    memory_index: 7,
                    granule_index: 3,
                },
                GranuleId::TMemory {
                    instance: Some(2),
                    memory_index: 0,
                    granule_index: 0,
                },
                GranuleId::TMemorySize {
                    instance: None,
                    memory_index: 1,
                },
                GranuleId::TMemorySize {
                    instance: None,
                    memory_index: 4,
                },
                GranuleId::TGlobal {
                    instance: None,
                    global_index: 1,
                },
                GranuleId::TGlobal {
                    instance: None,
                    global_index: 2,
                },
                GranuleId::TTable {
                    instance: Some(1),
                    table_index: 2,
                    granule_index: 0,
                },
                GranuleId::TTableSize {
                    instance: None,
                    table_index: 3,
                },
                GranuleId::TStruct {
                    object_id: ObjectId { object_index: 1 },
                },
                GranuleId::TArray {
                    object_id: ObjectId { object_index: 2 },
                },
            ]
        );
    }

    #[test]
    fn workspace_merges_staged_and_committed_granules() {
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(7));
        let instance = 11;
        let memory_index = 3;
        let committed: Vec<u8> = (0..TMEMORY_GRANULE_SIZE * 3)
            .map(|i| (i % 251) as u8)
            .collect();

        state.stage_granule_for_test(
            GranuleId::TMemory {
                instance: Some(instance),
                memory_index,
                granule_index: 0,
            },
            vec![0xAA; TMEMORY_GRANULE_SIZE],
        );
        state.stage_granule_for_test(
            GranuleId::TMemory {
                instance: Some(instance),
                memory_index,
                granule_index: 2,
            },
            vec![0xCC; TMEMORY_GRANULE_SIZE],
        );

        let merged = state
            .read_tmemory_range_for_test(
                Some(instance),
                memory_index,
                (TMEMORY_GRANULE_SIZE - 2) as u64,
                TMEMORY_GRANULE_SIZE + 6,
                &committed,
            )
            .unwrap();

        let mut expected = vec![0xAA; 2];
        expected.extend_from_slice(&committed[TMEMORY_GRANULE_SIZE..TMEMORY_GRANULE_SIZE * 2]);
        expected.extend_from_slice(&[0xCC; 4]);

        assert_eq!(merged, expected);
    }

    #[test]
    fn transaction_commit_copies_staged_granules_to_tmemory() {
        let mut tmemory = crate::runtime::vm::TMemory::new_vmemory(1).expect("tmemory");
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(7));

        state
            .stage_tmemory_write_for_test(0, 0, 4, &[9, 8, 7, 6], &tmemory)
            .expect("stage tmemory");
        state
            .commit_tmemory_for_test(&mut tmemory)
            .expect("commit tmemory");

        assert_eq!(
            tmemory.read_committed(4..8).expect("read committed"),
            vec![9, 8, 7, 6]
        );
    }

    #[test]
    fn transaction_abort_discards_staged_tmemory_bytes() {
        let tmemory = crate::runtime::vm::TMemory::new_vmemory(1).expect("tmemory");
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(7));

        state
            .stage_tmemory_write_for_test(0, 0, 4, &[9, 8, 7, 6], &tmemory)
            .expect("stage tmemory");
        state.abort().expect("abort transaction");

        assert_eq!(
            tmemory.read_committed(4..8).expect("read committed"),
            vec![0, 0, 0, 0]
        );
    }

    #[test]
    fn transaction_table_granules_follow_wizard_sixteen_element_chunks() {
        let owner = InstanceId::from_u32(2);

        assert_eq!(
            table_granule_id(Some(owner), 3, 0),
            GranuleId::TTable {
                instance: Some(2),
                table_index: 3,
                granule_index: 0,
            }
        );
        assert_eq!(
            table_granule_id(Some(owner), 3, 15),
            GranuleId::TTable {
                instance: Some(2),
                table_index: 3,
                granule_index: 0,
            }
        );
        assert_eq!(
            table_granule_id(Some(owner), 3, 16),
            GranuleId::TTable {
                instance: Some(2),
                table_index: 3,
                granule_index: 1,
            }
        );
    }

    #[test]
    fn transaction_table_granule_acquisition_tracks_read_and_write_sets() {
        let mut state = TransactionState::default();
        let owner = InstanceId::from_u32(1);
        state.begin().unwrap();

        assert!(
            state
                .acquire_table_granule_read_owned(Some(owner), 2, 15, 4)
                .unwrap()
        );
        assert!(
            !state
                .acquire_table_granule_read_owned(Some(owner), 2, 8, 4)
                .unwrap()
        );
        assert!(state.owns_table_granule_read_owned(Some(owner), 2, 0));
        assert!(!state.owns_table_granule_write_owned(Some(owner), 2, 0));

        assert!(
            state
                .acquire_table_granule_write_owned(Some(owner), 2, 16, 7)
                .unwrap()
        );
        assert!(state.owns_table_granule_read_owned(Some(owner), 2, 1));
        assert!(state.owns_table_granule_write_owned(Some(owner), 2, 1));
    }

    #[test]
    fn transaction_table_size_granule_is_separate_from_element_granules() {
        let mut state = TransactionState::default();
        let owner = InstanceId::from_u32(1);
        state.begin().unwrap();

        assert!(
            state
                .acquire_table_size_read_owned(Some(owner), 2, 11)
                .unwrap()
        );
        assert!(state.owns_table_size_read_owned(Some(owner), 2));
        assert!(!state.owns_table_granule_read_owned(Some(owner), 2, 0));

        assert!(
            state
                .acquire_table_size_write_owned(Some(owner), 2, 11)
                .unwrap()
        );
        assert!(state.owns_table_size_write_owned(Some(owner), 2));
        assert!(!state.owns_table_granule_write_owned(Some(owner), 2, 0));
    }

    #[test]
    fn lock_based_allows_shared_reads_and_writer_upgrade() {
        let mut locks = LockBased::default();
        let first = TransactionId::from_raw(1);
        let second = TransactionId::from_raw(2);
        let granule = GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 7,
        };

        locks.record_read_for_test(first, granule, 5).unwrap();
        locks.record_read_for_test(second, granule, 5).unwrap();
        locks.acquire_write_for_test(second, granule, 5).unwrap();
    }

    #[test]
    fn lock_based_writer_excludes_other_transactions() {
        let mut locks = LockBased::default();
        let first = TransactionId::from_raw(1);
        let second = TransactionId::from_raw(2);
        let granule = GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 7,
        };

        locks.acquire_write_for_test(first, granule, 3).unwrap();

        let read_error = locks.record_read_for_test(second, granule, 3).unwrap_err();
        assert!(read_error.to_string().contains("transaction read conflict"));

        let write_error = locks
            .acquire_write_for_test(second, granule, 3)
            .unwrap_err();
        assert!(
            write_error
                .to_string()
                .contains("transaction write conflict")
        );
    }

    #[test]
    fn lock_based_conflicts_are_released_on_abort() {
        let mut locks = LockBased::default();
        let first = TransactionId(1);
        let second = TransactionId(2);

        locks.acquire_memory_granule_write(first, 0, 0).unwrap();
        assert!(locks.acquire_memory_granule_read(second, 0, 0).is_err());

        locks.release_transaction(first);

        locks.acquire_memory_granule_read(second, 0, 0).unwrap();
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
    fn lock_based_write_conflict_aborts_current_transaction() {
        let mut locks = LockBased::default();
        let first = TransactionId::from_raw(1);
        let second = TransactionId::from_raw(2);
        let conflicted = GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 0,
        };
        let independent = GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 1,
        };

        locks.acquire_write_for_test(first, conflicted, 9).unwrap();
        locks
            .acquire_write_for_test(second, independent, 3)
            .unwrap();

        let error = locks
            .acquire_write_for_test(second, conflicted, 9)
            .unwrap_err();
        assert!(error.to_string().contains("transaction write conflict"));
        assert_eq!(locks.owner_for_test(independent), Some(second));

        locks.abort_for_test(second);

        assert_eq!(locks.owner_for_test(conflicted), Some(first));
        assert_eq!(locks.owner_for_test(independent), None);
    }

    #[test]
    fn lock_based_validates_optimistic_reads_at_commit() {
        let mut locks = LockBased::default();
        let reader = TransactionId::from_raw(1);
        let writer = TransactionId::from_raw(2);
        let granule = GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 0,
        };

        locks.record_read_for_test(reader, granule, 7).unwrap();
        locks.acquire_write_for_test(writer, granule, 7).unwrap();
        locks.abort_for_test(writer);

        let error = locks
            .validate_read_for_test(reader, granule, 8)
            .unwrap_err();
        assert!(error.to_string().contains("transaction read conflict"));
    }

    #[test]
    fn lock_based_abort_releases_owned_granules() {
        let mut locks = LockBased::default();
        let transaction = TransactionId::from_raw(1);
        let granule = GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 0,
        };

        locks
            .acquire_write_for_test(transaction, granule, 4)
            .unwrap();
        assert_eq!(locks.owner_for_test(granule), Some(transaction));

        locks.abort_for_test(transaction);

        assert_eq!(locks.owner_for_test(granule), None);
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
        let wasm = with_transaction_memory_metadata(&[
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x05, 0x03, 0x01, 0x00, 0x01, // memory section
            0x0a, 0x0a, 0x01, 0x08, 0x00, // code section/function body
            0x41, 0x00, // i32.const 0
            0xfa, 0x28, 0x02, 0x00, // i32.tload align=2 offset=0
            0x0b, // end
        ]);

        crate::Module::new(&engine, wasm).unwrap();
    }

    #[test]
    fn module_compilation_rejects_transaction_load_on_ordinary_memory() {
        let engine = crate::Engine::default();
        let wasm = [
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x05, 0x03, 0x01, 0x00, 0x01, // ordinary memory section
            0x0a, 0x0a, 0x01, 0x08, 0x00, // code section/function body
            0x41, 0x00, // i32.const 0
            0xfa, 0x28, 0x02, 0x00, // i32.tload align=2 offset=0
            0x0b, // end
        ];

        let error = crate::Module::new(&engine, wasm).unwrap_err();
        let error = format!("{error:?}");
        assert!(
            error.contains("transactional memory operator requires tmemory"),
            "{error}"
        );
    }

    #[test]
    fn module_compilation_accepts_transaction_store_helper_lowering() {
        let engine = crate::Engine::default();
        let wasm = with_transaction_memory_metadata(&[
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
        ]);

        crate::Module::new(&engine, wasm).unwrap();
    }

    #[test]
    fn module_compilation_accepts_transaction_global_get_helper_lowering() {
        let engine = crate::Engine::default();
        let wasm = with_transaction_global_metadata(&[
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x06, 0x06, 0x01, 0x7f, 0x00, 0x41, 0x00, 0x0b, // global section
            0x0a, 0x07, 0x01, 0x05, 0x00, // code section/function body
            0xfa, 0x23, 0x00, // tglobal.get 0
            0x0b, // end
        ]);

        crate::Module::new(&engine, wasm).unwrap();
    }

    #[test]
    fn module_compilation_rejects_transaction_global_get_on_ordinary_global() {
        let engine = crate::Engine::default();
        let wasm = [
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x06, 0x06, 0x01, 0x7f, 0x00, 0x41, 0x00, 0x0b, // ordinary global section
            0x0a, 0x07, 0x01, 0x05, 0x00, // code section/function body
            0xfa, 0x23, 0x00, // tglobal.get 0
            0x0b, // end
        ];

        let error = crate::Module::new(&engine, wasm).unwrap_err();
        let error = format!("{error:?}");
        assert!(
            error.contains("transactional global operator requires tglobal"),
            "{error}"
        );
    }

    #[test]
    fn module_compilation_accepts_transaction_global_set_helper_lowering() {
        let engine = crate::Engine::default();
        let wasm = with_transaction_global_metadata(&[
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x06, 0x06, 0x01, 0x7f, 0x01, 0x41, 0x00, 0x0b, // global section
            0x0a, 0x09, 0x01, 0x07, 0x00, // code section/function body
            0x41, 0x2a, // i32.const 42
            0xfa, 0x24, 0x00, // tglobal.set 0
            0x0b, // end
        ]);

        crate::Module::new(&engine, wasm).unwrap();
    }

    #[test]
    fn module_compilation_accepts_transaction_memory_size_helper_lowering() {
        let engine = crate::Engine::default();
        let wasm = with_transaction_memory_metadata(&[
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x05, 0x03, 0x01, 0x00, 0x01, // memory section
            0x0a, 0x07, 0x01, 0x05, 0x00, // code section/function body
            0xfa, 0x3f, 0x00, // tmemory.size 0
            0x0b, // end
        ]);

        crate::Module::new(&engine, wasm).unwrap();
    }

    #[test]
    fn module_compilation_accepts_transaction_memory_grow_helper_lowering() {
        let engine = crate::Engine::default();
        let wasm = with_transaction_memory_metadata(&[
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x05, 0x03, 0x01, 0x00, 0x01, // memory section
            0x0a, 0x09, 0x01, 0x07, 0x00, // code section/function body
            0x41, 0x01, // i32.const 1
            0xfa, 0x40, 0x00, // tmemory.grow 0
            0x0b, // end
        ]);

        crate::Module::new(&engine, wasm).unwrap();
    }
}
