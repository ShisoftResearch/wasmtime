#![allow(dead_code)]

use crate::prelude::*;
use crate::runtime::store::InstanceId;
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
    staged_globals: BTreeMap<GlobalObjectKey, GlobalSnapshot>,
    staged_memory_granules: BTreeMap<MemoryGranuleKey, Vec<u8>>,
    staged_memory_sizes: BTreeMap<MemoryObjectKey, u64>,
    memory_read_granules: BTreeSet<MemoryGranuleKey>,
    memory_write_granules: BTreeSet<MemoryGranuleKey>,
    scratch: Vec<u8>,
    pending_memory_store: Option<PendingMemoryStore>,
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
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct GlobalObjectKey {
    owner_instance: Option<u32>,
    global_index: u32,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct MemoryGranuleKey {
    object: MemoryObjectKey,
    granule_index: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct MemoryObjectKey {
    owner_instance: Option<u32>,
    memory_index: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingMemoryStore {
    instance: InstanceId,
    memory_index: u32,
    addr: u64,
    len: usize,
}

const TMEMORY_GRANULE_SHIFT: usize = 8;
pub(crate) const TMEMORY_GRANULE_SIZE: usize = 1 << TMEMORY_GRANULE_SHIFT;

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
            object: memory_object_key(None, memory_index),
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
            object: memory_object_key(None, memory_index),
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
        for record in self.staged_records()? {
            apply(&record)?;
        }
        self.clear_active();
        Ok(())
    }

    pub(crate) fn staged_records(&self) -> Result<Vec<StagedRecord>> {
        self.ensure_active()?;
        let mut records = Vec::new();
        for (key, &value) in &self.staged_globals {
            records.push(StagedRecord::Global {
                owner_instance: key.owner_instance.map(InstanceId::from_u32),
                global_index: key.global_index,
                value,
            });
        }
        for (key, bytes) in &self.staged_memory_granules {
            records.push(StagedRecord::MemoryGranule {
                owner_instance: key.object.owner_instance.map(InstanceId::from_u32),
                memory_index: key.object.memory_index,
                granule_index: key.granule_index,
                bytes: bytes.clone(),
            });
        }
        for (key, &new_pages) in &self.staged_memory_sizes {
            records.push(StagedRecord::MemorySize {
                owner_instance: key.owner_instance.map(InstanceId::from_u32),
                memory_index: key.memory_index,
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
            .insert(memory_object_key(owner_instance, memory_index), new_pages)
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
            .insert(global_object_key(owner_instance, global_index), value)
            .is_none())
    }

    pub(crate) fn staged_global_owned(
        &self,
        owner_instance: Option<InstanceId>,
        global_index: u32,
    ) -> Option<GlobalSnapshot> {
        self.staged_globals
            .get(&global_object_key(owner_instance, global_index))
            .copied()
    }

    pub(crate) fn stage_memory_granule(
        &mut self,
        memory_index: u32,
        granule_index: u64,
        bytes: Vec<u8>,
    ) -> Result<bool> {
        self.ensure_active()?;
        let key = MemoryGranuleKey {
            object: memory_object_key(None, memory_index),
            granule_index,
        };
        self.memory_read_granules.insert(key);
        self.memory_write_granules.insert(key);
        Ok(self.staged_memory_granules.insert(key, bytes).is_none())
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
            let granule_offset = current - granule_range.start;
            let chunk_len = (range.end - current).min(granule_range.end - current);
            let granule_chunk_end = granule_offset
                .checked_add(chunk_len)
                .context("tmemory granule write offset overflow")?;
            let write_chunk_end = offset
                .checked_add(chunk_len)
                .context("tmemory write offset overflow")?;

            {
                let staged = self
                    .staged_memory_granules
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
            let key = memory_granule_key(owner_instance, memory_index, granule_index)?;
            let granule_offset = current - granule_range.start;
            let chunk_len = (range.end - current).min(granule_range.end - current);
            let granule_chunk_end = granule_offset
                .checked_add(chunk_len)
                .context("tmemory granule read offset overflow")?;

            if let Some(staged) = self.staged_memory_granules.get(&key) {
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
        self.scratch = bytes;
        self.pending_memory_store = Some(PendingMemoryStore {
            instance,
            memory_index,
            addr,
            len,
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

    pub(crate) fn staged_memory_granule(
        &self,
        memory_index: u32,
        granule_index: u64,
    ) -> Option<&[u8]> {
        self.staged_memory_granules
            .get(&MemoryGranuleKey {
                object: memory_object_key(None, memory_index),
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
                object: memory_object_key(None, memory_index),
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
            object: memory_object_key(None, memory_index),
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
            object: memory_object_key(None, memory_index),
            granule_index,
        })
    }

    pub(crate) fn owns_memory_granule_write(&self, memory_index: u32, granule_index: u64) -> bool {
        self.memory_write_granules.contains(&MemoryGranuleKey {
            object: memory_object_key(None, memory_index),
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
        self.scratch.clear();
        self.pending_memory_store = None;
    }
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

fn memory_object_key(owner_instance: Option<InstanceId>, memory_index: u32) -> MemoryObjectKey {
    MemoryObjectKey {
        owner_instance: owner_instance.map(InstanceId::as_u32),
        memory_index,
    }
}

fn global_object_key(owner_instance: Option<InstanceId>, global_index: u32) -> GlobalObjectKey {
    GlobalObjectKey {
        owner_instance: owner_instance.map(InstanceId::as_u32),
        global_index,
    }
}

fn memory_granule_key(
    owner_instance: Option<InstanceId>,
    memory_index: u32,
    granule_index: usize,
) -> Result<MemoryGranuleKey> {
    Ok(MemoryGranuleKey {
        object: memory_object_key(owner_instance, memory_index),
        granule_index: u64::try_from(granule_index)
            .context("tmemory granule index does not fit u64")?,
    })
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
    fn mock_transaction_store_commits_to_memory() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (func (export "write")
                (ttry)
                (i32.tstore (i32.const 0) (i32.const 42)))
              (func (export "read") (result i32)
                (i32.load (i32.const 0))))
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
    fn mock_transaction_store_without_explicit_ttry_commits_on_return() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (func (export "write")
                (i32.tstore (i32.const 0) (i32.const 42)))
              (func (export "read") (result i32)
                (i32.load (i32.const 0))))
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
    fn mock_transaction_memory_size_returns_backing_memory_pages() {
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

        assert_eq!(size.call(&mut store, ()).unwrap(), 2);
    }

    #[test]
    fn mock_transaction_zero_memory_size_and_grow_do_not_trap() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 0)
              (func (export "size") (result i32)
                (tmemory.size))
              (func (export "grow") (param i32) (result i32)
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
              (func (export "trap")
                (ttry)
                (i32.tstore (i32.const 0) (i32.const 42))
                (drop (i32.tload (i32.const 65536))))
              (func (export "write")
                (ttry)
                (i32.tstore (i32.const 0) (i32.const 7)))
              (func (export "read") (result i32)
                (i32.load (i32.const 0))))
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
              (func (export "trap_after_tload")
                (drop (i32.tload (i32.const 0)))
                (call_indirect (type $sig) (i32.const 0)))
              (func (export "recover") (result i32)
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

        let error = trap_after_tload.call(&mut store, ()).unwrap_err();
        let error = format!("{error:?}");
        assert!(error.contains("table access"));

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
              (func (export "write")
                (ttry)
                (i32.tstore $tx (i32.const 0) (i32.const 55)))
              (func (export "read_tx") (result i32)
                (i32.load $tx (i32.const 0))))
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
              (func (export "write")
                (ttry)
                (i32.tstore $tx (i32.const 0) (i32.const 66)))
              (func (export "read_tx") (result i32)
                (i32.load $tx (i32.const 0))))
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
              (func (export "write_both")
                (ttry)
                (i32.tstore $imported (i32.const 0) (i32.const 11))
                (i32.tstore $local (i32.const 0) (i32.const 22)))
              (func (export "read_imported") (result i32)
                (i32.load $imported (i32.const 0)))
              (func (export "read_local") (result i32)
                (i32.load $local (i32.const 0))))
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
