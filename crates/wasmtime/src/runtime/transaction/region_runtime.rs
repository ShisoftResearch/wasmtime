use crate::prelude::*;
use alloc::collections::{BTreeMap, BTreeSet};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::TryLockError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::thread::ThreadId;

use super::concurrency::TransactionConflictAction;
#[cfg(all(
    feature = "transaction-mvcc",
    feature = "transaction-cc-optimistic-validation"
))]
use super::concurrency::{MvccCertificationAuthority, MvccCertificationPermit};
#[cfg(all(
    feature = "transaction-mvcc",
    feature = "transaction-cc-optimistic-validation"
))]
use super::mvcc::MvccRuntime;
use super::object_value::object_kind_from_u16;
use super::state::PersistentRootDelta;
use super::type_layout::TypeLayoutRegistry;
#[cfg(all(
    feature = "transaction-mvcc",
    feature = "transaction-cc-optimistic-validation"
))]
use super::visibility::CommitTimestamp;
use super::visibility::SelectedTransactionVisibility;
use super::{
    ConcurrencyControlState, GranuleId, ObjectId, ObjectKind, ObjectTable,
    PersistentObjectDirectoryEntry, PersistentObjectRecordLocation, PersistentObjectRecordSource,
    PersistentRootKey, TMemoryFileBacking, TransactionConfig, TransactionId, TxDurableLog,
    granule_uses_transaction_state_version, persistent_root_key_from_logical_id,
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
pub(crate) struct TransactionRegionRuntime(Arc<TransactionRegionRuntimeInner>);

#[derive(Debug)]
pub(crate) struct UserTransactionRegionPermit {
    runtime: TransactionRegionRuntime,
}

#[derive(Debug)]
pub(crate) struct PersistentGcRegionPermit {
    runtime: TransactionRegionRuntime,
}

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
            TMemoryFileBacking::Regions(regions) => {
                TransactionConfig::with_file_backed_tmemory_regions(regions.clone())
            }
            TMemoryFileBacking::ExistingRegions(regions) => {
                TransactionConfig::with_file_backed_tmemory_existing_regions(regions.clone())
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
struct TransactionRegionRuntimeInner {
    next_transaction_id: AtomicU64,
    #[cfg(test)]
    fail_allocate_transaction_id_once_for_test: std::sync::atomic::AtomicBool,
    visibility: SelectedTransactionVisibility,
    log_segments: Mutex<DurableLogSegmentRegistry>,
    lock_authority: Mutex<LockAuthorityState>,
    persistent_metadata: Mutex<PersistentMetadataState>,
    object_directory: Mutex<PersistentObjectDirectoryState>,
    file_backed_storage: Mutex<Option<SharedFileBackedStorageConfig>>,
    gc_state: Mutex<GcCoordinationState>,
}

#[derive(Debug)]
struct DurableLogSegmentRegistry {
    next_log_segment_stream_id: u32,
    thread_log_segments: HashMap<ThreadId, DurableLogSegment>,
    free_log_segment_stream_ids: Vec<u32>,
}

#[derive(Debug, Default)]
struct LockAuthorityState {
    concurrency: ConcurrencyControlState,
    conflict_aborted_transactions: BTreeSet<TransactionId>,
    terminal_commits: BTreeSet<TransactionId>,
    granule_versions: BTreeMap<GranuleId, u64>,
    #[cfg(test)]
    fail_release_transaction_once_for_test: bool,
    #[cfg(test)]
    fail_commit_transaction_once_for_test: bool,
}

#[derive(Debug, Default)]
struct PersistentMetadataState {
    next_object_index: u64,
    persistent_roots: BTreeMap<PersistentRootKey, BTreeSet<ObjectId>>,
    persistent_root_versions: BTreeMap<PersistentRootKey, u32>,
    object_versions: BTreeMap<ObjectId, u32>,
    #[cfg(test)]
    fail_apply_persistent_root_delta_once_for_test: bool,
}

#[derive(Debug, Default)]
struct PersistentObjectDirectoryState {
    entries: BTreeMap<ObjectId, PersistentObjectDirectoryEntry>,
    reservations: BTreeMap<ObjectId, PersistentObjectReservation>,
    next_directory_version: u64,
    type_layouts: TypeLayoutRegistry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PersistentObjectReservation {
    kind: ObjectKind,
    type_layout_id: u32,
}

#[derive(Debug, Default)]
struct GcCoordinationState {
    active_user_commits: u32,
    gc_active: bool,
    persistent_gc_epoch: u64,
}

impl Default for DurableLogSegmentRegistry {
    fn default() -> Self {
        Self {
            next_log_segment_stream_id: FIRST_DURABLE_LOG_SEGMENT_STREAM_ID,
            thread_log_segments: HashMap::new(),
            free_log_segment_stream_ids: Vec::new(),
        }
    }
}

impl Default for TransactionRegionRuntimeInner {
    fn default() -> Self {
        Self {
            next_transaction_id: AtomicU64::new(10_001),
            #[cfg(test)]
            fail_allocate_transaction_id_once_for_test: std::sync::atomic::AtomicBool::new(false),
            visibility: SelectedTransactionVisibility::default(),
            log_segments: Mutex::new(DurableLogSegmentRegistry::default()),
            lock_authority: Mutex::new(LockAuthorityState::default()),
            persistent_metadata: Mutex::new(PersistentMetadataState::default()),
            object_directory: Mutex::new(PersistentObjectDirectoryState::default()),
            file_backed_storage: Mutex::new(None),
            gc_state: Mutex::new(GcCoordinationState::default()),
        }
    }
}

impl Default for TransactionRegionRuntime {
    fn default() -> Self {
        Self(Arc::new(TransactionRegionRuntimeInner::default()))
    }
}

fn ensure_matching_persistent_object_metadata(
    object_id: ObjectId,
    kind: ObjectKind,
    type_layout_id: u32,
    expected_kind: ObjectKind,
    expected_type_layout_id: u32,
    context: &str,
) -> Result<()> {
    ensure!(
        kind == expected_kind && type_layout_id == expected_type_layout_id,
        "{context}: object {object_id:?} expected kind {expected_kind:?} type_layout_id {expected_type_layout_id}, got kind {kind:?} type_layout_id {type_layout_id}",
    );
    Ok(())
}

impl TransactionRegionRuntime {
    pub(crate) fn visibility(&self) -> SelectedTransactionVisibility {
        self.0.visibility.clone()
    }

    fn lock_log_segments(&self) -> Result<MutexGuard<'_, DurableLogSegmentRegistry>> {
        self.0
            .log_segments
            .lock()
            .map_err(|_| crate::format_err!("transaction log segment registry lock poisoned"))
    }

    fn lock_authority(&self) -> Result<MutexGuard<'_, LockAuthorityState>> {
        self.0
            .lock_authority
            .lock()
            .map_err(|_| crate::format_err!("transaction lock authority lock poisoned"))
    }

    fn lock_persistent_metadata(&self) -> Result<MutexGuard<'_, PersistentMetadataState>> {
        self.0
            .persistent_metadata
            .lock()
            .map_err(|_| crate::format_err!("transaction root metadata lock poisoned"))
    }

    fn lock_object_directory(&self) -> Result<MutexGuard<'_, PersistentObjectDirectoryState>> {
        self.0
            .object_directory
            .lock()
            .map_err(|_| crate::format_err!("persistent object directory lock poisoned"))
    }

    fn lock_file_backed_storage(
        &self,
    ) -> Result<MutexGuard<'_, Option<SharedFileBackedStorageConfig>>> {
        self.0
            .file_backed_storage
            .lock()
            .map_err(|_| crate::format_err!("shared file-backed storage config lock poisoned"))
    }

    fn lock_gc_state(&self) -> Result<MutexGuard<'_, GcCoordinationState>> {
        self.0
            .gc_state
            .lock()
            .map_err(|_| crate::format_err!("transaction GC coordination lock poisoned"))
    }

    pub(crate) fn allocate_transaction_id(&self) -> Result<TransactionId> {
        #[cfg(test)]
        if self
            .0
            .fail_allocate_transaction_id_once_for_test
            .swap(false, Ordering::Relaxed)
        {
            bail!("injected transaction id allocation failure");
        }
        let id = self
            .0
            .next_transaction_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| crate::format_err!("transaction id overflow"))?;
        Ok(TransactionId::from_raw(id))
    }

    pub(crate) fn allocate_persistent_object_id_at_least(
        &self,
        min_object_index: u64,
    ) -> Result<ObjectId> {
        let mut runtime = self.lock_persistent_metadata()?;
        runtime.next_object_index = runtime.next_object_index.max(min_object_index);
        let object_id = ObjectId {
            object_index: runtime.next_object_index,
        };
        runtime.next_object_index = runtime
            .next_object_index
            .checked_add(1)
            .context("persistent object id overflow")?;
        Ok(object_id)
    }

    pub(crate) fn reserve_persistent_object_metadata(
        &self,
        object_id: ObjectId,
        kind: ObjectKind,
        type_layout_id: u32,
    ) -> Result<()> {
        let mut directory = self.lock_object_directory()?;
        if let Some(entry) = directory.entries.get(&object_id) {
            ensure_matching_persistent_object_metadata(
                object_id,
                kind,
                type_layout_id,
                entry.kind,
                entry.type_layout_id,
                "persistent object reservation conflicts with committed directory entry",
            )?;
            return Ok(());
        }
        if let Some(reservation) = directory.reservations.get(&object_id).copied() {
            ensure_matching_persistent_object_metadata(
                object_id,
                kind,
                type_layout_id,
                reservation.kind,
                reservation.type_layout_id,
                "persistent object reservation conflicts with existing reservation",
            )?;
            return Ok(());
        }
        directory.reservations.insert(
            object_id,
            PersistentObjectReservation {
                kind,
                type_layout_id,
            },
        );
        Ok(())
    }

    pub(crate) fn install_persistent_object_directory_entries<I>(
        &self,
        entries: I,
    ) -> Result<Vec<PersistentObjectDirectoryEntry>>
    where
        I: IntoIterator<Item = PersistentObjectDirectoryEntry>,
    {
        let mut bumped_epoch = false;
        let installed = {
            let mut metadata = self.lock_persistent_metadata()?;
            let mut directory = self.lock_object_directory()?;
            let mut installed = Vec::new();
            for mut entry in entries {
                let current = directory.entries.get(&entry.object_id).cloned();
                if let Some(current) = current.as_ref() {
                    ensure_matching_persistent_object_metadata(
                        entry.object_id,
                        entry.kind,
                        entry.type_layout_id,
                        current.kind,
                        current.type_layout_id,
                        "persistent object directory entry conflicts with committed directory metadata",
                    )?;
                }
                if let Some(reservation) = directory.reservations.get(&entry.object_id).copied() {
                    ensure_matching_persistent_object_metadata(
                        entry.object_id,
                        entry.kind,
                        entry.type_layout_id,
                        reservation.kind,
                        reservation.type_layout_id,
                        "persistent object directory entry conflicts with reserved metadata",
                    )?;
                }

                match current {
                    Some(current) if current.record_version > entry.record_version => {
                        metadata
                            .object_versions
                            .entry(current.object_id)
                            .and_modify(|version| *version = (*version).max(current.record_version))
                            .or_insert(current.record_version);
                        directory.reservations.remove(&current.object_id);
                        continue;
                    }
                    Some(current) if current.record_version == entry.record_version => {
                        metadata
                            .object_versions
                            .entry(current.object_id)
                            .and_modify(|version| *version = (*version).max(current.record_version))
                            .or_insert(current.record_version);
                        directory.reservations.remove(&current.object_id);
                        installed.push(current.clone());
                        continue;
                    }
                    _ => {}
                }

                let next_version = directory
                    .next_directory_version
                    .checked_add(1)
                    .context("persistent object directory version overflow")?;
                directory.next_directory_version = next_version;
                entry.directory_version = next_version;

                directory.entries.insert(entry.object_id, entry.clone());
                directory.reservations.remove(&entry.object_id);
                metadata
                    .object_versions
                    .entry(entry.object_id)
                    .and_modify(|version| *version = (*version).max(entry.record_version))
                    .or_insert(entry.record_version);
                installed.push(entry);
                bumped_epoch = true;
            }
            installed
        };
        if bumped_epoch {
            self.bump_persistent_gc_epoch()?;
        }
        Ok(installed)
    }

    pub(crate) fn install_recovered_persistent_object_directory_entries(
        &self,
        winners: &[crate::runtime::vm::RecoveredObjectWinner],
        mapped_source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
    ) -> Result<()> {
        let entries = winners
            .iter()
            .map(|winner| {
                Ok(PersistentObjectDirectoryEntry {
                    object_id: ObjectId {
                        object_index: winner.object_id,
                    },
                    kind: object_kind_from_u16(winner.kind)?,
                    directory_version: 0,
                    record_version: winner.version,
                    type_layout_id: winner.type_layout_id,
                    runtime_type_index: None,
                    record_source: Some(PersistentObjectRecordSource {
                        mapped_source: mapped_source.clone(),
                        location: PersistentObjectRecordLocation {
                            data_block: winner.data_block,
                            data_offset: winner.data_offset,
                            data_record_offset: u64::try_from(winner.data_record_offset)
                                .context("recovered object data record offset overflow")?,
                            record_len: winner.record_len,
                        },
                    }),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.install_persistent_object_directory_entries(entries)
            .map(|_| ())
    }

    pub(crate) fn install_recovered_type_layouts(
        &self,
        recovered_type_layouts: &TypeLayoutRegistry,
    ) -> Result<()> {
        let mut directory = self.lock_object_directory()?;
        for layout in recovered_type_layouts.iter().cloned() {
            directory.type_layouts.insert(layout)?;
        }
        Ok(())
    }

    pub(crate) fn recovered_type_layouts(&self) -> Result<TypeLayoutRegistry> {
        Ok(self.lock_object_directory()?.type_layouts.clone())
    }

    pub(crate) fn persistent_object_directory_entry(
        &self,
        object_id: ObjectId,
    ) -> Result<Option<PersistentObjectDirectoryEntry>> {
        Ok(self
            .lock_object_directory()?
            .entries
            .get(&object_id)
            .cloned())
    }

    pub(crate) fn persistent_object_directory_version(&self, object_id: ObjectId) -> Result<u64> {
        Ok(self
            .lock_object_directory()?
            .entries
            .get(&object_id)
            .map(|entry| entry.directory_version)
            .unwrap_or(0))
    }

    fn bump_persistent_gc_epoch(&self) -> Result<u64> {
        let mut runtime = self.lock_gc_state()?;
        runtime.persistent_gc_epoch = runtime
            .persistent_gc_epoch
            .checked_add(1)
            .context("persistent GC epoch overflow")?;
        Ok(runtime.persistent_gc_epoch)
    }

    pub(crate) fn persistent_gc_epoch(&self) -> Result<u64> {
        Ok(self.lock_gc_state()?.persistent_gc_epoch)
    }

    pub(crate) fn begin_user_transaction_region(&self) -> Result<UserTransactionRegionPermit> {
        let mut inner = self.lock_gc_state()?;
        ensure!(!inner.gc_active, "persistent GC is active");
        inner.active_user_commits = inner
            .active_user_commits
            .checked_add(1)
            .context("active transaction commit count overflow")?;
        Ok(UserTransactionRegionPermit {
            runtime: self.clone(),
        })
    }

    pub(crate) fn begin_persistent_gc(&self) -> Result<PersistentGcRegionPermit> {
        let mut inner = self.lock_gc_state()?;
        ensure!(!inner.gc_active, "persistent GC is already active");
        ensure!(
            inner.active_user_commits == 0,
            "user transaction commit is active"
        );
        inner.gc_active = true;
        Ok(PersistentGcRegionPermit {
            runtime: self.clone(),
        })
    }

    pub(crate) fn shared_file_backed_storage(
        &self,
    ) -> Result<Option<SharedFileBackedStorageConfig>> {
        Ok(self.lock_file_backed_storage()?.clone())
    }

    pub(crate) fn shared_file_backed_storage_for_store_adoption(
        &self,
    ) -> Result<Option<SharedFileBackedStorageConfig>> {
        let mut runtime = self.lock_file_backed_storage()?;
        if let Some(shared) = runtime.as_mut() {
            // Fresh-create runtimes hand the first store a create/truncate path
            // until tmemory is materialized. Once the backing file exists, later
            // stores must reopen it as existing storage so size/capacity come
            // from disk rather than reusing fresh-create semantics.
            shared.reopen_tmemory_for_store_adoption();
        }
        Ok(runtime.clone())
    }

    pub(crate) fn shared_file_backed_tmemory_commit_lock(&self) -> Result<Option<Arc<RwLock<()>>>> {
        Ok(self
            .lock_file_backed_storage()?
            .as_ref()
            .map(SharedFileBackedStorageConfig::tmemory_commit_lock))
    }

    pub(crate) fn shared_file_backed_tmemory_pages(&self) -> Result<Option<u64>> {
        Ok(self
            .lock_file_backed_storage()?
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
        let mut runtime = self.lock_file_backed_storage()?;
        let existing = runtime.clone();
        *runtime = Some(
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
        let mut runtime = self.lock_file_backed_storage()?;
        let existing = runtime.clone();
        let shared = SharedFileBackedStorageConfig::new(
            TMemoryFileBacking::ExistingPath(tmemory_path.to_path_buf()),
            tx_log_path.to_path_buf(),
            tx_log_blocks,
        )
        .with_reused_locks_from(existing.as_ref());
        *runtime = Some(shared);
        Ok(())
    }

    pub(crate) fn record_file_backed_tmemory_pages(&self, tmemory_pages: u64) -> Result<()> {
        let mut runtime = self.lock_file_backed_storage()?;
        if let Some(shared) = runtime.as_mut() {
            shared.record_tmemory_pages(tmemory_pages);
        }
        Ok(())
    }

    pub(crate) fn current_thread_log_segment(&self) -> Result<DurableLogSegment> {
        let thread_id = std::thread::current().id();
        let mut runtime = self.lock_log_segments()?;
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
        let mut runtime = self.lock_log_segments()?;
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
    ) -> Result<TransactionConflictAction> {
        let mut runtime = self.lock_authority()?;
        Self::check_terminal_owner_conflict(&runtime, transaction, granule, false)?;
        let action =
            runtime
                .concurrency
                .acquire_granule_read(transaction, granule, current_version)?;
        if let Some(aborted) = action.aborted_transaction() {
            runtime.conflict_aborted_transactions.insert(aborted);
        }
        Ok(action)
    }

    pub(crate) fn acquire_granule_write(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction> {
        let mut runtime = self.lock_authority()?;
        Self::check_terminal_owner_conflict(&runtime, transaction, granule, true)?;
        let action =
            runtime
                .concurrency
                .acquire_granule_write(transaction, granule, current_version)?;
        if let Some(aborted) = action.aborted_transaction() {
            runtime.conflict_aborted_transactions.insert(aborted);
        }
        Ok(action)
    }

    pub(crate) fn validate_granule_read(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.lock_authority()?
            .concurrency
            .validate_read(transaction, granule, current_version)
    }

    pub(crate) fn validate_granule_write(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.lock_authority()?
            .concurrency
            .validate_write(transaction, granule, current_version)
    }

    pub(crate) fn refresh_read_version(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.lock_authority()?.concurrency.refresh_read_version(
            transaction,
            granule,
            current_version,
        );
        Ok(())
    }

    pub(crate) fn release_transaction(&self, transaction: TransactionId) -> Result<()> {
        let mut runtime = self.lock_authority()?;
        runtime.concurrency.release_transaction(transaction);
        runtime.conflict_aborted_transactions.remove(&transaction);
        runtime.terminal_commits.remove(&transaction);
        #[cfg(test)]
        if core::mem::take(&mut runtime.fail_release_transaction_once_for_test) {
            bail!("injected release transaction failure");
        }
        Ok(())
    }

    pub(crate) fn commit_transaction_result(&self, transaction: TransactionId) -> Result<()> {
        let mut runtime = self.lock_authority()?;
        #[cfg(test)]
        if core::mem::take(&mut runtime.fail_commit_transaction_once_for_test) {
            bail!("injected commit transaction failure");
        }
        runtime.concurrency.commit_transaction_result(transaction)
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    pub(crate) fn acquire_mvcc_certification(
        &self,
        transaction: TransactionId,
        reads: &BTreeSet<GranuleId>,
        writes: &BTreeSet<GranuleId>,
        snapshot: CommitTimestamp,
        visibility: &MvccRuntime,
    ) -> Result<MvccCertificationPermit> {
        let authority = self.mvcc_certification_authority()?;
        let permit = authority.acquire(transaction, reads, writes)?;
        for granule in reads.union(writes).copied() {
            ensure!(
                visibility.latest_committed_timestamp(granule)? <= snapshot,
                "transaction MVCC certification conflict on {granule:?}"
            );
        }
        Ok(permit)
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    fn mvcc_certification_authority(&self) -> Result<Arc<MvccCertificationAuthority>> {
        Ok(self
            .lock_authority()?
            .concurrency
            .mvcc_certification_authority())
    }

    pub(crate) fn take_conflict_aborted_transaction(
        &self,
        transaction: TransactionId,
    ) -> Result<bool> {
        Ok(self
            .lock_authority()?
            .conflict_aborted_transactions
            .remove(&transaction))
    }

    pub(crate) fn begin_terminal_commit(&self, transaction: TransactionId) -> Result<()> {
        let mut runtime = self.lock_authority()?;
        ensure!(
            !runtime.conflict_aborted_transactions.remove(&transaction),
            "transaction was conflict-aborted by another transaction"
        );
        runtime.terminal_commits.insert(transaction);
        Ok(())
    }

    pub(crate) fn end_terminal_commit(&self, transaction: TransactionId) -> Result<()> {
        self.lock_authority()?.terminal_commits.remove(&transaction);
        Ok(())
    }

    pub(crate) fn versioned_granule_version(&self, granule: GranuleId) -> Result<u64> {
        Ok(self
            .lock_authority()?
            .granule_versions
            .get(&granule)
            .copied()
            .unwrap_or(0))
    }

    pub(crate) fn bump_versioned_granules<I>(&self, granules: I) -> Result<()>
    where
        I: IntoIterator<Item = GranuleId>,
    {
        let mut runtime = self.lock_authority()?;
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
        let mut runtime = self.lock_persistent_metadata()?;
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

    pub(super) fn reserve_persistent_object_record_versions<I>(
        &self,
        object_ids: I,
    ) -> Result<BTreeMap<ObjectId, u32>>
    where
        I: IntoIterator<Item = ObjectId>,
    {
        let mut runtime = self.lock_persistent_metadata()?;
        let mut reserved = BTreeMap::new();
        for object_id in object_ids.into_iter().collect::<BTreeSet<_>>() {
            let version = runtime.object_versions.entry(object_id).or_insert(0);
            *version = (*version)
                .checked_add(1)
                .context("persistent object publication version overflow")?;
            reserved.insert(object_id, *version);
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

        let applied = {
            let mut runtime = self.lock_persistent_metadata()?;
            #[cfg(test)]
            if core::mem::take(&mut runtime.fail_apply_persistent_root_delta_once_for_test) {
                bail!("injected shared persistent root apply failure");
            }
            let mut applied = false;
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
                applied = true;
            }
            applied
        };
        if applied {
            self.bump_persistent_gc_epoch()?;
        }
        Ok(())
    }

    pub(super) fn persistent_root_ids(&self) -> Result<BTreeSet<ObjectId>> {
        Ok(self
            .lock_persistent_metadata()?
            .persistent_roots
            .values()
            .flat_map(|roots| roots.iter().copied())
            .collect())
    }

    pub(super) fn persistent_root_set(
        &self,
        key: PersistentRootKey,
    ) -> Result<Option<BTreeSet<ObjectId>>> {
        Ok(self
            .lock_persistent_metadata()?
            .persistent_roots
            .get(&key)
            .cloned())
    }

    pub(super) fn persistent_root_version(&self, key: PersistentRootKey) -> Result<Option<u32>> {
        Ok(self
            .lock_persistent_metadata()?
            .persistent_root_versions
            .get(&key)
            .copied())
    }

    pub(super) fn persistent_object_version(&self, object: ObjectId) -> Result<Option<u32>> {
        Ok(self
            .lock_persistent_metadata()?
            .object_versions
            .get(&object)
            .copied())
    }

    pub(super) fn install_recovered_persistent_roots<I>(&self, roots: I) -> Result<()>
    where
        I: IntoIterator<Item = ObjectId>,
    {
        let roots = roots.into_iter().collect::<BTreeSet<_>>();
        let mut runtime = self.lock_persistent_metadata()?;
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
        let mut runtime = self.lock_persistent_metadata()?;
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

    pub(crate) fn observe_recovered_object_ids<I>(&self, object_ids: I) -> Result<()>
    where
        I: IntoIterator<Item = ObjectId>,
    {
        let mut runtime = self.lock_persistent_metadata()?;
        for object_id in object_ids {
            let next = object_id
                .object_index
                .checked_add(1)
                .context("persistent object id overflow")?;
            runtime.next_object_index = runtime.next_object_index.max(next);
        }
        Ok(())
    }

    fn check_terminal_owner_conflict(
        runtime: &LockAuthorityState,
        transaction: TransactionId,
        granule: GranuleId,
        is_write: bool,
    ) -> Result<()> {
        let Some(owner) = runtime.concurrency.owner_for_granule(granule) else {
            return Ok(());
        };
        if owner == transaction || transaction > owner || !runtime.terminal_commits.contains(&owner)
        {
            return Ok(());
        }
        if is_write {
            bail!("transaction write conflict: granule is owned by another transaction")
        } else {
            bail!("transaction read conflict: granule is owned by another transaction")
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test() -> Self {
        Self::default()
    }

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
        let recovered =
            crate::runtime::vm::block_region::reopen_and_recover_file_backed_region_for_runtime(
                tx_log_path,
            )?;
        let object_winners = recovered.committed_object_winners()?;
        let mapped_source = recovered
            .cloned_mapped_region_source()
            .context("recovered file-backed region does not expose a mapped region source")?;
        let report = ObjectTable::persistent_recovery_gc_report_mapped(
            &recovered.type_layouts,
            &object_winners,
            &recovered.root_object_ids,
            mapped_source.clone(),
        )?;
        let reachable_winners = object_winners
            .iter()
            .filter(|winner| {
                report.mark.reachable.contains(&ObjectId {
                    object_index: winner.object_id,
                })
            })
            .cloned()
            .collect::<Vec<_>>();
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
        if let Some(tmemory_pages) = recovered.committed_file_backed_tmemory_pages()? {
            runtime.record_file_backed_tmemory_pages(tmemory_pages)?;
        }
        runtime.install_recovered_type_layouts(&recovered.type_layouts)?;
        runtime.install_recovered_persistent_object_directory_entries(
            &reachable_winners,
            mapped_source,
        )?;
        runtime.observe_recovered_object_ids(object_winners.iter().map(|winner| ObjectId {
            object_index: winner.object_id,
        }))?;
        Ok(runtime)
    }

    #[cfg(test)]
    pub(crate) fn allocate_transaction_id_for_test(&self) -> Result<TransactionId> {
        self.allocate_transaction_id()
    }

    #[cfg(test)]
    pub(crate) fn next_transaction_id_for_test(&self) -> u64 {
        self.0.next_transaction_id.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn fail_allocate_transaction_id_once_for_test(&self) {
        self.0
            .fail_allocate_transaction_id_once_for_test
            .store(true, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn visibility_for_test(&self) -> SelectedTransactionVisibility {
        self.visibility()
    }

    #[cfg(test)]
    pub(crate) fn begin_user_transaction_region_for_test(
        &self,
    ) -> Result<UserTransactionRegionPermit> {
        self.begin_user_transaction_region()
    }

    #[cfg(test)]
    pub(crate) fn begin_persistent_gc_for_test(&self) -> Result<PersistentGcRegionPermit> {
        self.begin_persistent_gc()
    }

    #[cfg(test)]
    pub(crate) fn acquire_granule_read_for_test(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction> {
        self.acquire_granule_read(transaction, granule, current_version)
    }

    #[cfg(test)]
    pub(crate) fn acquire_granule_write_for_test(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction> {
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
        Ok(self
            .lock_authority()?
            .terminal_commits
            .contains(&transaction))
    }

    pub(crate) fn is_shared_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    #[cfg(test)]
    pub(crate) fn ptr_eq_for_test(&self, other: &Self) -> bool {
        self.is_shared_with(other)
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
        Ok(self.lock_log_segments()?.thread_log_segments.len())
    }

    #[cfg(test)]
    pub(crate) fn free_log_segment_count_for_test(&self) -> Result<usize> {
        Ok(self.lock_log_segments()?.free_log_segment_stream_ids.len())
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
        self.lock_authority()
            .unwrap()
            .fail_release_transaction_once_for_test = true;
    }

    #[cfg(test)]
    pub(crate) fn fail_commit_transaction_once_for_test(&self) {
        self.lock_authority()
            .unwrap()
            .fail_commit_transaction_once_for_test = true;
    }

    #[cfg(test)]
    pub(crate) fn fail_apply_persistent_root_delta_once_for_test(&self) {
        self.lock_persistent_metadata()
            .unwrap()
            .fail_apply_persistent_root_delta_once_for_test = true;
    }

    #[cfg(test)]
    pub(crate) fn with_root_object_metadata_lock_for_test<T>(
        &self,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let _guard = self.lock_persistent_metadata()?;
        f()
    }

    #[cfg(test)]
    pub(crate) fn try_acquire_granule_write_for_test(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction> {
        let mut runtime = match self.0.lock_authority.try_lock() {
            Ok(runtime) => runtime,
            Err(TryLockError::WouldBlock) => {
                bail!("transaction lock authority lock was unexpectedly held")
            }
            Err(TryLockError::Poisoned(_)) => {
                bail!("transaction lock authority lock poisoned")
            }
        };
        Self::check_terminal_owner_conflict(&runtime, transaction, granule, true)?;
        let action =
            runtime
                .concurrency
                .acquire_granule_write(transaction, granule, current_version)?;
        if let Some(aborted) = action.aborted_transaction() {
            runtime.conflict_aborted_transactions.insert(aborted);
        }
        Ok(action)
    }

    #[cfg(test)]
    pub(crate) fn poison_lock_authority_for_test(&self) {
        let runtime = self.clone();
        let _ = std::thread::spawn(move || {
            let _guard = runtime.0.lock_authority.lock().unwrap();
            panic!("poison transaction lock authority");
        })
        .join();
    }
}

impl Drop for UserTransactionRegionPermit {
    fn drop(&mut self) {
        let Ok(mut inner) = self.runtime.0.gc_state.lock() else {
            return;
        };
        if inner.active_user_commits > 0 {
            inner.active_user_commits -= 1;
        }
    }
}

impl Drop for PersistentGcRegionPermit {
    fn drop(&mut self) {
        let Ok(mut inner) = self.runtime.0.gc_state.lock() else {
            return;
        };
        inner.gc_active = false;
    }
}
