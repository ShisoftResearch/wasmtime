use crate::prelude::*;
use alloc::collections::{BTreeMap, BTreeSet};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::thread::ThreadId;

use super::concurrency::LockBasedConflictKind;
use super::state::PersistentRootDelta;
use super::{
    GranuleId, LockBased, ObjectId, PersistentRootKey, TMemoryFileBacking, TransactionConfig,
    TransactionId, TxDurableLog, granule_uses_transaction_state_version,
    persistent_root_key_from_logical_id,
};

const FIRST_DURABLE_LOG_SEGMENT_STREAM_ID: u32 = 0x2000_0000;
const MAX_DURABLE_LOG_SEGMENT_STREAM_ID: u32 = 0x3fff_ffff;
const TMEMORY_WASM_PAGE_SIZE: u64 = 64 * 1024;

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

#[derive(Clone, Debug)]
pub(crate) struct SharedFileBackedStorageConfig {
    tmemory_file_backing: TMemoryFileBacking,
    tx_log_path: PathBuf,
    tx_log_blocks: u32,
    tmemory_pages: Option<u64>,
    // Coordinates file-backed durable-log region allocator metadata across
    // independent mappings. Per-stream publication still runs store-local.
    durable_log_allocator_lock: Arc<Mutex<()>>,
    tmemory_commit_lock: Arc<RwLock<()>>,
}

impl SharedFileBackedStorageConfig {
    fn new(
        tmemory_file_backing: TMemoryFileBacking,
        tx_log_path: PathBuf,
        tx_log_blocks: u32,
    ) -> Self {
        Self {
            tmemory_file_backing,
            tx_log_path,
            tx_log_blocks,
            tmemory_pages: None,
            durable_log_allocator_lock: Arc::new(Mutex::new(())),
            tmemory_commit_lock: Arc::new(RwLock::new(())),
        }
    }

    pub(crate) fn transaction_config(&self) -> Result<TransactionConfig> {
        match &self.tmemory_file_backing {
            TMemoryFileBacking::Temp => TransactionConfig::with_file_backed_tmemory_temp(),
            TMemoryFileBacking::Path(path) => {
                TransactionConfig::with_file_backed_tmemory_path(path.clone())
            }
            TMemoryFileBacking::ExistingPath(path) => {
                TransactionConfig::with_file_backed_tmemory_existing_path(path.clone())
            }
        }
    }

    pub(crate) fn tx_log_path(&self) -> &Path {
        &self.tx_log_path
    }

    pub(crate) fn tx_log_blocks(&self) -> u32 {
        self.tx_log_blocks
    }

    pub(crate) fn tmemory_pages(&self) -> Option<u64> {
        self.tmemory_pages
    }

    pub(crate) fn durable_log_allocator_lock(&self) -> Arc<Mutex<()>> {
        self.durable_log_allocator_lock.clone()
    }

    pub(crate) fn tmemory_commit_lock(&self) -> Arc<RwLock<()>> {
        self.tmemory_commit_lock.clone()
    }

    fn with_reused_locks_from(mut self, existing: Option<&SharedFileBackedStorageConfig>) -> Self {
        if let Some(existing) = existing {
            self.durable_log_allocator_lock = existing.durable_log_allocator_lock();
            self.tmemory_commit_lock = existing.tmemory_commit_lock();
        }
        self
    }

    fn reopen_tmemory_for_store_adoption(&mut self) {
        let TMemoryFileBacking::Path(path) = &self.tmemory_file_backing else {
            return;
        };
        if path.exists() {
            self.tmemory_file_backing = TMemoryFileBacking::ExistingPath(path.clone());
        }
    }

    fn record_tmemory_pages(&mut self, tmemory_pages: u64) {
        self.tmemory_pages = Some(self.tmemory_pages.unwrap_or(0).max(tmemory_pages));
    }
}

#[derive(Debug)]
pub(crate) struct TransactionRegionRuntimeInner {
    pub(crate) next_transaction_id: u64,
    pub(crate) next_log_segment_stream_id: u32,
    pub(crate) thread_log_segments: HashMap<ThreadId, DurableLogSegment>,
    pub(crate) free_log_segment_stream_ids: Vec<u32>,
    pub(crate) locks: LockBased,
    pub(crate) conflict_aborted_transactions: BTreeSet<TransactionId>,
    pub(crate) terminal_commits: BTreeSet<TransactionId>,
    pub(crate) granule_versions: BTreeMap<GranuleId, u64>,
    persistent_roots: BTreeMap<PersistentRootKey, BTreeSet<ObjectId>>,
    persistent_root_versions: BTreeMap<PersistentRootKey, u32>,
    object_versions: BTreeMap<ObjectId, u32>,
    pub(crate) durable_log: TxDurableLog,
    pub(crate) file_backed_storage: Option<SharedFileBackedStorageConfig>,
    #[cfg(test)]
    pub(crate) fail_release_transaction_once_for_test: bool,
    #[cfg(test)]
    pub(crate) fail_apply_persistent_root_delta_once_for_test: bool,
}

impl Default for TransactionRegionRuntimeInner {
    fn default() -> Self {
        Self {
            next_transaction_id: 10_001,
            next_log_segment_stream_id: FIRST_DURABLE_LOG_SEGMENT_STREAM_ID,
            thread_log_segments: HashMap::new(),
            free_log_segment_stream_ids: Vec::new(),
            locks: LockBased::default(),
            conflict_aborted_transactions: BTreeSet::new(),
            terminal_commits: BTreeSet::new(),
            granule_versions: BTreeMap::new(),
            persistent_roots: BTreeMap::new(),
            persistent_root_versions: BTreeMap::new(),
            object_versions: BTreeMap::new(),
            durable_log: TxDurableLog::default(),
            file_backed_storage: None,
            #[cfg(test)]
            fail_release_transaction_once_for_test: false,
            #[cfg(test)]
            fail_apply_persistent_root_delta_once_for_test: false,
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

    pub(crate) fn shared_file_backed_storage(
        &self,
    ) -> Result<Option<SharedFileBackedStorageConfig>> {
        Ok(self.lock()?.file_backed_storage.clone())
    }

    pub(crate) fn shared_file_backed_storage_for_store_adoption(
        &self,
    ) -> Result<Option<SharedFileBackedStorageConfig>> {
        let mut runtime = self.lock()?;
        if let Some(shared) = runtime.file_backed_storage.as_mut() {
            // Fresh-create runtimes hand the first store a create/truncate path
            // until tmemory is materialized. Once the backing file exists, later
            // stores must reopen it as existing storage so size/capacity come
            // from disk rather than reusing fresh-create semantics.
            shared.reopen_tmemory_for_store_adoption();
        }
        Ok(runtime.file_backed_storage.clone())
    }

    pub(crate) fn shared_file_backed_tmemory_commit_lock(&self) -> Result<Option<Arc<RwLock<()>>>> {
        Ok(self
            .lock()?
            .file_backed_storage
            .as_ref()
            .map(SharedFileBackedStorageConfig::tmemory_commit_lock))
    }

    pub(crate) fn shared_file_backed_tmemory_pages(&self) -> Result<Option<u64>> {
        Ok(self
            .lock()?
            .file_backed_storage
            .as_ref()
            .and_then(SharedFileBackedStorageConfig::tmemory_pages))
    }

    pub(crate) fn with_shared_file_backed_tmemory_commit_read_lock<T>(
        &self,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        if let Some(lock) = self.shared_file_backed_tmemory_commit_lock()? {
            let _guard = lock.read().map_err(|_| {
                crate::format_err!("shared file-backed tmemory commit lock poisoned")
            })?;
            return f();
        }
        f()
    }

    pub(crate) fn with_shared_file_backed_tmemory_commit_write_lock<T>(
        &self,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        if let Some(lock) = self.shared_file_backed_tmemory_commit_lock()? {
            let _guard = lock.write().map_err(|_| {
                crate::format_err!("shared file-backed tmemory commit lock poisoned")
            })?;
            return f();
        }
        f()
    }

    pub(crate) fn record_created_file_backed_storage(
        &self,
        tmemory_path: &Path,
        tx_log_path: &Path,
        tx_log_blocks: u32,
    ) -> Result<()> {
        let mut runtime = self.lock()?;
        let existing = runtime.file_backed_storage.clone();
        runtime.file_backed_storage = Some(
            SharedFileBackedStorageConfig::new(
                TMemoryFileBacking::Path(tmemory_path.to_path_buf()),
                tx_log_path.to_path_buf(),
                tx_log_blocks,
            )
            .with_reused_locks_from(existing.as_ref()),
        );
        Ok(())
    }

    pub(crate) fn record_opened_file_backed_storage(
        &self,
        tmemory_path: &Path,
        tx_log_path: &Path,
        tx_log_blocks: u32,
    ) -> Result<()> {
        let mut runtime = self.lock()?;
        let existing = runtime.file_backed_storage.clone();
        let mut shared = SharedFileBackedStorageConfig::new(
            TMemoryFileBacking::ExistingPath(tmemory_path.to_path_buf()),
            tx_log_path.to_path_buf(),
            tx_log_blocks,
        )
        .with_reused_locks_from(existing.as_ref());
        shared.tmemory_pages = Some(file_backed_tmemory_pages_from_path(tmemory_path)?);
        runtime.file_backed_storage = Some(shared);
        Ok(())
    }

    pub(crate) fn record_file_backed_tmemory_pages(&self, tmemory_pages: u64) -> Result<()> {
        let mut runtime = self.lock()?;
        if let Some(shared) = runtime.file_backed_storage.as_mut() {
            shared.record_tmemory_pages(tmemory_pages);
        }
        Ok(())
    }

    pub(crate) fn current_thread_log_segment(&self) -> Result<DurableLogSegment> {
        let thread_id = std::thread::current().id();
        let mut runtime = self.lock()?;
        if let Some(segment) = runtime.thread_log_segments.get(&thread_id).copied() {
            return Ok(segment);
        }

        let stream_id = if let Some(stream_id) = runtime.free_log_segment_stream_ids.pop() {
            stream_id
        } else {
            ensure!(
                runtime.next_log_segment_stream_id <= MAX_DURABLE_LOG_SEGMENT_STREAM_ID,
                "durable log segment stream id overflow"
            );
            let stream_id = runtime.next_log_segment_stream_id;
            runtime.next_log_segment_stream_id = runtime
                .next_log_segment_stream_id
                .checked_add(1)
                .context("durable log segment stream id overflow")?;
            stream_id
        };
        let segment = DurableLogSegment { stream_id };
        runtime.thread_log_segments.insert(thread_id, segment);
        Ok(segment)
    }

    pub(crate) fn release_current_thread_log_segment_reusable(&self) -> Result<()> {
        self.release_current_thread_log_segment(true)
    }

    pub(crate) fn retire_current_thread_log_segment(&self) -> Result<()> {
        self.release_current_thread_log_segment(false)
    }

    fn release_current_thread_log_segment(&self, reusable: bool) -> Result<()> {
        let thread_id = std::thread::current().id();
        let mut runtime = self.lock()?;
        if let Some(segment) = runtime.thread_log_segments.remove(&thread_id) {
            if reusable {
                runtime
                    .free_log_segment_stream_ids
                    .push(segment.stream_id());
            }
        }
        Ok(())
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
        #[cfg(test)]
        if core::mem::take(&mut runtime.fail_release_transaction_once_for_test) {
            bail!("injected release transaction failure");
        }
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

    pub(super) fn reserve_persistent_root_versions<I>(
        &self,
        keys: I,
    ) -> Result<BTreeMap<PersistentRootKey, u32>>
    where
        I: IntoIterator<Item = PersistentRootKey>,
    {
        let mut runtime = self.lock()?;
        let mut reserved = BTreeMap::new();
        for key in keys.into_iter().collect::<BTreeSet<_>>() {
            let version = runtime.persistent_root_versions.entry(key).or_insert(0);
            *version = (*version)
                .checked_add(1)
                .context("persistent root publication version overflow")?;
            reserved.insert(key, *version);
        }
        Ok(reserved)
    }

    pub(super) fn apply_committed_persistent_root_delta(
        &self,
        delta: PersistentRootDelta,
        reserved_versions: BTreeMap<PersistentRootKey, u32>,
    ) -> Result<()> {
        if delta.is_empty() {
            return Ok(());
        }

        let mut runtime = self.lock()?;
        #[cfg(test)]
        if core::mem::take(&mut runtime.fail_apply_persistent_root_delta_once_for_test) {
            bail!("injected shared persistent root apply failure");
        }
        for (key, roots) in delta.roots {
            let version = reserved_versions
                .get(&key)
                .copied()
                .context("shared persistent root publication version was not reserved")?;
            match runtime.persistent_root_versions.get(&key).copied() {
                Some(current) if current > version => {
                    continue;
                }
                Some(current) => ensure!(
                    current == version,
                    "shared persistent root publication version regressed"
                ),
                None => {
                    runtime.persistent_root_versions.insert(key, version);
                }
            }
            if roots.is_empty() {
                runtime.persistent_roots.remove(&key);
            } else {
                runtime.persistent_roots.insert(key, roots);
            }
        }
        Ok(())
    }

    pub(super) fn persistent_root_ids(&self) -> Result<BTreeSet<ObjectId>> {
        Ok(self
            .lock()?
            .persistent_roots
            .values()
            .flat_map(|roots| roots.iter().copied())
            .collect())
    }

    pub(super) fn persistent_root_set(
        &self,
        key: PersistentRootKey,
    ) -> Result<Option<BTreeSet<ObjectId>>> {
        Ok(self.lock()?.persistent_roots.get(&key).cloned())
    }

    pub(super) fn persistent_root_version(&self, key: PersistentRootKey) -> Result<Option<u32>> {
        Ok(self.lock()?.persistent_root_versions.get(&key).copied())
    }

    pub(super) fn persistent_object_version(&self, object: ObjectId) -> Result<Option<u32>> {
        Ok(self.lock()?.object_versions.get(&object).copied())
    }

    pub(super) fn install_recovered_persistent_roots<I>(&self, roots: I) -> Result<()>
    where
        I: IntoIterator<Item = ObjectId>,
    {
        let roots = roots.into_iter().collect::<BTreeSet<_>>();
        let mut runtime = self.lock()?;
        if roots.is_empty() {
            runtime
                .persistent_roots
                .remove(&PersistentRootKey::Recovered);
        } else {
            runtime
                .persistent_roots
                .insert(PersistentRootKey::Recovered, roots);
        }
        Ok(())
    }

    pub(super) fn install_recovered_persistent_root_state<I, J>(
        &self,
        roots: I,
        versions: J,
    ) -> Result<()>
    where
        I: IntoIterator<Item = ObjectId>,
        J: IntoIterator<Item = (u64, u32)>,
    {
        let roots = roots.into_iter().collect::<BTreeSet<_>>();
        let mut runtime = self.lock()?;
        if roots.is_empty() {
            runtime
                .persistent_roots
                .remove(&PersistentRootKey::Recovered);
        } else {
            runtime
                .persistent_roots
                .insert(PersistentRootKey::Recovered, roots);
        }
        for (logical_id, version) in versions {
            let Some(key) = persistent_root_key_from_logical_id(logical_id)? else {
                continue;
            };
            runtime
                .persistent_root_versions
                .entry(key)
                .and_modify(|current| *current = (*current).max(version))
                .or_insert(version);
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
    pub(crate) fn create_file_backed_for_test(
        tmemory_path: &Path,
        tx_log_path: &Path,
        tx_log_blocks: u32,
    ) -> Result<Self> {
        let runtime = Self::default();
        TxDurableLog::create_file_backed(tx_log_path, tx_log_blocks)?;
        runtime.record_created_file_backed_storage(tmemory_path, tx_log_path, tx_log_blocks)?;
        Ok(runtime)
    }

    #[cfg(test)]
    pub(crate) fn open_file_backed_for_test(
        tmemory_path: &Path,
        tx_log_path: &Path,
    ) -> Result<Self> {
        let runtime = Self::default();
        TxDurableLog::open_file_backed(tx_log_path)?;
        let tx_log_blocks = u32::try_from(
            std::fs::metadata(tx_log_path)
                .with_context(|| {
                    format!("failed to stat transaction log {}", tx_log_path.display())
                })?
                .len()
                / u64::try_from(crate::runtime::vm::block_region::BLOCK_SIZE).unwrap(),
        )
        .context("transaction log block count overflow")?;
        runtime.record_opened_file_backed_storage(tmemory_path, tx_log_path, tx_log_blocks)?;
        Ok(runtime)
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

    #[cfg(test)]
    pub(crate) fn release_current_thread_log_segment_for_test(&self) -> Result<()> {
        self.release_current_thread_log_segment_reusable()
    }

    #[cfg(test)]
    pub(crate) fn thread_log_segment_count_for_test(&self) -> Result<usize> {
        Ok(self.lock()?.thread_log_segments.len())
    }

    #[cfg(test)]
    pub(crate) fn free_log_segment_count_for_test(&self) -> Result<usize> {
        Ok(self.lock()?.free_log_segment_stream_ids.len())
    }

    #[cfg(test)]
    pub(super) fn persistent_root_ids_for_test(&self) -> Result<BTreeSet<ObjectId>> {
        self.persistent_root_ids()
    }

    #[cfg(test)]
    pub(super) fn persistent_root_set_for_test(
        &self,
        key: PersistentRootKey,
    ) -> Result<Option<BTreeSet<ObjectId>>> {
        self.persistent_root_set(key)
    }

    #[cfg(test)]
    pub(super) fn persistent_root_version_for_test(
        &self,
        key: PersistentRootKey,
    ) -> Result<Option<u32>> {
        self.persistent_root_version(key)
    }

    #[cfg(test)]
    pub(super) fn persistent_object_version_for_test(
        &self,
        object: ObjectId,
    ) -> Result<Option<u32>> {
        self.persistent_object_version(object)
    }

    #[cfg(test)]
    pub(crate) fn fail_release_transaction_once_for_test(&self) {
        self.lock().unwrap().fail_release_transaction_once_for_test = true;
    }

    #[cfg(test)]
    pub(crate) fn fail_apply_persistent_root_delta_once_for_test(&self) {
        self.lock()
            .unwrap()
            .fail_apply_persistent_root_delta_once_for_test = true;
    }
}

fn file_backed_tmemory_pages_from_path(path: &Path) -> Result<u64> {
    let len = std::fs::metadata(path)
        .with_context(|| format!("failed to stat file-backed tmemory {}", path.display()))?
        .len();
    ensure!(
        len % TMEMORY_WASM_PAGE_SIZE == 0,
        "file-backed tmemory length {} is not wasm-page aligned",
        path.display()
    );
    Ok(len / TMEMORY_WASM_PAGE_SIZE)
}
